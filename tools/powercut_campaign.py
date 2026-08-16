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
  must land on one of them *holding the right bytes*: for an upload, the book
  is wholly there or wholly absent; for a delete, gone or still present.
  Anything else — a stranger appearing, an untargeted book vanishing, a name
  listed twice, a book whose contents match neither landing — is the failure
  this campaign exists to catch. A duplicated name is the specific signature
  of a half-finished move, where two directory entries share one cluster
  chain.
- **A standing journal record refuses further uploads and deletes.** So the
  next cycle's operation being accepted at all is the proof that recovery
  cleared the record. Cleanup is checked for the same thing, because the
  last cycle has no next one.

The oracle reads books back off the card (`/test-digest`, selftest only)
rather than trusting the listing. The length `/list` reports is the FAT
directory entry's, which is metadata written beside the data: an entry of
the right size whose chain is short, unreadable, or pointing at somebody
else's clusters passes a length check — and mangled entries and chains are
precisely what an interrupted install would leave. The digest comes from
opening the file and streaming it, so it describes the chain.

Books already on the card are an untouched baseline. What that is checked
against differs by cost, and the difference is worth knowing before reading
a pass:

- **Every cycle**, for every baseline book: still listed, still the same
  directory-entry size. Free, since the listing is read anyway.
- **Every cycle**, for every book the campaign owns: read back and compared
  to the bytes it wrote. These are small and few, and they are the ones a
  cut could plausibly damage without touching their names.
- **At the start and end of a run** (`--verify-baseline`), for every
  baseline book: read back byte for byte. This is a full read of the card —
  ~4 minutes for a 184 MB shelf at the ~800 kB/s an X3 manages — so it is
  not run per cycle unless asked. A corrupted baseline book is therefore
  caught, but possibly some cycles after the cut that caused it.

What the campaign may delete comes from a host-side manifest (`--manifest`),
and a claim in it is an intent, not a licence: before treating a listed book
as its own the campaign reads it back and checks the bytes are the bytes it
wrote. A book whose contents do not match — a reader's own book that happens
to be called `PCUT...`, or one that replaced a claim after a crashed run —
stops the run rather than being deleted.

A run whose cuts did not actually land inside operations, or never reached
the journal's replay path, exits non-zero. Green has to mean the property
was exercised, not merely that nothing contradicted it.

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


# How much of a book to digest per request. The device answers over a socket
# with a 30-second idle timeout and reads at ~800 kB/s, so a whole 29 MB book
# in one request outlives the connection carrying its answer — measured, as
# an empty reply for that book and for whatever was asked next. Four
# megabytes is about five seconds.
DIGEST_CHUNK_BYTES = 4 * 1024 * 1024

DIGEST_SEED = 0xCBF29CE484222325
DIGEST_PRIME = 0x100000001B3
DIGEST_MASK = (1 << 64) - 1


def digest(body):
    """FNV-1a over `body`, matching `fw::powercut::digest_chunk`.

    Not a cryptographic claim: the campaign compares against a body it
    generated itself, so this only has to catch bytes that differ, not bytes
    chosen to collide.
    """
    hash_ = DIGEST_SEED
    for byte in body:
        hash_ = ((hash_ ^ byte) * DIGEST_PRIME) & DIGEST_MASK
    return hash_


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
    """One line of `/list`: `B|OPENNAME|Label|SIZE` (B = /BOOKS, R = card
    root). The size field is present only in a `powercut-selftest` build and
    is required here — see `parse_listing`."""

    __slots__ = ("in_books", "label", "open_name", "size")

    def __init__(self, in_books, open_name, label, size):
        self.in_books = in_books
        self.open_name = open_name
        self.label = label
        self.size = size

    def __repr__(self):
        return f"{'B' if self.in_books else 'R'}|{self.open_name}|{self.label}|{self.size}"


class Manifest:
    """The host's record of which books on the card this tool created.

    Ownership has to be knowledge, not inference: the campaign deletes what
    it owns, and it is documented to run against a card that also holds the
    user's library. Kept on the host and written through on every claim, so
    a run killed mid-cycle still leaves a truthful list behind.

    Two states per label, and the difference decides whether this tool may
    delete a book:

    - **pending** — recorded before the upload, so a crash mid-write leaves
      the intent behind. It carries the identity of the bytes that were
      about to be sent. A pending claim is *not* authority to delete: the
      upload may never have landed, and by the time anyone looks the label
      could belong to a book somebody else put there.
    - **owned** — the identity was read back off the card and matched. Only
      these are deleted.

    So ownership is knowledge in the strict sense: the campaign deletes a
    book only when it has read that book's bytes and they are the bytes it
    wrote. A label whose contents do not match is somebody else's.

    A claim holds every identity the label may legitimately have, which
    during a write is two: an interrupted replace leaves either the body
    that was there or the one being written, and both are the campaign's.
    The pair collapses to the one observed as soon as a cycle reads the
    result back.
    """

    def __init__(self, path):
        self.path = path
        # label -> {(length, digest), ...} the campaign wrote or meant to.
        self.claims = {}

    @property
    def owned(self):
        return set(self.claims)

    def load(self):
        """Read the manifest. One line per label:

            LABEL<tab>LENGTH:DIGEST[<tab>LENGTH:DIGEST...]

        A line that does not parse stops the run with the line in hand. This
        file is meant to be inspected and sometimes edited — the error a
        reader gets for a stray keystroke should say what is wrong with it.
        """
        self.claims = {}
        try:
            with open(self.path, encoding="utf-8") as f:
                for number, line in enumerate(f, start=1):
                    if not line.strip():
                        continue
                    label, *fields = line.rstrip("\n").split("\t")
                    identities = set()
                    for field in fields:
                        length, _, hash_ = field.partition(":")
                        if (
                            not length.isdigit()
                            or len(hash_) != 16
                            or not all(c in "0123456789abcdef" for c in hash_)
                        ):
                            raise AssertionError(
                                f"{self.path}:{number}: cannot read {field!r} as "
                                f"LENGTH:DIGEST — each claim is a decimal length and a "
                                f"16-digit hex digest, tab-separated from the label"
                            )
                        identities.add((int(length), int(hash_, 16)))
                    if not identities:
                        raise AssertionError(
                            f"{self.path}:{number}: {label!r} claims nothing, so it could "
                            f"never be verified. Delete the line."
                        )
                    self.claims[label] = identities
        except FileNotFoundError:
            pass
        return self.claims

    def claim(self, label, identities):
        """Record every identity `label` may legitimately hold.

        A claim with no identities would be a label this tool owns and can
        never recognise — unverifiable, and so undeletable. `release` is the
        way to say there is nothing there.
        """
        wanted = {identity for identity in identities if identity is not None}
        if not wanted:
            raise AssertionError(f"a claim on {label} must name what it may contain")
        if self.claims.get(label) == wanted:
            return
        self.claims[label] = wanted
        self._write()

    def release(self, label):
        self.claims.pop(label, None)
        self._write()

    def _write(self):
        # Rewritten and fsynced on every change: a manifest that lags the
        # card either orphans files or claims books it does not own.
        with open(self.path, "w", encoding="utf-8") as f:
            for label, identities in sorted(self.claims.items()):
                fields = "\t".join(f"{length}:{hash_:016x}" for length, hash_ in sorted(identities))
                f.write(f"{label}\t{fields}\n")
            f.flush()
            os.fsync(f.fileno())


class ListingTruncated(Exception):
    """The device could not fit the whole catalog in the shelf buffer.

    Fatal rather than tolerated: the campaign's central invariant is that
    nothing outside its own books moved, and a listing that silently dropped
    entries cannot support it.
    """


def parse_listing(body):
    """`/list`'s payload: one `B|OPENNAME|Label|SIZE` line per catalog record.

    The size is peeled off the end rather than counted from the front,
    because a label may legitimately contain the separator.

    A missing size is an error, not a default. It means the device is running
    a shipping build, and without sizes the campaign's oracle degrades to
    "a book with that name exists" — which cannot tell a complete book from a
    truncated one, and would report a pass for exactly the corruption this
    exists to catch.
    """
    books = []
    for line in body.splitlines():
        if not line:
            continue
        if line.startswith("!TRUNCATED|"):
            _, written, total = line.split("|", 2)
            raise ListingTruncated(
                f"the device listed {written} of {total} books; the shelf buffer "
                f"could not hold the catalog, so nothing about this card can be "
                f"proven untouched"
            )
        head, sep, size = line.rpartition("|")
        if not sep or not size.isdigit():
            raise AssertionError(
                f"listing line has no size field: {line!r} — is this a powercut-selftest build?"
            )
        parts = head.split("|", 2)
        if len(parts) != 3 or parts[0] not in ("B", "R"):
            raise AssertionError(f"malformed listing line: {line!r}")
        books.append(Book(parts[0] == "B", parts[1], parts[2], int(size)))
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


def landing_identity(kind, landing, new_identity, old_identity):
    """The `(length, digest)` the target book must have under a given
    landing, or None if it should be absent.

    This is what turns presence into proof, and it has to be the bytes
    rather than the length. The length in `/list` is the FAT directory
    entry's, which is metadata written beside the data: an entry of the
    right size whose chain is short, unreadable, or pointing at somebody
    else's clusters satisfies a length check and is exactly the damage an
    interrupted FAT manipulation would do. The digest comes from reading the
    file back, so it describes the chain.
    """
    if kind == "delete":
        return None if landing == "committed" else old_identity
    if landing == "committed":
        return new_identity
    return old_identity if kind == "replace" else None


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

    A replace leaves the same label set either way, because it reuses the
    name. Its two landings are told apart by size instead — see
    `landing_size` — so both are returned here and the caller narrows them.
    """
    if kind == "create":
        committed = set(mine) | {target}
    elif kind == "delete":
        committed = set(mine) - {target}
    else:
        committed = set(mine)
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

    def digest_book(self, open_name, in_books=True, timeout=180):
        """`(length, digest)` of what is actually in the named book, or None.

        The device opens the file, streams it, and hashes the bytes, so this
        describes the cluster chain rather than the directory entry over it.
        Slow by nature — a megabyte read on the session's stack — hence the
        long default timeout.
        """
        base = f"/test-digest?name={urllib.parse.quote(open_name)}"
        if not in_books:
            base += "&root=1"

        def piece(offset, hash_):
            def once():
                path = f"{base}&from={offset}&len={DIGEST_CHUNK_BYTES}&seed={hash_:016x}"
                status, body = self._request("GET", path, timeout=timeout)
                if status != 200:
                    raise AssertionError(f"digest {open_name} -> {status}: {body}")
                if body.strip() == "unreadable":
                    return None
                m = re.fullmatch(r"size=(\d+) read=(\d+) fnv=([0-9a-f]{16})", body.strip())
                if not m:
                    raise AssertionError(f"digest {open_name} -> unparsable {body!r}")
                return int(m.group(1)), int(m.group(2)), int(m.group(3), 16)

            return self._retrying(once)

        offset, hash_ = 0, DIGEST_SEED
        size = None
        while True:
            got = piece(offset, hash_)
            if got is None:
                return None
            size, read, hash_ = got
            offset += read
            if read == 0 or offset >= size:
                break
        # A chain that runs short of what the directory entry advertises is
        # exactly the corruption this exists to catch, so it reads as a book
        # that is not there rather than as a book of a different length.
        if offset != size:
            return None
        return size, hash_

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
        self.baseline_identity = {}  # label -> (length, digest), if verified
        self.mine = {}  # label -> Book, uploaded by this campaign
        self.results = []
        self.rng = random.Random()
        self.manifest = Manifest(args.manifest)
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
            # Mount-time, from the boot scan's reconcile.
            (r"an install is still in flight", "recovery could not finish an install at mount"),
            (r"shelf unreadable", "recovery reported the shelf unreadable"),
            (
                r"cannot read; .*refused",
                "a journal record this build cannot read is blocking changes",
            ),
            # Session-start, from the upload writer. A distinct path with its
            # own wording: mount-time recovery can complete while the session
            # still refuses, and either one means the journal did not clear.
            (
                r"an install is unfinished; refusing changes",
                "the upload session refused changes over an unfinished install",
            ),
            (
                r"no install is replayed while the old snapshot stands",
                "recovery was blocked because the catalog snapshot would not clear",
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
            # The cheap half of "untouched", run every cycle. The byte-level
            # half is `verify_baseline`, which costs a full read of the
            # library and so runs at the ends of a campaign by default.
            if by_label[label].size != book.size:
                self.fail(
                    cycle,
                    f"a book that predates the campaign changed size: {book} "
                    f"was {book.size}, now {by_label[label].size}",
                )
        return by_label

    def verify_identities(self, cycle, books, expected, what):
        """Read each of `books` back and check it against `expected`.

        `expected` maps label to the identity, or set of identities, the book
        may legitimately have. This is the only check that sees a book's
        contents; everything else in `read_shelf` is the directory entry,
        which is metadata written beside the data and survives corruption of
        it.
        """
        for label, book in sorted(books.items()):
            want = expected.get(label)
            if want is None:
                continue
            allowed = want if isinstance(want, set) else {want}
            got = self.device.digest_book(book.open_name, book.in_books)
            if got is None:
                self.fail(
                    cycle,
                    f"{what} {label} is listed but could not be read back: {book}. "
                    f"The directory entry outlived the data it points at.",
                )
            if got not in allowed:
                self.fail(
                    cycle,
                    f"{what} {label} reads back as {got}, not {sorted(allowed)}. "
                    f"Nothing this campaign did should have changed it.",
                )

    def verify_baseline(self, cycle):
        """Byte-check every book that predates the campaign.

        Expensive — a full read of the library, measured at ~800 kB/s on an
        X3, so ~4 minutes for a 184 MB shelf — which is why it is not run
        every cycle by default. What it protects is the campaign's loudest
        claim: that a durability test run beside somebody's library leaves
        that library alone.
        """
        if not self.baseline_identity:
            return
        print(f"  verifying {len(self.baseline_identity)} baseline book(s) byte for byte...")
        self.verify_identities(
            cycle,
            {label: self.baseline[label] for label in self.baseline_identity},
            self.baseline_identity,
            "a book that predates the campaign,",
        )

    def converge(self, cycle, op, by_label, answered):
        """Decide which legal landing the shelf reached, and refuse anything
        that is none of them.

        Two gates, and the second is what makes this an oracle rather than a
        head-count: the set of campaign books must match a legal landing, and
        the target book's *contents*, read back off the card, must match what
        that landing implies. A book published half-written carries the name
        the campaign expects and would pass the first gate on its own.
        """
        target = op["label"]
        campaign_labels = set(by_label) - set(self.baseline)

        # Said separately from the set comparison because it is the clearer
        # message for the failure a reader will actually hit.
        if op["kind"] == "replace" and target not in campaign_labels:
            self.fail(cycle, f"a replaced book is missing entirely: {target}")

        legal = legal_landings(op["kind"], target, self.mine, answered)
        by_labels = [name for name, want in legal.items() if campaign_labels == want]
        if not by_labels:
            wanted = "; ".join(f"{name} would be {sorted(want)}" for name, want in legal.items())
            self.fail(
                cycle,
                f"shelf landed on no legal outcome: got {sorted(campaign_labels)}, {wanted}",
            )

        landed = by_label.get(target)
        got = self.device.digest_book(landed.open_name, landed.in_books) if landed else None
        # A listed book the device cannot read back is a failure on its own,
        # whatever the label sets say: the entry survived and the data behind
        # it did not.
        if landed and got is None:
            self.fail(
                cycle,
                f"{target} is listed as {landed.size} bytes but could not be read back. "
                f"The directory entry outlived the data it points at.",
            )
        if landed and got[0] != landed.size:
            self.fail(
                cycle,
                f"{target} is listed as {landed.size} bytes but reads back as {got[0]}. "
                f"The directory entry disagrees with its own cluster chain.",
            )
        by_content = [
            name
            for name in by_labels
            if landing_identity(op["kind"], name, op.get("new_identity"), op.get("old_identity"))
            == got
        ]
        if not by_content:
            wanted = "; ".join(
                f"{name} implies "
                f"{landing_identity(op['kind'], name, op.get('new_identity'), op.get('old_identity'))}"
                for name in by_labels
            )
            self.fail(
                cycle,
                f"{target} reads back as {got}, which no landing allows: {wanted}. "
                f"A book whose bytes are neither what was there nor what was sent "
                f"is a book published half-written.",
            )

        self.mine = {name: by_label[name] for name in campaign_labels}
        return by_content[0]

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
        # Claimed before the write, not after: a run killed here must leave
        # behind a manifest that already knows what this book would contain.
        self.manifest.claim(CALIBRATION_LABEL, [(len(body), digest(body))])
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

        # Ownership is read from the manifest of what this tool actually
        # created, never inferred from the label. A title prefix is a guess
        # about somebody else's library, and the campaign acts on ownership
        # by deleting — so a reader whose own book happens to be called
        # "PCUT..." would lose it. The manifest is on the host and survives
        # a crashed run, which is the case the guess existed to cover.
        claims = self.manifest.load()
        listed = self.device.list_books()

        # A claim is an intent, not a licence. Before treating a listed book
        # as the campaign's — which means deleting it — read it back and
        # check it is the book the manifest describes. A run that died before
        # its upload landed leaves a claim whose label may since have been
        # taken by a book somebody else put on the card, and the whole point
        # of this tool being safe beside a real library is that such a book
        # is not touched.
        unverified = []
        for book in listed:
            if book.label not in claims:
                self.baseline[book.label] = book
                continue
            got = self.device.digest_book(book.open_name, book.in_books)
            if got in claims[book.label]:
                self.mine[book.label] = book
            else:
                unverified.append((book, claims[book.label], got))
        print(f"baseline: {len(self.baseline)} book(s) left alone")

        # Two ways to end up here, and neither may be resolved by guessing:
        # a book whose label this campaign claims but whose contents are not
        # what it wrote, and a book named like the campaign that no claim
        # covers at all.
        strangers = sorted(
            label
            for label in self.baseline
            if label.startswith(CAMPAIGN_PREFIX) or label == CALIBRATION_LABEL
        )
        if unverified:
            described = ", ".join(
                f"{book.label} (claimed {claimed}, found {got})"
                for book, claimed, got in unverified
            )
            raise AssertionError(
                f"books this campaign's manifest claims whose contents are not what it "
                f"wrote: {described}. They are not this tool's to delete. Move or rename "
                f"them, or clear the manifest at {self.manifest.path} if you know they "
                f"are disposable."
            )
        if strangers and not self.args.adopt_unclaimed:
            raise AssertionError(
                f"books named like this campaign's, which its manifest does not claim: "
                f"{strangers}. They may be yours. Re-run with --adopt-unclaimed to have "
                f"the campaign take ownership and delete them, or rename them first. "
                f"Manifest: {self.manifest.path}"
            )
        if strangers:
            print(f"adopting {len(strangers)} unclaimed book(s) named like this campaign")
            for label in strangers:
                book = self.baseline.pop(label)
                self.mine[label] = book
                got = self.device.digest_book(book.open_name, book.in_books)
                if got is not None:
                    self.manifest.claim(label, [got])

        # Claims with nothing on the card behind them are spent intents from
        # a run that died before its upload landed. Dropping them here keeps
        # a stale label from ever meeting a future book that happens to
        # share it.
        for label in sorted(set(claims) - {book.label for book in listed}):
            self.manifest.release(label)

        # What the baseline actually contains, so the promise to leave it
        # alone can be checked against bytes rather than against the
        # directory entries written beside them.
        if self.args.verify_baseline != "never":
            total = sum(book.size for book in self.baseline.values())
            print(
                f"reading {len(self.baseline)} baseline book(s) "
                f"({total / 1_048_576:.0f} MB) to record what untouched means..."
            )
            for label, book in sorted(self.baseline.items()):
                got = self.device.digest_book(book.open_name, book.in_books)
                if got is None:
                    raise AssertionError(
                        f"a book that predates the campaign cannot be read: {book}. "
                        f"This card has a problem the campaign did not cause, and a "
                        f"baseline that cannot be read cannot be proven untouched."
                    )
                self.baseline_identity[label] = got

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

        # Coverage is a result, not a footnote. A run whose cuts all landed
        # beside the operation checked a device that was doing nothing, and
        # exiting 0 on it says "the installer is power-cut safe" on evidence
        # that contains no power cut. Refuse instead, and say what to change.
        shortfalls = []
        if during < len(self.results) * self.args.min_cut_share:
            shortfalls.append(
                f"only {during}/{len(self.results)} cycles were cut mid-operation "
                f"(wanted {self.args.min_cut_share:.0%}); a reset before the request is "
                f"answered normally on the far side of the reboot and one after it hits "
                f"an idle device — adjust --min-fraction/--max-fraction"
            )
        if len(recovered) < self.args.min_replays:
            shortfalls.append(
                f"only {len(recovered)} cut(s) reached the journal's replay path "
                f"(wanted {self.args.min_replays}); the window is between the record "
                f"becoming durable and its being cleared — adjust "
                f"--install-min-ms/--install-max-ms/--install-share"
            )
        if shortfalls:
            print("INSUFFICIENT COVERAGE — this run does not support a pass:")
            for shortfall in shortfalls:
                print(f"  - {shortfall}")
            sys.exit(1)

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

    def identity_of(self, label):
        """What is actually in one of the campaign's books right now."""
        book = self.mine[label]
        got = self.device.digest_book(book.open_name, book.in_books)
        if got is None:
            raise AssertionError(f"{label} is listed but could not be read back: {book}")
        return got

    def body_differing_from(self, label, old_len, tries=8):
        """An EPUB for `label` whose encoded length is not `old_len`.

        Compression makes the finished length only loosely a function of the
        padding, so this checks rather than assumes, and gives up loudly:
        silently returning a same-length body would leave a replace cycle
        that cannot fail.
        """
        for _ in range(tries):
            body = make_epub(label, self.rng.randrange(120_000, 400_000))
            if len(body) != old_len:
                return body
        raise AssertionError(
            f"could not build a replacement for {label} differing in length from {old_len}"
        )

    def placement_for(self, cycle, op):
        """Which window this cycle cuts. A delete has no body to stream, so
        there is nothing for the device-timed arm to sit in front of."""
        if op["kind"] == "delete":
            return "transfer"
        return "install" if self.rng.random() < self.args.install_share else "transfer"

    def delete_mine(self):
        """Delete every book this campaign made, with no cuts."""
        # Deliberately does not release anything. A 200 says the device
        # accepted the delete, not that the entry is gone; the caller proves
        # that from a fresh listing and releases then. Releasing here would
        # hide a delete that answered but did not take.
        for label, book in list(self.mine.items()):
            status, body = self.device.delete(book.open_name, book.in_books)
            if status != 200:
                print(f"  warning: delete {label} -> {status} {body}")

    def cleanup(self):
        """Return the card to its baseline, then reboot so the final listing
        is a fresh snapshot rather than this session's.

        Failures here are fatal, not advisory. The last cycle's verification
        is the only one no later cycle re-checks, and a journal left standing
        by that cut refuses every delete — so books surviving cleanup is
        exactly the signature of an unfinished recovery escaping the run.
        """
        print(f"cleanup: deleting {len(self.mine)} campaign book(s)")
        mark = self.serial.mark()
        # The set to prove gone is captured before anything is deleted, and
        # nothing is released until after the reboot. Releasing on a 200
        # would take each book out of the set the final check looks at, so a
        # delete the device answered but did not perform would be invisible
        # to the very check meant to catch it.
        expected_gone = set(self.mine)
        self.delete_mine()
        self.reboot("cleanup")
        self.check_recovery_clean("cleanup", mark)
        listed = {book.label for book in self.device.list_books()}
        remaining = sorted(expected_gone & listed)
        if remaining:
            self.fail(
                "cleanup",
                f"{len(remaining)} campaign book(s) survived cleanup: {remaining}. "
                f"A delete that will not take is what a standing journal record "
                f"looks like from outside.",
            )
        # Proven absent, so the claims can go. Claims for books that never
        # landed go too — the listing is known complete here, since a
        # truncated one is fatal.
        for label in sorted(set(self.manifest.load()) - listed):
            self.manifest.release(label)
        self.mine = {}
        # "Returned to its baseline" is a claim about the user's library, and
        # until here it rested on names and directory-entry sizes. Deleting
        # is the campaign's most destructive act and cleanup does the most of
        # it, so the library is read back before that sentence is printed.
        self.read_shelf("cleanup")
        self.verify_baseline("cleanup")
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
        if len(mine) >= 3 or (mine and cycle % 4 == 0):
            victim = self.mine[mine[0]]
            op = {
                "kind": "delete",
                "label": mine[0],
                "book": victim,
                "new_identity": None,
                "old_identity": self.identity_of(mine[0]),
            }
        elif mine and cycle % 3 == 0:
            label = mine[0]
            old_identity = self.identity_of(label)
            # The replacement must differ from what it replaces, or the
            # operation's two landings would be indistinguishable and the
            # cycle unfalsifiable. Distinct random contents make the digests
            # differ; the length check below makes it plain to read.
            body = self.body_differing_from(label, old_identity[0])
            op = {
                "kind": "replace",
                "label": label,
                "filename": f"{label}.epub",
                "body": body,
                "new_identity": (len(body), digest(body)),
                "old_identity": old_identity,
            }
        else:
            label = f"{CAMPAIGN_PREFIX}{cycle:03}"
            # A create must name a book that does not exist: a create of a
            # standing label is a replace wearing a create's name, and both
            # its landings leave the same shelf, so the cycle would pass
            # without discriminating anything.
            if label in self.mine:
                self.fail(cycle, f"create would reuse a standing label: {label}")
            body = make_epub(label, self.rng.randrange(120_000, 400_000))
            op = {
                "kind": "create",
                "label": label,
                "filename": f"{label}.epub",
                "body": body,
                "new_identity": (len(body), digest(body)),
                "old_identity": None,
            }

        # Claimed before the write, so a run killed mid-upload leaves a
        # manifest describing what it may have created. A replace changes
        # what the label will contain, so the claim moves with it — the old
        # identity stops being this campaign's proof the moment the new
        # bytes may have landed, and either body is legitimately ours.
        if op["kind"] != "delete":
            self.manifest.claim(op["label"], [op["new_identity"], op.get("old_identity")])

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
        # Collapse the claim onto what the card actually holds now, so the
        # two-identity window a write opens closes as soon as it is resolved.
        settled = landing_identity(
            op["kind"], outcome, op.get("new_identity"), op.get("old_identity")
        )
        if settled is None:
            # Nothing under this label any more — a committed delete, or a
            # create that rolled back. There is no book to own.
            self.manifest.release(op["label"])
        else:
            self.manifest.claim(op["label"], [settled])

        # The target is proven by `converge`. Everything else the campaign
        # owns is collateral: a cut that corrupted a book it was not aiming
        # at, without disturbing that book's name or directory entry, is
        # invisible to every check above. These are small and few, so they
        # are read back every cycle.
        self.verify_identities(cycle, self.mine, self.manifest.claims, "a campaign book,")
        if self.args.verify_baseline == "always":
            self.verify_baseline(cycle)

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
        books = parse_listing("B|DUNE1234.EPU|Dune|4096\nR|LOOSE.EPU|Loose|17\n")
        self.assertEqual(len(books), 2)
        self.assertTrue(books[0].in_books)
        self.assertEqual(books[0].open_name, "DUNE1234.EPU")
        self.assertEqual(books[0].label, "Dune")
        self.assertEqual(books[0].size, 4096)
        self.assertFalse(books[1].in_books)

    def test_keeps_a_label_containing_the_separator(self):
        # The size is peeled off the end, so a label may still hold a pipe.
        books = parse_listing("B|A.EPU|Pipes | Drums|900\n")
        self.assertEqual(books[0].label, "Pipes | Drums")
        self.assertEqual(books[0].size, 900)

    def test_refuses_a_listing_without_sizes(self):
        """A shipping build answers /list without the size field. Running
        against one would silently reduce the oracle to name-matching, which
        cannot tell a complete book from a truncated one."""
        with self.assertRaises(AssertionError):
            parse_listing("B|A.EPU|Label\n")

    def test_refuses_a_truncated_listing(self):
        """A listing the buffer could not hold cannot support the promise
        that nothing outside the campaign's own books moved."""
        with self.assertRaises(ListingTruncated):
            parse_listing("B|A.EPU|Label|10\n!TRUNCATED|1|900\n")

    def test_refuses_a_malformed_listing(self):
        for body in ("B|A.EPU|12\n", "X|A.EPU|Label|12\n", "nonsense|1\n"):
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

    def test_a_replace_leaves_the_same_labels_either_way(self):
        legal = legal_landings("replace", "PCUT001", {"PCUT001"})
        self.assertEqual(legal["committed"], {"PCUT001"})
        self.assertEqual(legal["rolled_back"], {"PCUT001"})

    def test_contents_are_what_tell_a_replace_apart(self):
        """The label set cannot distinguish a replace's landings, so the
        bytes do: committed is the new body, rolled back is the old one.
        Without this the cycle asserts only that a book with the right name
        exists, which a half-written one also satisfies."""
        new, old = (500, 0xAAAA), (300, 0xBBBB)
        self.assertEqual(landing_identity("replace", "committed", new, old), new)
        self.assertEqual(landing_identity("replace", "rolled_back", new, old), old)

    def test_identity_for_a_create_and_a_delete(self):
        new, old = (500, 0xAAAA), (300, 0xBBBB)
        self.assertEqual(landing_identity("create", "committed", new, None), new)
        # A rolled-back create leaves nothing behind to read.
        self.assertIsNone(landing_identity("create", "rolled_back", new, None))
        self.assertIsNone(landing_identity("delete", "committed", None, old))
        self.assertEqual(landing_identity("delete", "rolled_back", None, old), old)

    def test_wrong_bytes_at_the_right_length_match_no_landing(self):
        """The false green content checking exists to prevent, and the one a
        length check cannot: an entry advertising the expected size whose
        chain holds something else."""
        new = (500, 0xAAAA)
        landings = {
            landing: landing_identity("create", landing, new, None)
            for landing in ("committed", "rolled_back")
        }
        same_length_wrong_bytes = (500, 0x1234)
        self.assertNotIn(same_length_wrong_bytes, landings.values())

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

    def test_manifest_round_trips_identities(self):
        """The file is the record, not the in-memory set: a killed run
        leaves only the file behind."""
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            manifest = Manifest(os.path.join(tmp, "m.txt"))
            self.assertEqual(manifest.load(), {})
            manifest.claim("PCUT001", [(500, 0xABCD)])
            manifest.claim("PCUT002", [(600, 0x1234), (700, 0x5678)])
            reloaded = Manifest(manifest.path).load()
            self.assertEqual(reloaded["PCUT001"], {(500, 0xABCD)})
            self.assertEqual(reloaded["PCUT002"], {(600, 0x1234), (700, 0x5678)})
            manifest.release("PCUT001")
            self.assertEqual(set(Manifest(manifest.path).load()), {"PCUT002"})

    def test_a_claim_is_not_authority_over_someone_elses_book(self):
        """The blocker from review: a claim records what the campaign
        *intended* to write. If the run died before that landed and a user
        later put their own book under the same label, the contents will not
        match — and mismatching contents must not be treated as owned,
        because ownership is what authorises deletion."""
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            manifest = Manifest(os.path.join(tmp, "m.txt"))
            manifest.load()
            manifest.claim("PCUT007", [(500, 0xABCD)])
            claims = Manifest(manifest.path).load()
            somebody_elses = (1234, 0x9999)
            self.assertNotIn(somebody_elses, claims["PCUT007"])

    def test_a_replace_in_flight_may_hold_either_body(self):
        """An interrupted replace leaves the old body or the new one, and
        both are the campaign's — so a claim spanning a write records both,
        or the next run refuses to recognise its own book."""
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            manifest = Manifest(os.path.join(tmp, "m.txt"))
            manifest.load()
            old, new = (300, 0x1111), (500, 0x2222)
            manifest.claim("PCUT001", [new, old])
            claims = Manifest(manifest.path).load()
            self.assertIn(old, claims["PCUT001"])
            self.assertIn(new, claims["PCUT001"])
            # Once the cycle reads the result back, the pair collapses.
            manifest.claim("PCUT001", [old])
            self.assertEqual(Manifest(manifest.path).load()["PCUT001"], {old})

    def test_a_claim_must_name_what_it_may_contain(self):
        """An identity-less claim is a label owned but unrecognisable, so it
        could never be verified and never cleaned up. It also used to be
        written as a line the next load could not parse, which took out a
        run at cleanup."""
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            manifest = Manifest(os.path.join(tmp, "m.txt"))
            manifest.load()
            with self.assertRaises(AssertionError):
                manifest.claim("PCUT001", [None])
            with self.assertRaises(AssertionError):
                manifest.claim("PCUT001", [])
            # And the file is still readable, because nothing was written.
            self.assertEqual(Manifest(manifest.path).load(), {})

    def test_a_malformed_manifest_says_what_is_wrong(self):
        """The file is documented as something to inspect and sometimes
        edit, so a stray keystroke should not surface as an unpacking
        error from inside a comprehension."""
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "m.txt")
            for bad in (
                "PCUT001\t999\tdeadbeefdeadbeef\n",
                "PCUT001\tnonsense\n",
                "PCUT001\n",
                # Right length, not hexadecimal: used to reach int(x, 16)
                # and surface as a raw ValueError.
                "PCUT001\t123:zzzzzzzzzzzzzzzz\n",
            ):
                with open(path, "w", encoding="utf-8") as f:
                    f.write(bad)
                with self.assertRaises(AssertionError) as caught:
                    Manifest(path).load()
                self.assertIn(path, str(caught.exception))

    def test_manifest_claim_is_idempotent(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            manifest = Manifest(os.path.join(tmp, "m.txt"))
            manifest.load()
            manifest.claim("PCUT001", [(1, 2)])
            manifest.claim("PCUT001", [(1, 2)])
            self.assertEqual(Manifest(manifest.path).load(), {"PCUT001": {(1, 2)}})

    def test_identity_check_catches_same_length_corruption(self):
        """The collateral case: a book that is not the operation's target,
        still listed under its own name at its own size, whose bytes have
        changed. Directory-entry checks cannot see it; a readback can."""
        book = Book(True, "PCUT001.EPU", "PCUT001", 500)
        expected = {"PCUT001": (500, 0xAAAA)}
        corrupted = (500, 0xBBBB)
        self.assertNotIn(corrupted, {expected[book.label]})
        # And a set-valued expectation (a write in flight) still rejects it.
        self.assertNotIn(corrupted, {(500, 0xAAAA), (300, 0xCCCC)})

    def test_digest_matches_the_firmware_construction(self):
        """FNV-1a 64, same seed and prime as fw::powercut::digest_chunk.
        A drift here would make every readback disagree."""
        self.assertEqual(digest(b""), DIGEST_SEED)
        expected = ((DIGEST_SEED ^ 0x61) * DIGEST_PRIME) & DIGEST_MASK
        self.assertEqual(digest(b"a"), expected)
        self.assertNotEqual(digest(b"ab"), digest(b"ba"))

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
    # Which books on the card this tool owns. Ownership is recorded, never
    # inferred from a name, because acting on it means deleting.
    parser.add_argument("--manifest", default="/tmp/powercut_manifest.txt")
    parser.add_argument(
        "--adopt-unclaimed",
        action="store_true",
        help="take ownership of books named like this campaign that the manifest "
        "does not claim, and delete them. Only for a card you know is disposable.",
    )
    # Coverage floors below which a run is refused rather than reported as a
    # pass. Lower them for exploratory bench work, not for a gate.
    parser.add_argument("--min-cut-share", type=float, default=0.5)
    parser.add_argument("--min-replays", type=int, default=1)
    # How often the pre-existing library is read back byte for byte. It is a
    # full read of the card — ~4 minutes for a 184 MB shelf at the ~800 kB/s
    # an X3 manages — so per-cycle is opt-in rather than default.
    parser.add_argument(
        "--verify-baseline",
        choices=("ends", "always", "never"),
        default="ends",
        help="byte-check books that predate the campaign at the start and end of a "
        "run (ends, default), before and after every cut (always), or not at all "
        "(never, leaving only names and directory-entry sizes checked)",
    )
    args = parser.parse_args()
    Campaign(args).run()


if __name__ == "__main__":
    main()
