#!/usr/bin/env python3
"""Abrupt-reset durability campaign for the M0S store (Path B of the
PRD's hardware power-cut gate).

Drives a firmware built with `--features powercut-selftest`: each cycle
arms the device's RTC watchdog over HTTP (`POST /test-powercut`), starts
a mutating logical-book operation sized to straddle the deadline, waits
out the reset and the auto-started next session, then proves the catalog
converged: the operation either fully committed or fully didn't, its
request ID replays to a definitive outcome, and the book list matches
the tracked expectation exactly.

This validates the store's write ordering on a real card and real FAT
timing. It does NOT validate card-level power-loss physics — the card
keeps power through the reset — so the true bench-rig gate stays open.

Usage:
  python3 tools/powercut_campaign.py --ip 192.168.1.158 \
      --serial /dev/cu.usbmodem1301 --cycles 20

The script owns the serial port for the whole run (reset detection and
per-boot IP discovery); stop any other capture first.
"""

import argparse
import contextlib
import fcntl
import hashlib
import io
import json
import os
import random
import re
import struct
import sys
import termios
import threading
import time
import urllib.error
import urllib.request
import zipfile
from typing import NoReturn

# Refusals that mean "this request can no longer execute, and therefore
# never committed" — the definitive negative outcome of a replay.
DEFINITIVE_REFUSALS = {"stale_epoch", "epoch_exhausted"}
# Refusals that are never acceptable at any point in a campaign.
FORBIDDEN_REFUSALS = {"ambiguous_request_evidence"}


class Serial:
    """Raw read-only serial tail with line timestamps (see
    tools/serial_capture.py for the DTR/RTS reasoning)."""

    def __init__(self, port, logfile):
        self.port = port
        # Held for the life of the process, appended from the reader
        # thread; there is no scope a context manager could close it at.
        self.log = open(logfile, "ab", buffering=0)  # noqa: SIM115
        self.lines = []
        self.lock = threading.Lock()
        threading.Thread(target=self._run, daemon=True).start()

    def _attach(self):
        fd = os.open(self.port, os.O_RDONLY | os.O_NOCTTY | os.O_NONBLOCK)
        attrs = termios.tcgetattr(fd)
        attrs[0] = attrs[1] = attrs[3] = 0
        attrs[2] = termios.CREAD | termios.CLOCAL | termios.CS8
        termios.tcsetattr(fd, termios.TCSANOW, attrs)
        fcntl.ioctl(fd, termios.TIOCMBIS, struct.pack("i", termios.TIOCM_DTR))
        fcntl.ioctl(fd, termios.TIOCMBIC, struct.pack("i", termios.TIOCM_RTS))
        return fd

    def _run(self):
        buf = b""
        while True:
            try:
                fd = self._attach()
            except OSError:
                time.sleep(0.5)
                continue
            try:
                while True:
                    try:
                        data = os.read(fd, 4096)
                    except BlockingIOError:
                        time.sleep(0.03)
                        continue
                    if not data:
                        time.sleep(0.03)
                        continue
                    self.log.write(data)
                    buf += data
                    while b"\n" in buf:
                        line, buf = buf.split(b"\n", 1)
                        with self.lock:
                            self.lines.append((time.monotonic(), line.decode("utf-8", "replace")))
            except OSError:
                # Port vanishes across some resets; reattach.
                with contextlib.suppress(OSError):
                    os.close(fd)
                time.sleep(0.5)

    def mark(self):
        with self.lock:
            return len(self.lines)

    def since(self, mark):
        with self.lock:
            return [line for _, line in self.lines[mark:]]

    def wait_for(self, mark, pattern, timeout):
        """First line at/after `mark` matching regex `pattern`, or None."""
        deadline = time.monotonic() + timeout
        at = mark
        while time.monotonic() < deadline:
            with self.lock:
                chunk = self.lines[at:]
                at = len(self.lines)
            for _, line in chunk:
                m = re.search(pattern, line)
                if m:
                    return m
            time.sleep(0.2)
        return None


def make_epub(cycle, pad_bytes):
    """A classic-ZIP EPUB with unique content per cycle."""
    unique = os.urandom(16).hex()
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr(zipfile.ZipInfo("mimetype"), "application/epub+zip", zipfile.ZIP_STORED)
        z.writestr(
            "META-INF/container.xml",
            '<?xml version="1.0"?><container version="1.0" '
            'xmlns="urn:oasis:names:tc:opendocument:xmlns:container">'
            '<rootfiles><rootfile full-path="content.opf" '
            'media-type="application/oebps-package+xml"/></rootfiles></container>',
        )
        z.writestr(
            "content.opf",
            f'<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" '
            f'version="3.0" unique-identifier="id"><metadata '
            f'xmlns:dc="http://purl.org/dc/elements/1.1/">'
            f'<dc:identifier id="id">pc-{cycle}-{unique}</dc:identifier>'
            f"<dc:title>PC {cycle}</dc:title><dc:language>en</dc:language>"
            f'</metadata><manifest><item id="c1" href="ch1.xhtml" '
            f'media-type="application/xhtml+xml"/></manifest>'
            f'<spine><itemref idref="c1"/></spine></package>',
        )
        z.writestr(
            "ch1.xhtml",
            '<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml">'
            f"<head><title>PC {cycle}</title></head><body><p>{unique}</p></body></html>",
        )
        if pad_bytes:
            z.writestr(zipfile.ZipInfo("pad.bin"), os.urandom(pad_bytes), zipfile.ZIP_STORED)
    data = out.getvalue()
    return data, hashlib.sha256(data).hexdigest()


class Device:
    def __init__(self, ip):
        self.base = f"http://{ip}"

    def _request(self, method, path, data=None, headers=None, timeout=30):
        req = urllib.request.Request(
            self.base + path, data=data, headers=headers or {}, method=method
        )
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                return resp.status, resp.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as err:
            return err.code, err.read().decode("utf-8", "replace")

    def get_json(self, path, timeout=30):
        status, body = self._request("GET", path, timeout=timeout)
        return status, json.loads(body)

    def alive(self):
        try:
            status, _ = self._request("GET", "/", timeout=3)
            return status == 200
        except OSError:
            return False

    def capabilities(self):
        return self.get_json("/capabilities", timeout=45)

    def list_books(self):
        status, body = self._request("GET", "/list-books", timeout=45)
        if status != 200:
            raise AssertionError(f"list-books -> {status}: {body}")
        return json.loads(body)

    def arm(self, after_ms):
        status, body = self._request(
            "POST", f"/test-powercut?after_ms={after_ms}", data=b"", timeout=10
        )
        if status != 200 or "armed" not in body:
            raise AssertionError(f"arm failed -> {status}: {body}")

    def upload(self, request_id, label, sha_hex, body, replace=None, timeout=90):
        path = f"/upload?name={label}"
        if replace:
            path += f"&replace={replace}"
        headers = {"X-Upload-Request-Id": request_id, "X-Source-SHA256": sha_hex}
        status, text = self._request("POST", path, body, headers, timeout=timeout)
        return status, json.loads(text)

    def delete(self, request_id, token, timeout=60):
        headers = {"X-Delete-Request-Id": request_id}
        body = json.dumps({"book_token": token}).encode()
        status, text = self._request("POST", "/delete-book", body, headers, timeout)
        return status, json.loads(text)


def fresh_id(epoch):
    return f"{epoch:016x}{os.urandom(16).hex()}"


class Campaign:
    def __init__(self, args):
        self.serial = Serial(args.serial, args.serial_log)
        self.device = Device(args.ip)
        self.args = args
        self.committed = {}  # token -> {label, generation, sha, logical_id}
        self.results = []
        self.rng = random.Random()

    def fail(self, cycle, why) -> NoReturn:
        print(f"FAIL cycle {cycle}: {why}")
        print("--- last serial lines ---")
        for line in self.serial.since(max(0, self.serial.mark() - 40)):
            print(f"  {line}")
        sys.exit(1)

    def wait_serving(self, mark, timeout=120):
        m = self.serial.wait_for(mark, r"upload: serving at (\d+\.\d+\.\d+\.\d+)", timeout)
        if m:
            self.device = Device(m.group(1))
        # Serial can lag or miss; fall back to polling the known address.
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if self.device.alive():
                return
            time.sleep(1)
        raise AssertionError("device did not come back to serving")

    def check_list_invariants(self, cycle, books):
        seen_tokens = set()
        for entry in books:
            for key in ("book_token", "logical_book_id", "source_generation"):
                if key not in entry:
                    self.fail(cycle, f"malformed list entry: {entry}")
            if entry["book_token"] in seen_tokens:
                self.fail(cycle, f"duplicate token in list: {entry['book_token']}")
            seen_tokens.add(entry["book_token"])
        logicals = {}
        for entry in books:
            lid = entry["logical_book_id"]
            if lid in logicals:
                self.fail(cycle, f"logical book listed twice: {lid}")
            logicals[lid] = entry

    def expect_exact(self, cycle, books):
        got = {e["book_token"]: e for e in books}
        if set(got) != set(self.committed):
            self.fail(
                cycle,
                f"list mismatch: expected {sorted(self.committed)}, got {sorted(got)}",
            )
        for token, want in self.committed.items():
            entry = got[token]
            # The list contract carries no committed SHA; label, length,
            # and generation pin the entry to the intended operation.
            if (
                entry["source_generation"] != want["generation"]
                or entry["source_length"] != want["length"]
                or entry["display_label"] != want["label"]
            ):
                self.fail(cycle, f"entry drifted: want {want}, got {entry}")

    def converge(self, cycle, op):
        """Resolve an interrupted operation to committed-or-not, replaying
        its original request ID, and update the expectation."""
        status, reply = None, None
        try:
            if op["kind"] in ("create", "replace"):
                status, reply = self.device.upload(
                    op["id"], op["label"], op["sha"], op["body"], op.get("replace")
                )
            else:
                status, reply = self.device.delete(op["id"], op["token"])
        except OSError as err:
            # A dropped replay (e.g. server answered before reading the
            # whole body and the socket closed under us): fall back to the
            # list to decide, then re-raise only if still ambiguous.
            books = self.device.list_books()
            by_label = {e["display_label"]: e for e in books}
            if op["kind"] in ("create", "replace") and op["label"] in by_label:
                reply = {"status": "ok"}
                entry = by_label[op["label"]]
                reply.update(
                    book_token=entry["book_token"],
                    logical_book_id=entry["logical_book_id"],
                    source_generation=entry["source_generation"],
                )
                status = 200
            elif op["kind"] == "delete" and not any(e["book_token"] == op["token"] for e in books):
                status, reply = 200, {"status": "ok"}
            else:
                self.fail(cycle, f"replay dropped and list ambiguous: {err}")

        code = reply.get("code")
        if code in FORBIDDEN_REFUSALS:
            self.fail(cycle, f"forbidden refusal on replay: {reply}")
        if reply.get("status") == "ok":
            if op["kind"] in ("create", "replace"):
                if op.get("replace"):
                    self.committed.pop(op["replace"], None)
                self.committed[reply["book_token"]] = {
                    "label": op["label"],
                    "generation": reply["source_generation"],
                    "length": len(op["body"]),
                    "logical_id": reply["logical_book_id"],
                }
            else:
                self.committed.pop(op["token"], None)
            return "committed"
        if code in DEFINITIVE_REFUSALS:
            return "rolled_back"
        if code == "no_tombstone_slot" and op["kind"] == "delete":
            # Retention can legitimately pin tombstones for two epochs;
            # the book stays, later cycles retry.
            return "refused_tombstones"
        return self.fail(cycle, f"replay did not converge: HTTP {status} {reply}")

    def run(self):
        mark = self.serial.mark()
        print("waiting for device to serve...")
        self.wait_serving(mark)
        # Whatever the card already holds is the baseline — a rerun after
        # a crashed campaign (or a card with real books) must not read as
        # an inconsistency.
        books = self.device.list_books()
        self.check_list_invariants(0, books)
        for entry in books:
            self.committed[entry["book_token"]] = {
                "label": entry["display_label"],
                "generation": entry["source_generation"],
                "length": entry["source_length"],
                "logical_id": entry["logical_book_id"],
            }
        if books:
            print(f"adopted {len(books)} pre-existing book(s) as baseline")
        for cycle in range(1, self.args.cycles + 1):
            self.cycle(cycle)
        # Final convergence sweep: delete every campaign book, no cuts.
        print("cleanup: deleting campaign books")
        for token in list(self.committed):
            _, caps = self.device.capabilities()
            _status, reply = self.device.delete(fresh_id(caps["idempotency_epoch"]), token)
            if reply.get("status") == "ok":
                self.committed.pop(token, None)
        books = self.device.list_books()
        print(f"final list: {len(books)} book(s) remain (expected {len(self.committed)})")
        outcomes = {}
        for result in self.results:
            outcomes[result["outcome"]] = outcomes.get(result["outcome"], 0) + 1
        print(f"campaign complete: {len(self.results)} cycles, outcomes {outcomes}")

    def cycle(self, cycle):
        # 1. Consistent ground state.
        books = self.device.list_books()
        self.check_list_invariants(cycle, books)
        self.expect_exact(cycle, books)

        # 2. Choose and prepare this cycle's operation.
        _, caps = self.device.capabilities()
        epoch = caps["idempotency_epoch"]
        deletable = list(self.committed)
        if len(self.committed) >= 3 or (deletable and cycle % 4 == 0):
            op = {"kind": "delete", "id": fresh_id(epoch), "token": deletable[0]}
        elif deletable and cycle % 3 == 0:
            body, sha = make_epub(cycle, self.rng.randrange(20_000, 300_000))
            op = {
                "kind": "replace",
                "id": fresh_id(epoch),
                "label": f"PC{cycle:03}R",
                "sha": sha,
                "body": body,
                "replace": deletable[0],
            }
        else:
            body, sha = make_epub(cycle, self.rng.randrange(20_000, 300_000))
            op = {
                "kind": "create",
                "id": fresh_id(epoch),
                "label": f"PC{cycle:03}",
                "sha": sha,
                "body": body,
            }

        # 3. Arm, then fire the op into the deadline.
        arm_ms = self.rng.randrange(self.args.min_ms, self.args.max_ms)
        mark = self.serial.mark()
        self.device.arm(arm_ms)
        cut_mid_op = False
        try:
            if op["kind"] in ("create", "replace"):
                status, _reply = self.device.upload(
                    op["id"], op["label"], op["sha"], op["body"], op.get("replace")
                )
            else:
                status, _reply = self.device.delete(op["id"], op["token"])
            landed = f"response completed (HTTP {status})"
        except OSError:
            cut_mid_op = True
            landed = "connection died mid-operation"

        # 4. Wait out the reset and the next auto-started session. The
        # ROM's `rst:` banner names the cause, but the capture can miss
        # it while USB re-enumerates — any boot-only marker proves the
        # reset fired.
        reset = self.serial.wait_for(
            mark,
            r"rst:.*(RTC|WDT)|powercut: auto-starting",
            arm_ms / 1000 + 30,
        )
        if not reset:
            self.fail(cycle, "armed reset never fired")
        self.wait_serving(mark)

        # 5. Converge and verify.
        outcome = self.converge(cycle, op)
        books = self.device.list_books()
        self.check_list_invariants(cycle, books)
        self.expect_exact(cycle, books)

        result = {
            "cycle": cycle,
            "op": op["kind"],
            "arm_ms": arm_ms,
            "cut_mid_op": cut_mid_op,
            "landed": landed,
            "outcome": outcome,
        }
        self.results.append(result)
        print(
            f"cycle {cycle:3}: {op['kind']:7} arm={arm_ms:5}ms "
            f"{'CUT ' if cut_mid_op else 'done'} -> {outcome}"
        )
        if self.args.results:
            with open(self.args.results, "a", encoding="utf-8") as f:
                f.write(json.dumps(result) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ip", required=True)
    parser.add_argument("--serial", required=True)
    parser.add_argument("--cycles", type=int, default=20)
    parser.add_argument("--min-ms", type=int, default=200)
    parser.add_argument("--max-ms", type=int, default=6000)
    parser.add_argument("--serial-log", default="/tmp/powercut_serial.log")
    parser.add_argument("--results", default="")
    args = parser.parse_args()
    Campaign(args).run()


if __name__ == "__main__":
    main()
