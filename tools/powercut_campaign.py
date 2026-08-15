#!/usr/bin/env python3
"""Abrupt-reset durability campaign for the journalled installer (Path B of
the implementation plan's hardware power-cut gate).

Drives a firmware built with `--features powercut-selftest`: each cycle arms
the device's RTC watchdog over HTTP (`POST /test-powercut`), starts an upload
or delete sized to straddle the deadline, waits out the reset and the
auto-started next session, then proves the shelf converged — the operation
either fully committed or fully didn't, no name is left duplicated, and the
journal cleared.

This validates the installer's write ordering on a real card and real FAT
timing. It does NOT validate card-level power-loss physics — the card keeps
power through the reset — so the true bench-rig gate stays open.

Three properties of the device shape the checks:

- **`/list` is a session-start snapshot.** The catalog listing is rendered
  into the loaned buffer when a wireless session opens, so an upload does not
  change what `/list` returns until the device reboots. Every read here is
  therefore taken *after* a reboot, which each cycle performs anyway.
- **An interrupted operation has exactly two legal outcomes**, and the shelf
  must land on one of them: for an upload, the book is wholly there or wholly
  absent; for a delete, gone or still present. Anything else — a stranger
  appearing, an untargeted book vanishing, a name listed twice — is the
  failure this campaign exists to catch. A duplicated name is the specific
  signature of a half-finished move, where two directory entries share one
  cluster chain.
- **A standing journal record refuses further uploads and deletes.** So the
  next cycle's operation being accepted at all is the proof that recovery
  cleared the record; no separate probe is needed.

Books already on the card are adopted as an untouched baseline. Only books
this campaign uploaded (label prefix below) are ever deleted.

**Two windows, cut two different ways.** An upload spends almost all its
time streaming a body and a few hundred milliseconds installing what it
streamed, and only the second window has journal state in it.

- `transfer` aims the deadline from a measured throughput. It proves a
  half-streamed upload leaves no partial book on the shelf.
- `install` hands the timing to the device: `at_install_ms=N` arms the
  watchdog when the install actually starts. Aiming this window from the
  host does not work and was measured not working — over 36 cycles on an X3
  (2026-08-15) not one host-aimed cut reached it, because the install is the
  last few percent of an upload while throughput moved between 100 and
  185 kB/s between runs.

The evidence that a cut reached the journal is the device saying it replayed
a record at mount, which the selftest firmware prints. The shipping "finished
an interrupted install" line is *not* that evidence: it appears only when
recovery moves a file, and a cut after the last move but before the record is
cleared still replays a real record.

Usage:
  python3 tools/powercut_campaign.py --ip 192.168.1.158 \
      --serial /dev/cu.usbmodem1301 --cycles 20

The script owns the serial port for the whole run (reset detection and
per-boot IP discovery); stop any other capture first.
"""

import argparse
import contextlib
import fcntl
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
import unittest
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from typing import NoReturn

# Every book this campaign creates carries this label prefix, and nothing
# without it is ever deleted — the card under test is also somebody's
# library.
CAMPAIGN_PREFIX = "PCUT"

# The uncut upload that measures the write path's throughput.
CALIBRATION_LABEL = f"{CAMPAIGN_PREFIX}CAL"

# A reset fired. The ROM's `rst:` banner names the cause, but the capture can
# miss it while USB re-enumerates, so any boot-only marker counts.
RESET_PATTERN = r"rst:.*(RTC|WDT)|powercut: auto-starting"

# Mount-time recovery reporting that it had a record to replay, printed by
# the powercut-selftest firmware. The capture group says whether finishing
# the transaction required moving a file.
RECOVERY_PATTERN = r"powercut: recovery replayed a record: moved=(\w+)"


def scan(lines, pattern):
    """Every `(timestamp, text)` line matching `pattern`, as `(match, ts)`.

    Deliberately unordered with respect to any other marker. A boot emits
    mount-time recovery well before the line that announces the session, and
    the ROM's reset banner is often lost while USB re-enumerates — so the
    marker that proves a reset happened frequently arrives *after* the
    evidence of what that reset caused. Anything that discards lines older
    than that marker discards the finding.
    """
    return [(m, stamp) for stamp, line in lines if (m := re.search(pattern, line))]


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
        found = self.wait_for_at(mark, pattern, timeout)
        return found[0] if found else None

    def matches_since(self, mark, pattern):
        """Every line at/after `mark` matching `pattern`, with arrival times.

        Scanning rather than waiting, for evidence that may already have gone
        past: a boot prints mount-time recovery seconds before the marker
        that says the session is up, so a window opened at the marker starts
        after the thing it was meant to catch.
        """
        with self.lock:
            lines = self.lines[mark:]
        return scan(lines, pattern)

    def wait_for_at(self, mark, pattern, timeout):
        """As `wait_for`, but returns `(match, arrival time)`.

        The arrival time is what says whether an armed reset landed inside
        the operation or beside it — a line matching after `mark` proves the
        reset happened, not that it happened when it was wanted.
        """
        deadline = time.monotonic() + timeout
        at = mark
        while time.monotonic() < deadline:
            with self.lock:
                chunk = self.lines[at:]
                at = len(self.lines)
            for stamp, line in chunk:
                m = re.search(pattern, line)
                if m:
                    return m, stamp
            time.sleep(0.2)
        return None


def make_epub(stem, pad_bytes):
    """A classic-ZIP EPUB with unique content, titled to match its filename.

    The title matters: the catalog shows a book's persisted title when it has
    one and a label derived from its filename otherwise, so making the two
    identical means the listing reads the same either way and the campaign
    never has to know which path filled it in.
    """
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
            f'<dc:identifier id="id">{stem}-{unique}</dc:identifier>'
            f"<dc:title>{stem}</dc:title><dc:language>en</dc:language>"
            f'</metadata><manifest><item id="c1" href="ch1.xhtml" '
            f'media-type="application/xhtml+xml"/></manifest>'
            f'<spine><itemref idref="c1"/></spine></package>',
        )
        z.writestr(
            "ch1.xhtml",
            '<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml">'
            f"<head><title>{stem}</title></head><body><p>{unique}</p></body></html>",
        )
        if pad_bytes:
            # Stored, not deflated: random bytes would not compress anyway,
            # and this keeps the on-wire size predictable enough to aim at
            # the armed deadline.
            z.writestr(zipfile.ZipInfo("pad.bin"), os.urandom(pad_bytes), zipfile.ZIP_STORED)
    return out.getvalue()


class Book:
    """One line of `/list`: `B|OPENNAME|Label` (B = /BOOKS, R = card root)."""

    __slots__ = ("in_books", "label", "open_name")

    def __init__(self, in_books, open_name, label):
        self.in_books = in_books
        self.open_name = open_name
        self.label = label

    def __repr__(self):
        return f"{'B' if self.in_books else 'R'}|{self.open_name}|{self.label}"


def parse_listing(body):
    """`/list`'s payload: one `B|OPENNAME|Label` line per catalog record."""
    books = []
    for line in body.splitlines():
        if not line:
            continue
        parts = line.split("|", 2)
        if len(parts) != 3 or parts[0] not in ("B", "R"):
            raise AssertionError(f"malformed listing line: {line!r}")
        books.append(Book(parts[0] == "B", parts[1], parts[2]))
    return books


def classify_cut(reset_at, op_start, op_end):
    """Where the armed reset landed relative to the operation.

    Only `during` is a durability test. The device is reachable again within
    seconds of a reset and a TCP connect retries its SYNs across the reboot,
    so an operation fired at a device that is already resetting simply runs
    on the far side of it and answers normally — indistinguishable from a
    clean run unless the reset is timed.
    """
    if reset_at < op_start:
        return "before"
    if reset_at <= op_end:
        return "during"
    return "after"


def aim_ms(body_bytes, bytes_per_sec, fraction, floor_ms, ceiling_ms):
    """Where to put the cut, in ms after arming.

    `fraction` is how far into the expected transfer to aim. Below 1.0 lands
    in the body transfer; at and just past it lies the install itself. The
    floor keeps the arm long enough for the device to answer the arm request.

    Aiming this way reaches the transfer reliably and the install barely at
    all — see the campaign's limitation note in the module docstring. The
    parameter is kept because the transfer *is* worth cutting: it proves a
    half-streamed upload leaves no partial book behind.
    """
    if not bytes_per_sec or not body_bytes:
        return floor_ms
    expected_ms = 1000.0 * body_bytes / bytes_per_sec
    return int(max(floor_ms, min(ceiling_ms, expected_ms * fraction)))


def legal_landings(kind, target, mine, answered=None):
    """The campaign-owned label sets an operation may legally leave behind,
    keyed by what that landing is called.

    `answered` is what the device said before the reset, and it is the
    difference between a strong check and a vacuous one:

    - `True` — the operation returned success. It committed *before* the cut,
      so the only legal landing is committed. Accepting "either" here reads a
      committed operation as rolled back, and then reports the book it
      committed as a casualty on the next cycle.
    - `False` — the operation was refused. Nothing was written.
    - `None` — the connection died with the outcome unresolved. Either
      landing is legal, which is the case this campaign exists to exercise.

    A replace is the exception: it reuses the name, so every landing leaves
    exactly one book under it and `/list` cannot say which content won.
    Distinguishing them needs the book's bytes back off the card, which no
    endpoint offers.
    """
    if kind == "replace":
        return {"intact": set(mine)}
    committed = set(mine) | {target} if kind == "create" else set(mine) - {target}
    if answered is True:
        return {"committed": committed}
    if answered is False:
        return {"rolled_back": set(mine)}
    return {"committed": committed, "rolled_back": set(mine)}


class Device:
    def __init__(self, ip):
        self.base = f"http://{ip}"

    def _request(self, method, path, data=None, timeout=30):
        req = urllib.request.Request(self.base + path, data=data, method=method)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                return resp.status, resp.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as err:
            return err.code, err.read().decode("utf-8", "replace")

    def _retrying(self, call, attempts=10, delay=2):
        for attempt in range(attempts):
            try:
                return call()
            except OSError:
                if attempt == attempts - 1:
                    raise
                time.sleep(delay)
        raise AssertionError("unreachable")

    def alive(self):
        try:
            status, _ = self._request("GET", "/", timeout=3)
            return status == 200
        except OSError:
            return False

    def list_books(self):
        """The catalog snapshot this session opened with. Read-only and safe
        to retry: the listener arms a beat after the boot's "serving" line,
        and a reboot can land between any two requests of a cycle."""

        def once():
            status, body = self._request("GET", "/list", timeout=45)
            if status != 200:
                raise AssertionError(f"list -> {status}: {body}")
            return parse_listing(body)

        return self._retrying(once)

    def arm(self, after_ms):
        status, body = self._request(
            "POST", f"/test-powercut?after_ms={after_ms}", data=b"", timeout=10
        )
        if status != 200 or "armed" not in body:
            raise AssertionError(f"arm failed -> {status}: {body}")

    def arm_at_install(self, ms):
        """Hand the timing to the device: it arms when the install starts."""
        status, body = self._request(
            "POST", f"/test-powercut?at_install_ms={ms}", data=b"", timeout=10
        )
        if status != 200 or "armed" not in body:
            raise AssertionError(f"install arm failed -> {status}: {body}")

    def upload(self, filename, body, timeout=120):
        path = f"/upload?name={urllib.parse.quote(filename)}"
        return self._request("POST", path, body, timeout=timeout)

    def delete(self, open_name, in_books=True, timeout=60):
        path = f"/delete?name={urllib.parse.quote(open_name)}"
        if not in_books:
            path += "&root=1"
        return self._request("POST", path, data=b"", timeout=timeout)


class Campaign:
    def __init__(self, args):
        self.serial = Serial(args.serial, args.serial_log)
        self.device = Device(args.ip)
        self.args = args
        self.baseline = {}  # label -> Book, present before the campaign
        self.mine = {}  # label -> Book, uploaded by this campaign
        self.results = []
        self.rng = random.Random()
        # Measured by calibrate() before the first cycle.
        self.bytes_per_sec = None

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
        # Always poll until a request actually succeeds: the serial line
        # prints a beat before the listener is armed, and serial can also
        # lag or miss the line entirely.
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if self.device.alive():
                return
            time.sleep(1)
        raise AssertionError("device did not come back to serving")

    def check_recovery_clean(self, cycle, mark):
        """Recovery runs at mount and is expected to finish there. The
        installer says so out loud when it cannot, and under the sole-writer
        promise this campaign honours, it always can.

        Anchored on `mark`, taken before this cycle armed anything, because
        the only boot between there and here is the one the cut caused.
        Anchoring on the reset marker instead loses these: the `rst:` banner
        is often missed while USB re-enumerates, leaving the session's own
        "auto-starting" line as the match — and that prints seconds after
        mount-time recovery has already had its say.
        """
        for pattern, why in (
            (r"an install is still in flight", "recovery could not finish an install at mount"),
            (r"shelf unreadable", "recovery reported the shelf unreadable"),
            (
                r"cannot read; .*refused",
                "a journal record this build cannot read is blocking changes",
            ),
        ):
            if self.serial.matches_since(mark, pattern):
                self.fail(cycle, why)

    def read_shelf(self, cycle):
        """The listing, checked for the invariants that hold unconditionally."""
        books = self.device.list_books()
        seen = {}
        for book in books:
            key = (book.in_books, book.open_name)
            if key in seen:
                # Two directory entries sharing one chain is what a move
                # interrupted between its two halves leaves behind.
                self.fail(cycle, f"name listed twice: {book}")
            seen[key] = book
        by_label = {}
        for book in books:
            if book.label in by_label:
                self.fail(cycle, f"label listed twice: {book.label}")
            by_label[book.label] = book
        for label, book in self.baseline.items():
            if label not in by_label:
                self.fail(cycle, f"a book that predates the campaign vanished: {book}")
        return by_label

    def converge(self, cycle, op, by_label, answered):
        """Decide which legal landing the shelf reached, and refuse anything
        that is none of them."""
        target = op["label"]
        campaign_labels = set(by_label) - set(self.baseline)

        # Said separately from the set comparison because it is the clearer
        # message for the failure a reader will actually hit.
        if op["kind"] == "replace" and target not in campaign_labels:
            self.fail(cycle, f"a replaced book is missing entirely: {target}")

        legal = legal_landings(op["kind"], target, self.mine, answered)
        outcome = next((name for name, want in legal.items() if campaign_labels == want), None)
        if outcome is None:
            wanted = "; ".join(f"{name} would be {sorted(want)}" for name, want in legal.items())
            self.fail(
                cycle,
                f"shelf landed on no legal outcome: got {sorted(campaign_labels)}, {wanted}",
            )

        self.mine = {name: by_label[name] for name in campaign_labels}
        return outcome

    def reboot(self, why, arm_ms=200):
        """Cut the device deliberately, with nothing in flight, to get a
        fresh `/list` snapshot — it is only rendered when a session opens."""
        mark = self.serial.mark()
        self.device.arm(arm_ms)
        if not self.serial.wait_for(mark, RESET_PATTERN, 60):
            raise AssertionError(f"{why}: reboot never fired")
        self.wait_serving(mark)

    def refresh_mine(self, cycle):
        """Re-derive the campaign's own books from a freshly booted listing."""
        by_label = self.read_shelf(cycle)
        self.mine = {label: book for label, book in by_label.items() if label not in self.baseline}
        return by_label

    def calibrate(self):
        """Time one uncut upload to learn the write path's throughput.

        Without this the armed deadline is guesswork: an arm that outlasts
        the transfer resets an idle device, which proves nothing, and the
        campaign still reports a pass. The measured rate is what lets each
        cycle aim its cut *inside* the write.
        """
        body = make_epub(CALIBRATION_LABEL, 250_000)
        started = time.monotonic()
        status, text = self.device.upload(f"{CALIBRATION_LABEL}.epub", body)
        elapsed = time.monotonic() - started
        if status != 200:
            raise AssertionError(f"calibration upload -> {status}: {text}")
        self.bytes_per_sec = len(body) / max(elapsed, 0.001)
        print(
            f"calibration: {len(body) / 1000:.0f} kB in {elapsed:.1f} s "
            f"= {self.bytes_per_sec / 1000:.0f} kB/s"
        )

    def run(self):
        mark = self.serial.mark()
        print("waiting for device to serve...")
        self.wait_serving(mark)
        # The listing this session opened with can predate work done in it,
        # so the baseline is read from a session opened for the purpose.
        # Getting this wrong adopts a leftover as untouchable, or misses one.
        self.reboot("baseline")
        for book in self.device.list_books():
            if book.label.startswith(CAMPAIGN_PREFIX):
                self.mine[book.label] = book
            else:
                self.baseline[book.label] = book
        print(f"baseline: {len(self.baseline)} book(s) left alone")

        # Leftovers from a crashed run share this run's label scheme, and a
        # "create" of a label that already exists is a replace wearing a
        # create's name: both landings leave the same set, so the check
        # passes without discriminating anything. Clear them first, and
        # re-read rather than assume: a crashed run can leave a book the
        # snapshot it crashed under never showed.
        for attempt in range(4):
            if not self.mine:
                break
            print(f"clearing {len(self.mine)} leftover campaign book(s)")
            self.delete_mine()
            self.reboot(f"leftover sweep {attempt + 1}")
            self.refresh_mine(0)
        if self.mine:
            raise AssertionError(f"leftovers survived the sweep: {sorted(self.mine)}")

        self.calibrate()
        self.reboot("post-calibration")
        self.refresh_mine(0)

        for cycle in range(1, self.args.cycles + 1):
            self.cycle(cycle)

        self.cleanup()
        outcomes = {}
        for result in self.results:
            outcomes[result["outcome"]] = outcomes.get(result["outcome"], 0) + 1
        timings = {}
        for result in self.results:
            timings[result["timing"]] = timings.get(result["timing"], 0) + 1
        during = timings.get("during", 0)
        print(
            f"campaign complete: {len(self.results)} cycles, outcomes {outcomes}, "
            f"cut placement {timings}"
        )
        # Only a cut that lands inside the operation tests durability. The
        # others reset a device with nothing in flight and pass every check
        # while proving nothing, so the count is stated rather than left for
        # a reader to infer from an exit code.
        print(f"{during}/{len(self.results)} cycles were cut mid-operation.")
        if during < len(self.results) * 0.5:
            print(
                "WARNING: most cuts landed beside the operation, not inside it. "
                "A reset before the request is answered normally on the far side "
                "of the reboot (TCP retries the connect), and a reset after it "
                "hits an idle device. Adjust --min-fraction/--max-fraction and "
                "re-run before reading this as a pass."
            )

        # The journalled window, reported separately: a cut in the body
        # transfer rolls back by abandoning a staged file and proves nothing
        # about recovery. Only a cycle where recovery finished a transaction
        # says the journal's replay path ran.
        install_cycles = [r for r in self.results if r["placement"] == "install"]
        recovered = [r for r in self.results if r["recovered"]]
        moved = [r for r in recovered if r["recovery_moved"]]
        print(
            f"{len(recovered)}/{len(install_cycles)} install-timed cuts left a journal "
            f"record for recovery to replay; {len(moved)} of those needed a file moved "
            f"to finish the transaction."
        )
        if install_cycles and not recovered:
            print(
                "WARNING: no cycle exercised the journal's replay path. Every cut "
                "landed outside the window between the record becoming durable and "
                "its being cleared. Widen --install-min-ms/--install-max-ms."
            )

    def aim(self, op):
        """This cycle's arm deadline. Uploads are aimed at a random point
        inside the measured transfer; a delete carries no body to measure,
        so it gets a window scattered across the range a delete plausibly
        occupies — mount, journal write, unlink — and the timing check
        reports where the cut actually landed."""
        if op["kind"] == "delete":
            return self.rng.randrange(self.args.delete_min_ms, self.args.delete_max_ms)
        fraction = self.rng.uniform(self.args.min_fraction, self.args.max_fraction)
        return aim_ms(
            len(op["body"]),
            self.bytes_per_sec,
            fraction,
            self.args.min_ms,
            self.args.max_ms,
        )

    def placement_for(self, cycle, op):
        """Which window this cycle cuts. A delete has no body to stream, so
        there is nothing for the device-timed arm to sit in front of."""
        if op["kind"] == "delete":
            return "transfer"
        return "install" if self.rng.random() < self.args.install_share else "transfer"

    def delete_mine(self):
        """Delete every book this campaign made, with no cuts."""
        for label, book in list(self.mine.items()):
            status, body = self.device.delete(book.open_name, book.in_books)
            if status != 200:
                print(f"  warning: delete {label} -> {status} {body}")

    def cleanup(self):
        """Return the card to its baseline, then reboot so the final listing
        is a fresh snapshot rather than this session's."""
        print(f"cleanup: deleting {len(self.mine)} campaign book(s)")
        self.delete_mine()
        self.reboot("cleanup")
        remaining = [b for b in self.device.list_books() if b.label.startswith(CAMPAIGN_PREFIX)]
        if remaining:
            print(f"  warning: {len(remaining)} campaign book(s) remain: {remaining}")
        else:
            print("  card returned to its baseline")

    def cycle(self, cycle):
        # 1. Ground state, from the snapshot this session opened with.
        by_label = self.read_shelf(cycle)
        campaign_labels = set(by_label) - set(self.baseline)
        if campaign_labels != set(self.mine):
            self.fail(
                cycle,
                f"shelf drifted between cycles: expected {sorted(self.mine)}, "
                f"got {sorted(campaign_labels)}",
            )

        # 2. Choose this cycle's operation. Deletes keep the shelf from
        # growing without bound; replaces exercise the predecessor-parking
        # half of the journal, which a create never reaches.
        mine = sorted(self.mine)
        pad = self.rng.randrange(120_000, 400_000)
        if len(mine) >= 3 or (mine and cycle % 4 == 0):
            victim = self.mine[mine[0]]
            op = {"kind": "delete", "label": mine[0], "book": victim}
        elif mine and cycle % 3 == 0:
            label = mine[0]
            op = {
                "kind": "replace",
                "label": label,
                "filename": f"{label}.epub",
                "body": make_epub(label, pad),
            }
        else:
            label = f"{CAMPAIGN_PREFIX}{cycle:03}"
            # A create must name a book that does not exist: a create of a
            # standing label is a replace wearing a create's name, and both
            # its landings leave the same shelf, so the cycle would pass
            # without discriminating anything.
            if label in self.mine:
                self.fail(cycle, f"create would reuse a standing label: {label}")
            op = {
                "kind": "create",
                "label": label,
                "filename": f"{label}.epub",
                "body": make_epub(label, pad),
            }

        # 3. Arm. Two placements, because the upload has two windows worth
        # cutting and they need different mechanisms.
        #
        # `transfer` aims from the measured throughput at the body stream:
        # that proves a half-streamed upload leaves no partial book. It
        # cannot reach the install, which is the last few percent of the
        # upload and narrower than the run-to-run variation in throughput.
        #
        # `install` hands the timing to the device, which arms when the
        # install actually starts. That is the window the journal exists
        # for, and the only one where recovery has anything to do.
        #
        # A delete carries no body, so it is always aimed from the host.
        placement = self.placement_for(cycle, op)
        mark = self.serial.mark()
        if placement == "install":
            arm_ms = self.rng.randrange(self.args.install_min_ms, self.args.install_max_ms)
            self.device.arm_at_install(arm_ms)
        else:
            arm_ms = self.aim(op)
            self.device.arm(arm_ms)
        cut_mid_op = False
        # What the device said before the cut, which decides how much this
        # cycle can prove. The installer answers only once the install has
        # finished, so a success here means the operation committed and the
        # reset landed after it — weaker than a real cut, but it must still
        # be recorded as committed or the next cycle reads the book it wrote
        # as a stray.
        answered = None
        op_start = time.monotonic()
        try:
            if op["kind"] == "delete":
                status, body = self.device.delete(op["book"].open_name, op["book"].in_books)
            else:
                status, body = self.device.upload(op["filename"], op["body"])
            answered = status == 200
            landed = f"response completed (HTTP {status} {body.strip()})"
        except OSError:
            cut_mid_op = True
            landed = "connection died mid-operation"
        op_end = time.monotonic()

        # 4. Wait out the reset and the next auto-started session. The ROM's
        # `rst:` banner names the cause, but the capture can miss it while
        # USB re-enumerates — any boot-only marker proves the reset fired.
        found = self.serial.wait_for_at(mark, RESET_PATTERN, arm_ms / 1000 + 30)
        if not found:
            self.fail(cycle, "armed reset never fired")
        _, reset_at = found
        timing = classify_cut(reset_at, op_start, op_end)
        self.wait_serving(mark)
        self.check_recovery_clean(cycle, mark)
        # Did recovery find a record to replay? This is the evidence that
        # the cut reached the journalled window — the window between the
        # record becoming durable and its being cleared. Not the shipping
        # "finished an interrupted install" line: that one prints only when
        # recovery *moves* something, and a cut after the last move but
        # before the record is cleared is still a replay of a real record.
        #
        # Anchored on `mark` for the reason `check_recovery_clean` gives.
        replays = self.serial.matches_since(mark, RECOVERY_PATTERN)
        recovered = bool(replays)
        moved = any(m.group(1) == "true" for m, _ in replays)

        # 5. Verify — from a session that opened *after* the operation
        # finished. `/list` is rendered when a session opens, so when the
        # reset lands before or during the request, the operation runs in
        # the session the campaign is now talking to and its snapshot
        # predates the work. One clean reboot makes the read honest.
        self.reboot(f"cycle {cycle} verification")
        by_label = self.read_shelf(cycle)
        outcome = self.converge(cycle, op, by_label, answered)

        result = {
            "cycle": cycle,
            "op": op["kind"],
            "placement": placement,
            "arm_ms": arm_ms,
            "cut_mid_op": cut_mid_op,
            "timing": timing,
            "answered": answered,
            "recovered": recovered,
            "recovery_moved": moved,
            "landed": landed,
            "outcome": outcome,
        }
        self.results.append(result)
        if moved:
            replay = "REPLAYED+MOVED"
        elif recovered:
            replay = "REPLAYED"
        else:
            replay = ""
        print(
            f"cycle {cycle:3}: {op['kind']:7} {placement:8} arm={arm_ms:5}ms "
            f"cut={timing:6} {'CUT ' if cut_mid_op else 'done'} "
            f"{replay:14} -> {outcome}"
        )
        if self.args.results:
            with open(self.args.results, "a", encoding="utf-8") as f:
                f.write(json.dumps(result) + "\n")


class TestPowercutCampaign(unittest.TestCase):
    """The decision logic, which is what turns a durability defect into a
    reported failure. Everything else in this file needs a device."""

    def test_reads_a_listing(self):
        books = parse_listing("B|DUNE1234.EPU|Dune\nR|LOOSE.EPU|Loose\n")
        self.assertEqual(len(books), 2)
        self.assertTrue(books[0].in_books)
        self.assertEqual(books[0].open_name, "DUNE1234.EPU")
        self.assertEqual(books[0].label, "Dune")
        self.assertFalse(books[1].in_books)

    def test_keeps_a_label_containing_the_separator(self):
        # Only the first two fields are delimited; a label may hold a pipe.
        books = parse_listing("B|A.EPU|Pipes | Drums\n")
        self.assertEqual(books[0].label, "Pipes | Drums")

    def test_refuses_a_malformed_listing(self):
        for body in ("B|A.EPU\n", "X|A.EPU|Label\n", "nonsense\n"):
            with self.assertRaises(AssertionError):
                parse_listing(body)

    def test_an_unresolved_create_may_land_either_way(self):
        legal = legal_landings("create", "PCUT007", {"PCUT001"})
        self.assertEqual(legal["committed"], {"PCUT001", "PCUT007"})
        self.assertEqual(legal["rolled_back"], {"PCUT001"})

    def test_an_unresolved_delete_may_land_either_way(self):
        legal = legal_landings("delete", "PCUT001", {"PCUT001", "PCUT002"})
        self.assertEqual(legal["committed"], {"PCUT002"})
        self.assertEqual(legal["rolled_back"], {"PCUT001", "PCUT002"})

    def test_an_answered_operation_has_only_the_answer(self):
        """The regression this cost a hardware run to find: a delete that
        returned 200 committed, and reading it as "either" reports the book
        it removed as a casualty on the very next cycle."""
        legal = legal_landings("delete", "PCUT001", {"PCUT001", "PCUT002"}, answered=True)
        self.assertEqual(legal, {"committed": {"PCUT002"}})
        legal = legal_landings("create", "PCUT007", {"PCUT001"}, answered=True)
        self.assertEqual(legal, {"committed": {"PCUT001", "PCUT007"}})

    def test_a_refused_operation_wrote_nothing(self):
        legal = legal_landings("create", "PCUT007", {"PCUT001"}, answered=False)
        self.assertEqual(legal, {"rolled_back": {"PCUT001"}})

    def test_a_replace_has_one_indistinguishable_landing(self):
        legal = legal_landings("replace", "PCUT001", {"PCUT001"})
        self.assertEqual(legal, {"intact": {"PCUT001"}})
        # Even answered: /list cannot say which content won.
        legal = legal_landings("replace", "PCUT001", {"PCUT001"}, answered=True)
        self.assertEqual(legal, {"intact": {"PCUT001"}})

    def test_finds_evidence_that_precedes_the_reset_marker(self):
        """Cost three hardware runs to see. Mount-time recovery prints
        seconds before the session's "auto-starting" line, and that line is
        routinely the only reset marker the capture keeps — so evidence
        filtered to "after the reset marker" is evidence thrown away. Real
        line order from an X3 run."""
        lines = [
            (100.0, "rst:0x9 (RTC_WDT_SYS_RST),boot:0xd"),
            (101.0, "powercut: recovery replayed a record: moved=true swept=true complete=true"),
            (104.0, "powercut: auto-starting wireless session"),
        ]
        found = scan(lines, RECOVERY_PATTERN)
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0][0].group(1), "true")
        # The evidence is older than the marker a naive anchor would use.
        marker = scan(lines, r"powercut: auto-starting")[0][1]
        self.assertLess(found[0][1], marker)

    def test_scan_returns_every_match_not_just_the_first(self):
        lines = [(1.0, "a=1"), (2.0, "b"), (3.0, "a=2")]
        self.assertEqual([m.group(1) for m, _ in scan(lines, r"a=(\d)")], ["1", "2"])

    def test_places_the_cut_against_the_operation(self):
        self.assertEqual(classify_cut(reset_at=5.0, op_start=10.0, op_end=12.0), "before")
        self.assertEqual(classify_cut(reset_at=11.0, op_start=10.0, op_end=12.0), "during")
        self.assertEqual(classify_cut(reset_at=13.0, op_start=10.0, op_end=12.0), "after")

    def test_a_reset_before_the_request_is_not_a_cut(self):
        """The hardware run's second false failure: a reset that fires
        before the request is sent lets the operation run to completion on
        the far side of the reboot, because a TCP connect retries across
        it. Counting that as a cut credits a test that never happened."""
        self.assertEqual(classify_cut(reset_at=9.99, op_start=10.0, op_end=12.0), "before")

    def test_aims_inside_the_expected_transfer(self):
        # 200 kB at 100 kB/s is a 2 s write; half way in is 1000 ms.
        self.assertEqual(aim_ms(200_000, 100_000, 0.5, 150, 20_000), 1000)
        self.assertEqual(aim_ms(200_000, 100_000, 0.25, 150, 20_000), 500)

    def test_aim_respects_its_clamps(self):
        # A tiny body would aim below the floor; the device still needs long
        # enough to answer the arm request.
        self.assertEqual(aim_ms(1_000, 100_000, 0.5, 150, 20_000), 150)
        # A huge one would stretch the cycle; the ceiling caps it.
        self.assertEqual(aim_ms(100_000_000, 100_000, 0.5, 150, 20_000), 20_000)

    def test_aim_falls_back_when_there_is_nothing_to_measure(self):
        # A delete carries no body, and the first cycle has no measurement.
        self.assertEqual(aim_ms(0, 100_000, 0.5, 150, 20_000), 150)
        self.assertEqual(aim_ms(200_000, None, 0.5, 150, 20_000), 150)

    def test_no_landing_covers_a_stranger_or_a_casualty(self):
        """The failures this campaign exists to catch: a book that was not
        the target appearing or vanishing must match no legal landing."""
        legal = legal_landings("create", "PCUT007", {"PCUT001"})
        stranger = {"PCUT001", "PCUT007", "PCUT999"}
        casualty = {"PCUT007"}
        for got in (stranger, casualty):
            self.assertNotIn(got, legal.values())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ip", required=True)
    parser.add_argument("--serial", required=True)
    parser.add_argument("--cycles", type=int, default=20)
    # Clamps on the aimed deadline, not the aim itself: the floor keeps the
    # arm long enough for the device to answer the arm request, the ceiling
    # keeps a slow card from stretching a cycle indefinitely.
    parser.add_argument("--min-ms", type=int, default=150)
    parser.add_argument("--max-ms", type=int, default=20000)
    # How far into the expected transfer to cut. Staying well under 1.0 is
    # the point — at or past it the write has already finished.
    parser.add_argument("--min-fraction", type=float, default=0.15)
    parser.add_argument("--max-fraction", type=float, default=0.85)
    # A delete has no transfer to aim inside, so its window is scattered
    # across the span one plausibly occupies and the result is reported.
    parser.add_argument("--delete-min-ms", type=int, default=200)
    parser.add_argument("--delete-max-ms", type=int, default=1200)
    # The device-timed cut: the deadline runs from the start of the install,
    # so this range sweeps the journalled window itself.
    parser.add_argument("--install-min-ms", type=int, default=4)
    parser.add_argument("--install-max-ms", type=int, default=400)
    # Share of upload cycles that cut the install rather than the transfer.
    parser.add_argument("--install-share", type=float, default=0.6)
    parser.add_argument("--serial-log", default="/tmp/powercut_serial.log")
    parser.add_argument("--results", default="")
    args = parser.parse_args()
    Campaign(args).run()


if __name__ == "__main__":
    main()
