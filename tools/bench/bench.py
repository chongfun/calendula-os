#!/usr/bin/env python3
"""Development bench harness for CalendulaOS hardware runs.

The harness deliberately starts as a serial log collector and parser. The
firmware has no interactive command channel, so hardware suites are guided:
the host tells the operator what workflow to perform, captures structured
`bench:` lines, and writes JSONL for repeatable reporting.
"""

from __future__ import annotations

import argparse
import errno
import json
import os
import re
import select
import signal
import statistics
import struct
import subprocess
import sys
import termios
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterable, TextIO

try:
    import tomllib
except ImportError:  # pragma: no cover - Python < 3.11 fallback.
    try:
        # Third-party backport with the same API; optional, never required.
        import tomli as tomllib  # type: ignore[no-redef]
    except ImportError:
        tomllib = None  # type: ignore[assignment]

try:
    import fcntl
except ImportError:  # pragma: no cover - non-POSIX hosts cannot capture serial.
    fcntl = None  # type: ignore[assignment]


DEFAULT_PORT = "/dev/cu.usbmodem101"
DEFAULT_OUT = Path("target/bench/latest.jsonl")
DEFAULT_BUDGETS = Path(__file__).with_name("benches.toml")

LEGACY_RENDER_RE = re.compile(
    r"bench: render (?P<view>\w+) (?P<mode>\w+) page=(?P<page>\d+) "
    r"ch=(?P<ch>\d+) layout=(?P<layout>\d+)ms flush=(?P<flush>\d+)ms "
    r"prestage=(?P<prestage>\d+)ms t=(?P<t>\d+)"
)
LEGACY_INPUT_RE = re.compile(
    r"input: (?P<button>Some\((?P<some>\w+)\)|None) gpio0=(?P<aux>\d+) "
    r"gpio1=(?P<nav>\d+) gpio2=(?P<page>\d+) t=(?P<t>\d+)"
)
REFRESH_BUSY_RE = re.compile(r"display: refresh busy (?P<busy>\d+) ms")
STORAGE_OPEN_RE = re.compile(
    r"storage: open complete status=(?P<status>\w+) pages=(?P<pages>\d+) "
    r"chapters=(?P<chapters>\d+)"
)
# Printed once per boot, before esp_rtos::start, so it carries no t_ms; it
# marks the start of a boot's time base and says how the chip woke.
DEEP_SLEEP_WAKE_RE = re.compile(
    r"main: deep_sleep_wake=(?P<wake>true|false) "
    r"\(gpio=(?P<gpio>true|false), sleep_image=(?P<image>true|false)\)"
)
# Boot-stage lines that carry a t_ms so boot-to-first-paint can be attributed
# to a stage rather than just totalled. Each must fire at most once per boot,
# or its median describes nothing. `sd: session enter` is deliberately NOT
# here: several SD sessions run before first paint (catalog load, book open,
# cache build), so it contributed multiple samples from one boot and its
# "median" marked no repeatable point. Filtering to stages ahead of the first
# render does not fix that — they are all ahead of it.
BOOT_STAGE_RE = re.compile(
    r"(?P<stage>main: spawn \w+|display: started|display: x3 init done)"
    r" t_ms=(?P<t>\d+)$"
)


@dataclass(frozen=True)
class Suite:
    name: str
    guidance: str
    stop_event: str | None = None
    stop_count_arg: str | None = None


SUITES = {
    "page-turn": Suite(
        "page-turn",
        "Open a warmed SD book, then press Next for the requested turn count.",
        stop_event="reading_render",
        stop_count_arg="turns",
    ),
    "reader-soak": Suite(
        "reader-soak",
        "Run a normal reading workflow: page turns, chapter jumps, Home/Library returns, and a sleep/wake cycle.",
    ),
    "storage-cache": Suite(
        "storage-cache",
        "Exercise cold/warm catalog, book open, section extend, and progress-write paths.",
    ),
    "sleep-sync": Suite(
        "sleep-sync",
        "After several fast page turns, press Power or wait for idle sleep, then wake and repeat.",
        stop_event="sleep_complete",
        stop_count_arg="cycles",
    ),
    "thermal-run": Suite(
        "thermal-run",
        "Run the named underlying workflow while recording temperature/ambient notes in the run metadata.",
    ),
}


def main() -> int:
    parser = argparse.ArgumentParser(prog="bench")
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("list", help="list available suites")

    for name in SUITES:
        add_capture_parser(sub, name)

    stress = sub.add_parser("channel-stress", help="run host concurrency checks")
    stress.add_argument("--host", action="store_true", help="required; no hardware is used")
    stress.set_defaults(func=run_channel_stress)

    report = sub.add_parser("report", help="summarize one or more bench JSONL logs")
    report.add_argument("paths", nargs="+", type=Path)
    report.add_argument("--budgets", type=Path, default=DEFAULT_BUDGETS)
    report.add_argument("--strict", action="store_true", help="exit non-zero on budget warnings")
    report.add_argument(
        "--all",
        action="store_true",
        help="pool every run in the log instead of only the latest",
    )
    report.set_defaults(func=run_report)

    args = parser.parse_args()
    if args.command == "list":
        print_suites()
        return 0
    return args.func(args)


def add_capture_parser(sub: argparse._SubParsersAction[argparse.ArgumentParser], name: str) -> None:
    suite = SUITES[name]
    p = sub.add_parser(name, help=suite.guidance)
    p.add_argument("--port", default=DEFAULT_PORT)
    p.add_argument("--out", type=Path, default=DEFAULT_OUT)
    p.add_argument("--seconds", type=int, default=None, help="stop after this many seconds")
    p.add_argument(
        "--reset-before",
        action="store_true",
        help="hard-reset the ESP32-C3 with espflash before capture",
    )
    p.add_argument("--espflash", default="espflash", help="espflash executable")
    p.add_argument("--strict", action="store_true", help="exit non-zero on budget warnings")
    p.add_argument("--note", action="append", default=[], help="free-form note stored in metadata")
    p.add_argument("--book", default=None, help="operator label for the book under test")
    if name == "page-turn":
        p.add_argument("--turns", type=int, default=50)
    if name == "reader-soak":
        p.add_argument("--minutes", type=int, default=30)
    if name == "sleep-sync":
        p.add_argument("--cycles", type=int, default=10)
    if name == "storage-cache":
        p.add_argument("--cold", action="store_true")
        p.add_argument("--warm", action="store_true")
    if name == "thermal-run":
        p.add_argument("--suite", choices=["page-turn", "sleep-sync"], default="page-turn")
        p.add_argument("--minutes", type=int, default=45)
    p.set_defaults(func=run_capture)


def print_suites() -> None:
    for name, suite in sorted(SUITES.items()):
        print(f"{name:14} {suite.guidance}")
    print("channel-stress  Host-only interleaving checks for queue/coalescing rules.")
    print("report          Summarize captured JSONL logs.")


MAX_PENDING_PRESTAGE_LINES = 5
DEFAULT_PENDING_PRESTAGE_TIMEOUT_S = 2.0


def process_capture_stream(
    lines: Iterable[str],
    suite_name: str,
    stop_target: tuple[str, int] | None,
    out: TextIO | None = None,
    print_lines: bool = True,
    event_callback: Callable[[dict[str, Any]], None] | None = None,
    pending_prestage_timeout_s: float = DEFAULT_PENDING_PRESTAGE_TIMEOUT_S,
    on_deadline_set: Callable[[float], None] | None = None,
) -> dict[str, int]:
    counts: dict[str, int] = {}
    pending_prestage = False
    pending_prestage_deadline: float | None = None
    pending_prestage_lines = 0

    for line in lines:
        if line != "":
            if print_lines:
                sys.stdout.write(line)
                sys.stdout.flush()
            parsed_events = parse_line(line, suite_name)
            for event in parsed_events:
                if out is not None:
                    write_event(out, event)
                if event_callback is not None:
                    event_callback(event)
                for counter in event_counters(event):
                    counts[counter] = counts.get(counter, 0) + 1
        else:
            parsed_events = []

        if pending_prestage:
            has_prestage = any(e.get("event") == "prestage" for e in parsed_events)
            has_new_turn_or_render = any(e.get("event") in {"render", "input"} for e in parsed_events)
            if line != "":
                pending_prestage_lines += 1
            expired = pending_prestage_deadline is not None and time.monotonic() >= pending_prestage_deadline
            if has_prestage or has_new_turn_or_render or pending_prestage_lines >= MAX_PENDING_PRESTAGE_LINES or expired:
                break
        elif stop_target and counts.get(stop_target[0], 0) >= stop_target[1]:
            if stop_target[0] == "reading_render":
                already_prestaged = any(
                    e.get("event") == "render" and isinstance(e.get("prestage_ms"), int)
                    for e in parsed_events
                )
                if not already_prestaged:
                    pending_prestage = True
                    deadline = time.monotonic() + pending_prestage_timeout_s
                    pending_prestage_deadline = deadline
                    if on_deadline_set is not None:
                        on_deadline_set(deadline)
                    pending_prestage_lines = 0
                else:
                    break
            else:
                break

    return counts


def run_capture(args: argparse.Namespace) -> int:
    suite = SUITES[args.command]
    seconds = capture_seconds(args)
    stop_target = stop_target_for(args, suite)
    args.out.parent.mkdir(parents=True, exist_ok=True)

    workflow = capture_workflow(args, suite)
    print(f"bench {suite.name}: {suite.guidance}")
    if workflow != suite.name:
        # What the report will hold this run to, said before it starts.
        print(f"workflow: {workflow} (budgets and signal checks follow it)")
    print(f"port: {args.port}")
    print(f"out:  {args.out}")
    if seconds:
        print(f"stop: after {seconds}s")
    elif stop_target:
        print(f"stop: after {stop_target[1]} parsed {stop_target[0]} events")
    else:
        print("stop: Ctrl-C")
    if args.note:
        print("notes:", "; ".join(args.note))
    if args.reset_before:
        print("reset: hard-reset before capture")

    metadata = {
        "suite": suite.name,
        "workflow": workflow,
        "event": "run_start",
        "host_time": time.time(),
        "port": args.port,
        "notes": args.note,
        "book": getattr(args, "book", None),
        "reset_before": bool(args.reset_before),
    }
    counts: dict[str, int] = {}
    started = time.monotonic()
    stop_at = started + seconds if seconds else None
    pending_deadline: float | None = None

    def get_stop_at() -> float | None:
        if stop_at is not None and pending_deadline is not None:
            return min(stop_at, pending_deadline)
        return pending_deadline if pending_deadline is not None else stop_at

    def set_pending_deadline(deadline: float) -> None:
        nonlocal pending_deadline
        pending_deadline = deadline

    with args.out.open("a", encoding="utf-8") as out:
        write_event(out, metadata)
        try:
            if args.reset_before:
                reset_device(args.espflash, args.port)
            counts = process_capture_stream(
                capture_lines(args.port, stop_at=stop_at, get_stop_at=get_stop_at),
                suite.name,
                stop_target,
                out=out,
                print_lines=True,
                on_deadline_set=set_pending_deadline,
            )
        except KeyboardInterrupt:
            print("\nbench: capture stopped")
        finally:
            write_event(
                out,
                {
                    "suite": suite.name,
                    "event": "run_end",
                    "host_time": time.time(),
                    "elapsed_s": round(time.monotonic() - started, 3),
                    "counts": counts,
                },
            )
    report_warnings = summarize_paths(
        [args.out],
        DEFAULT_BUDGETS,
        validate_suites=args.strict,
    )
    return 1 if args.strict and report_warnings else 0


def capture_workflow(args: argparse.Namespace, suite: Suite) -> str:
    """The workflow a capture actually ran, which is not always its suite.

    `thermal-run` is a condition, not a workload: `--suite` picks which of the
    other workflows runs under it. That choice decided what telemetry the run
    would produce and was then thrown away, so every thermal run looked alike
    to the report — a `--suite sleep-sync` capture was gated by the page-turn
    budgets and never asked for a sleep at all, which is the "selected and
    unchecked" shape this harness exists to catch.
    """
    underlying = getattr(args, "suite", None)
    return str(underlying) if isinstance(underlying, str) else suite.name


def reset_device(espflash: str, port: str) -> None:
    command = [
        espflash,
        "reset",
        "--chip",
        "esp32c3",
        "--port",
        port,
        "--non-interactive",
        "--after",
        "hard-reset",
    ]
    subprocess.run(command, check=True)


def event_counters(event: dict[str, Any]) -> list[str]:
    event_name = str(event.get("event", ""))
    counters = [event_name]
    if event_name == "render" and event.get("view") == "Reading":
        counters.append("reading_render")
    if event_name == "sleep" and event.get("phase") == "complete":
        counters.append("sleep_complete")
    if event_name == "input" and event.get("button") in {"Next", "Previous"}:
        counters.append("page_input")
    return counters


def capture_seconds(args: argparse.Namespace) -> int | None:
    if args.seconds is not None:
        return args.seconds
    if args.command in {"reader-soak", "thermal-run"}:
        return int(args.minutes) * 60
    return None


def stop_target_for(args: argparse.Namespace, suite: Suite) -> tuple[str, int] | None:
    if suite.stop_event is None or suite.stop_count_arg is None:
        return None
    return suite.stop_event, int(getattr(args, suite.stop_count_arg))


# Errno values a vanishing USB-serial device raises: the ESP32-C3's
# USB-JTAG port drops off the bus when the firmware enters deep sleep
# (idle timeout, the sleep-cycle suites). macOS reports ENXIO ("Device
# not configured"), Linux EIO or ENODEV; ENOENT covers a reopen racing
# re-enumeration.
PORT_LOST_ERRNOS = {errno.ENXIO, errno.EIO, errno.ENODEV, errno.ENOENT}


def capture_lines(
    port: str,
    stop_at: float | None = None,
    get_stop_at: Callable[[], float | None] | None = None,
) -> Iterable[str]:
    """`serial_lines`, surviving the device dropping off the bus.

    Deep sleep mid-capture is expected — an idle timeout or a
    sleep-cycle suite kills the USB-JTAG port — so rather than dying
    with a traceback, announce the loss, wait for the port to
    re-enumerate (waking the device is the operator's job), and resume
    until the capture window closes. A port that never produced a line
    still fails fast, so a mistyped --port errors immediately.
    """
    connected = False
    reconnecting = False
    while True:
        if reconnecting:
            effective_stop_at = get_stop_at() if get_stop_at is not None else stop_at
            if effective_stop_at is not None and time.monotonic() >= effective_stop_at:
                print("port: capture window ended while the device was away", flush=True)
                return
            if not os.path.exists(port):
                time.sleep(0.5)
                continue
            # Let enumeration settle before reopening the fresh device node.
            time.sleep(0.5)

        try:
            for line in serial_lines(port, stop_at=stop_at, get_stop_at=get_stop_at):
                if line == "":
                    if reconnecting:
                        print("port: back; resuming capture", flush=True)
                        reconnecting = False
                    continue
                connected = True
                yield line
        except OSError as err:
            if not connected or err.errno not in PORT_LOST_ERRNOS:
                raise
            if not reconnecting:
                print(f"port: {port} vanished (device asleep?); wake it to resume capture", flush=True)
                reconnecting = True
        else:
            return


def serial_lines(
    port: str,
    stop_at: float | None = None,
    get_stop_at: Callable[[], float | None] | None = None,
) -> Iterable[str]:
    if fcntl is None:
        raise RuntimeError("serial capture requires POSIX fcntl support")
    fd = os.open(port, os.O_RDONLY | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        attrs = termios.tcgetattr(fd)
        attrs[0] = 0
        attrs[1] = 0
        attrs[2] = termios.CREAD | termios.CLOCAL | termios.CS8
        attrs[3] = 0
        termios.tcsetattr(fd, termios.TCSANOW, attrs)
        fcntl.ioctl(fd, termios.TIOCMBIS, struct.pack("i", termios.TIOCM_DTR))
        fcntl.ioctl(fd, termios.TIOCMBIC, struct.pack("i", termios.TIOCM_RTS))
        yield ""
        buf = b""
        while True:
            effective_stop_at = get_stop_at() if get_stop_at is not None else stop_at
            timeout = 0.2
            if effective_stop_at is not None:
                remaining = effective_stop_at - time.monotonic()
                if remaining <= 0:
                    return
                timeout = min(timeout, remaining)
            ready, _, _ = select.select([fd], [], [], timeout)
            if not ready:
                continue
            chunk = os.read(fd, 4096)
            if not chunk:
                raise OSError(errno.EIO, "EOF on serial port")
            buf += chunk
            while b"\n" in buf:
                raw, buf = buf.split(b"\n", 1)
                yield raw.decode("utf-8", errors="replace") + "\n"
    finally:
        os.close(fd)


def parse_line(line: str, suite: str = "unknown") -> list[dict[str, Any]]:
    text = line.strip()

    match = LEGACY_RENDER_RE.match(text)
    if match:
        data = match.groupdict()
        return [
            {
                "suite": suite,
                "event": "render",
                "view": data["view"],
                "mode": data["mode"],
                "page": int(data["page"]),
                "chapter": int(data["ch"]),
                "layout_ms": int(data["layout"]),
                "flush_ms": int(data["flush"]),
                "prestage_ms": int(data["prestage"]),
                "t_ms": int(data["t"]),
                "legacy": True,
            }
        ]

    if text.startswith("bench: "):
        return [parse_bench_line(text, suite)]

    match = LEGACY_INPUT_RE.match(text)
    if match:
        button = match.group("some") or "None"
        return [
            {
                "suite": suite,
                "event": "input",
                "button": button,
                "aux": int(match.group("aux")),
                "nav": int(match.group("nav")),
                "page_raw": int(match.group("page")),
                "t_ms": int(match.group("t")),
                "legacy": True,
            }
        ]

    match = REFRESH_BUSY_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "refresh",
                "busy_ms": int(match.group("busy")),
                "legacy": True,
            }
        ]

    if text == "display: sleep deep":
        return [{"suite": suite, "event": "sleep", "phase": "deep", "legacy": True}]
    if text == "display: sleep framebuffer flush failed":
        return [{"suite": suite, "event": "sleep", "phase": "refresh", "ok": False}]
    if "queue full" in text or "panicked at" in text or "watchdog" in text.lower():
        return [{"suite": suite, "event": "warning", "line": text}]

    match = STORAGE_OPEN_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "storage_open",
                "status": match.group("status"),
                "pages": int(match.group("pages")),
                "chapters": int(match.group("chapters")),
                "legacy": True,
            }
        ]

    match = DEEP_SLEEP_WAKE_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "boot",
                "deep_sleep_wake": match.group("wake") == "true",
                "gpio": match.group("gpio") == "true",
                "sleep_image": match.group("image") == "true",
            }
        ]

    match = BOOT_STAGE_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "boot_stage",
                "stage": match.group("stage"),
                "t_ms": int(match.group("t")),
            }
        ]
    return []


def parse_bench_line(text: str, suite: str) -> dict[str, Any]:
    body = text.removeprefix("bench: ").strip()
    if not body:
        return {"suite": suite, "event": "unknown", "line": text}
    parts = body.split()
    event = parts[0].replace("-", "_")
    result: dict[str, Any] = {"suite": suite, "event": event}
    for part in parts[1:]:
        if "=" not in part:
            result.setdefault("tokens", []).append(part)
            continue
        key, raw = part.split("=", 1)
        result[key] = parse_value(raw)
    return result


def parse_value(raw: str) -> Any:
    value = raw.strip().rstrip(",")
    if value.startswith("Some(") and value.endswith(")"):
        return value[5:-1]
    if value.endswith("ms") and value[:-2].isdigit():
        return int(value[:-2])
    if value in {"true", "false"}:
        return value == "true"
    if value in {"ok", "fail"}:
        return value == "ok"
    if re.fullmatch(r"-?\d+", value):
        return int(value)
    try:
        return float(value)
    except ValueError:
        return value


def write_event(out: Any, event: dict[str, Any]) -> None:
    out.write(json.dumps(event, sort_keys=True, separators=(",", ":")) + "\n")
    out.flush()


def run_report(args: argparse.Namespace) -> int:
    report_warnings = summarize_paths(
        args.paths,
        args.budgets,
        validate_suites=args.strict,
        latest_only=not getattr(args, "all", False),
    )
    return 1 if args.strict and report_warnings else 0


def split_runs(events: list[dict[str, Any]]) -> list[list[dict[str, Any]]]:
    """Splits a pooled event stream at its run_start markers.

    Captures append to the same log, so a file usually holds several
    runs; events before the first marker (hand-assembled logs) form
    their own leading segment.
    """
    runs: list[list[dict[str, Any]]] = [[]]
    for event in events:
        if event.get("event") == "run_start" and runs[-1]:
            runs.append([])
        runs[-1].append(event)
    return [run for run in runs if run]


def summarize_paths(
    paths: list[Path],
    budgets_path: Path | None = None,
    *,
    validate_suites: bool = False,
    latest_only: bool = True,
) -> list[str]:
    events: list[dict[str, Any]] = []
    for path in paths:
        file_events = read_events(path)
        if events and file_events and file_events[0].get("event") != "run_start":
            # Two files are two captures, but the event stream was simply
            # concatenated, so a log that opens without a `run_start` — every
            # capture predating the marker, and every hand-built one — had no
            # boundary and was swallowed by whichever run preceded it. Its
            # samples then inherited that run's suite and its device time base,
            # both wrong, and both decided by the order of the paths on the
            # command line. The marker is synthesized in memory only; it
            # carries no suite, so the run stays unlabelled and says so.
            events.append({"event": "run_start", "file_boundary": str(path)})
        events.extend(file_events)

    # `validate_suites` is the --strict flag. A strict gate that cannot load
    # its budgets must fail loudly: exiting 0 with the checks silently absent
    # is exactly the Python-3.9 tomllib hole this guards against.
    budgets, budgets_problem = load_budgets(budgets_path)
    if budgets_problem is not None:
        if validate_suites:
            raise SystemExit(
                f"bench report: --strict cannot enforce budgets: {budgets_problem}"
            )
        print(f"bench report: warning: budgets not checked: {budgets_problem}")

    if not events:
        print("bench report: no events")
        return ["no events parsed"] if validate_suites else []

    runs = split_runs(events)
    if latest_only and len(runs) > 1:
        events = runs[-1]
        start = next((e for e in events if e.get("event") == "run_start"), {})
        print(
            f"bench report: latest run only ({start.get('suite', 'unknown')}; "
            f"{len(runs) - 1} earlier run(s) in the log — pass --all to pool)"
        )
    # Boot detection and t_ms monotonicity are per run: pooled runs restart
    # the device time base at every run_start, which is not a reset.
    scoped_runs = runs[-1:] if latest_only else runs
    boot_paints, boot_stages, time_warnings = boot_report(scoped_runs)

    renders = [event for event in events if event.get("event") == "render"]
    reading_renders = [event for event in renders if event.get("view") == "Reading"]
    refreshes = [event for event in events if event.get("event") == "refresh"]
    sleeps = [event for event in events if event.get("event") == "sleep"]
    warnings = [event for event in events if event.get("event") == "warning"]
    storage = [event for event in events if str(event.get("event", "")).startswith("storage")]

    print("\nbench report")
    # Synthesized file boundaries are structure, not telemetry; counting them
    # would report more events than the logs hold.
    print(f"events:        {sum(1 for e in events if 'file_boundary' not in e)}")
    print(f"renders:       {len(renders)}")
    print(f"storage:       {len(storage)}")
    print(f"sleeps:        {len(sleeps)}")
    print(f"warnings:      {len(warnings)}")
    print_duration("reading layout", values(reading_renders, "layout_ms"))
    turn_stats = page_turn_stats_over_epochs(events)
    if turn_stats.median_trusted or not turn_stats.durations:
        print_duration("page turn", turn_stats.durations)
    else:
        print(
            f"page turn      UNTRUSTED: {turn_stats.untrusted_presses}/"
            f"{turn_stats.presses} presses produced no page turn "
            f"({turn_stats.coalesced_presses} coalesced, "
            f"{turn_stats.unmatched_presses} unanswered; over "
            f"{PAGE_TURN_UNTRUSTED_MAX_FRACTION:.0%}); median suppressed — "
            "the distribution is operator cadence, not firmware latency"
        )
    if turn_stats.presses:
        print(
            f"page inputs:   presses={turn_stats.presses} "
            f"page_turns={len(turn_stats.durations)} "
            f"nav={turn_stats.nav_answered} "
            f"coalesced={turn_stats.coalesced_presses} "
            f"unmatched={turn_stats.unmatched_presses} "
            f"reading_renders={turn_stats.reading_renders}"
        )
    print_duration("render flush", values(renders, "flush_ms"))
    print_duration("prestage", prestage_values(events))
    print_duration("refresh busy", values(refreshes, "busy_ms"))
    print_duration(
        "storage open",
        values([event for event in events if event.get("event") == "storage_open"], "elapsed_ms"),
    )
    print_duration(
        "catalog load",
        values(
            [
                event
                for event in events
                if event.get("event") == "storage_catalog" and event.get("action") == "load"
            ],
            "elapsed_ms",
        ),
    )
    print_duration(
        "catalog scan",
        values(
            [
                event
                for event in events
                if event.get("event") == "storage_catalog" and event.get("action") == "scan"
            ],
            "elapsed_ms",
        ),
    )
    print_duration(
        "progress write",
        values(
            [
                event
                for event in events
                if event.get("event") == "storage_progress" and event.get("elapsed_ms") is not None
            ],
            "elapsed_ms",
        ),
    )
    builds = [event for event in events if event.get("event") == "storage_build"]
    print_duration("storage build", values(builds, "elapsed_ms"))
    print_duration("build spine", values(builds, "spine_ms"))
    print_duration("build write", values(builds, "write_ms"))
    if builds:
        totals = {
            key: sum(int(event[key]) for event in builds if isinstance(event.get(key), int))
            for key in ("rd_calls", "rd_blocks", "wr_calls", "wr_blocks")
        }
        print(
            f"build io:      builds={len(builds)} "
            + " ".join(f"{key}={value}" for key, value in totals.items())
        )
    print_duration(
        "first page",
        values(
            [event for event in events if event.get("event") == "storage_first_page"],
            "elapsed_ms",
        ),
    )
    print_duration(
        "bg build",
        values(
            [event for event in events if event.get("event") == "storage_background_build"],
            "elapsed_ms",
        ),
    )
    # One line per boot kind, never a pooled median: a cold boot pays the full
    # waveform and a wake does not, so their mixture is a number matching no
    # boot that ever happened — and the mixing ratio is decided by the suite
    # (sleep-sync is ~1 cold to 20 wakes, --reset-before is all cold).
    for kind in sorted(boot_paints):
        print_duration(f"boot to paint ({kind})", boot_paints[kind])
    if boot_paints:
        print(
            "boots:         "
            + " ".join(
                f"{kind}={len(boot_paints[kind])}" for kind in sorted(boot_paints)
            )
            + " (first render t_ms per witnessed boot; reset = unexplained reboot)"
        )
    if boot_stages:
        print("boot stages:   (median t_ms across witnessed boots)")
        for stage, samples in sorted(boot_stages.items(), key=lambda kv: statistics.median(kv[1])):
            print(f"  {stage:24} {statistics.median(samples):.0f}ms (n={len(samples)})")

    modes: dict[str, int] = {}
    for event in renders:
        mode = str(event.get("mode", "unknown"))
        modes[mode] = modes.get(mode, 0) + 1
    if modes:
        print("refresh modes: " + ", ".join(f"{k}={v}" for k, v in sorted(modes.items())))
    if warnings:
        print("warning lines:")
        for event in warnings[:10]:
            print(f"  {event.get('line', event)}")
    budget_warnings = evaluate_budgets(events, budgets)
    suite_warnings = evaluate_suite_signals(events) if validate_suites else []
    if time_warnings:
        print("time warnings:")
        for warning in time_warnings:
            print(f"  {warning}")
    if budget_warnings:
        print("budget warnings:")
        for warning in budget_warnings:
            print(f"  {warning}")
    if suite_warnings:
        print("suite warnings:")
        for warning in suite_warnings:
            print(f"  {warning}")
    return time_warnings + budget_warnings + suite_warnings


def read_events(path: Path) -> list[dict[str, Any]]:
    result = []
    with path.open(encoding="utf-8") as handle:
        for line_no, line in enumerate(handle, 1):
            line = line.strip()
            if not line:
                continue
            try:
                result.append(json.loads(line))
            except json.JSONDecodeError as err:
                raise SystemExit(f"{path}:{line_no}: invalid JSONL: {err}") from err
    return result


def values(events: list[dict[str, Any]], key: str) -> list[int]:
    return [int(event[key]) for event in events if isinstance(event.get(key), int)]


def refresh_busy_values(events: list[dict[str, Any]], mode: str) -> list[int]:
    return values(
        [
            event
            for event in events
            if event.get("event") == "refresh" and event.get("mode") == mode
        ],
        "busy_ms",
    )


def print_duration(label: str, data: list[int]) -> None:
    if not data:
        return
    median = statistics.median(data)
    p95 = percentile(data, 95)
    print(f"{label:14} median={median:.0f}ms p95={p95:.0f}ms min={min(data)}ms max={max(data)}ms")


def prestage_values(events: list[dict[str, Any]]) -> list[int]:
    """Prestage durations, from either or both firmware event shapes.

    Firmware from 2026-07-27 emits prestage as its own event, after the
    render event that ends press-to-settled. Older firmware folded
    `prestage_ms` into the render line, which is also why its `page turn`
    figures run ~24 ms long — the render timestamp was printed after the
    prestage rather than at the settle. Captures from before that change
    still summarize, but do not compare their `page turn` against a newer
    run's.
    """
    result: list[int] = []
    for event in events:
        if event.get("event") == "prestage" and isinstance(event.get("elapsed_ms"), int):
            result.append(int(event["elapsed_ms"]))
        elif event.get("event") == "render" and isinstance(event.get("prestage_ms"), int):
            result.append(int(event["prestage_ms"]))
    return result


# Fraction of page presses that may fail to produce a page-turn sample before
# the median is untrustworthy. Two ways a press produces no sample, and both
# mean the same thing — the capture is recording tapping rhythm rather than
# firmware latency:
#
#   unmatched  no render ever answered it (the firmware logs every debounced
#              press whether or not the reader acts on it, and the input
#              channel drops the oldest event on overflow).
#   coalesced  a newer press superseded it before any render began. The app
#              coalesces input while a render is in flight, so N presses
#              during one refresh produce one frame showing the latest state.
#              The superseded presses never had a frame of their own and
#              their "latency" is just how fast the operator tapped.
#
# A deliberate-cadence capture (one press per settled page) leaves at most a
# press or two in either bucket — a final press after the last captured
# render, say — so ~2-4% of a 50-turn run. 10% keeps headroom over that
# boundary noise while still catching a burst or a dropped-input episode.
PAGE_TURN_UNTRUSTED_MAX_FRACTION = 0.10


@dataclass(frozen=True)
class PageTurnStats:
    """Press-to-settled pairing, with the bookkeeping needed to trust it."""

    durations: list[int]
    presses: int
    reading_renders: int
    # Presses whose answering render was not a Reading render: Library/Home
    # navigation, accounted for but not page turns.
    nav_answered: int
    # Presses superseded by a newer press before any render began. See the
    # constant above — these are the burst signal, and the reason the median
    # cannot be trusted on unmatched count alone.
    coalesced_presses: int

    @property
    def unmatched_presses(self) -> int:
        return (
            self.presses
            - len(self.durations)
            - self.nav_answered
            - self.coalesced_presses
        )

    @property
    def untrusted_presses(self) -> int:
        """Presses that produced no page-turn sample, for either reason."""
        return self.unmatched_presses + self.coalesced_presses

    @property
    def untrusted_fraction(self) -> float:
        if self.presses == 0:
            return 0.0
        return self.untrusted_presses / self.presses

    @property
    def median_trusted(self) -> bool:
        return bool(self.durations) and (
            self.untrusted_fraction <= PAGE_TURN_UNTRUSTED_MAX_FRACTION
        )


def render_begin_ms(event: dict[str, Any]) -> int:
    """When the reader state this render displays stopped being able to change.

    A press later than this cannot be reflected in the frame, so it must not
    be credited to it. `req_ms` is that boundary, stamped by the app as it
    freezes the request. `deq_ms`, when present, is the later instant the
    display task dequeued that request; it is reported for diagnosis and
    never used for pairing.

    Older captures have no `req_ms` and fall back to `t_ms - layout_ms -
    flush_ms`. That estimate is deliberately *not* trusted where the real
    stamp exists, because it omits everything outside those two intervals —
    the catalog and TOC window reads ahead of layout, and after the flush the
    planner record, a 52 KB framebuffer copy, and a chapter-tracking read that
    touches the card whenever the chapter changes. Each omission pushes the
    estimate later than the truth, which is the direction that mispairs: a
    press inside one of those windows looks earlier than the render's start
    and gets charged to it, worst at chapter boundaries. Treat page-turn
    minima from pre-`req_ms` logs as suspect for that reason.
    """
    # `req_ms` is stamped by the app as it freezes the request, so it is the
    # instant the frame's state stopped being able to change. It is deliberately
    # not the display task's dequeue time: a render can wait in the channel
    # behind a flush, a prestage, a storage command or a background build step,
    # and a press arriving during that wait belongs to the next frame, not this
    # one. (`deq_ms`, when present, is the dequeue instant; `deq_ms - req_ms` is
    # the queue wait, reported for diagnosis and never used for pairing.)
    # 0 means unstamped — see `RenderRequest::requested_at_ms`.
    req_ms = event.get("req_ms")
    if isinstance(req_ms, int) and req_ms > 0:
        return req_ms
    t_ms = int(event["t_ms"])
    layout_ms = event.get("layout_ms")
    flush_ms = event.get("flush_ms")
    if isinstance(layout_ms, int) and isinstance(flush_ms, int):
        return t_ms - layout_ms - flush_ms
    return t_ms


def page_turn_stats(events: list[dict[str, Any]]) -> PageTurnStats:
    """Press-to-settled durations, robust to renders the press did not cause.

    Each press is credited with the first render that *began* after it
    (the display task coalesces queued input into the latest reader state,
    so every press older than a render's begin time is reflected in it).
    A press that lands while a render is already in flight is NOT credited
    with the remainder of that render — it waits for the next one. This is
    what makes the statistic a function of firmware latency rather than
    tapping rhythm: a repaint or a storage-driven re-render begins after the
    press it follows was already answered and therefore credits nothing,
    where the old render-driven FIFO charged it to the *next* press and
    produced impossible 2 ms samples next to multi-second stale-press ones.

    A render yields at most one duration, measured from the newest press it
    answers. Older presses it also clears were superseded before it began —
    the app coalesced them into this one frame — so they are counted as
    `coalesced_presses` rather than given a duration of their own. Charging
    them would reintroduce the defect from the other side: a burst would
    produce a long sample per superseded press and still report every press
    as "matched". Presses answered by a non-Reading render are navigation,
    counted but excluded from durations.
    """
    pending_inputs: list[int] = []
    durations: list[int] = []
    presses = 0
    reading_renders = 0
    nav_answered = 0
    coalesced_presses = 0
    for event in sorted(events, key=event_sort_key):
        name = event.get("event")
        if name == "input" and event.get("button") in {"Next", "Previous"}:
            t_ms = event.get("t_ms")
            if isinstance(t_ms, int):
                presses += 1
                pending_inputs.append(t_ms)
        elif name == "render":
            t_ms = event.get("t_ms")
            if not isinstance(t_ms, int):
                continue
            is_reading = event.get("view") == "Reading"
            if is_reading:
                reading_renders += 1
            begin = render_begin_ms(event)
            # Events are sorted by t_ms, so pending_inputs is nondecreasing
            # and this pops exactly the presses this render answers.
            newest_answered: int | None = None
            answered = 0
            while pending_inputs and pending_inputs[0] <= begin:
                newest_answered = pending_inputs.pop(0)
                answered += 1
            if newest_answered is None:
                continue
            coalesced_presses += answered - 1
            if is_reading:
                durations.append(t_ms - newest_answered)
            else:
                nav_answered += 1
    return PageTurnStats(
        durations, presses, reading_renders, nav_answered, coalesced_presses
    )


def page_turn_stats_over_epochs(events: list[dict[str, Any]]) -> PageTurnStats:
    """Pair presses to renders within each device time epoch, then merge.

    `t_ms` is device uptime. It restarts at every reboot, and separate
    captures in one log each carry their own. `page_turn_stats` sorts by it,
    so handing it events that span an epoch boundary interleaves two clocks:
    two healthy runs that each recorded `input @1000, render @1500` sort as
    two inputs then two renders, and the first render answers both — one
    invented coalesced press, one render credited with nothing, and with
    other overlaps, durations measured from one run's press to another run's
    render.

    `boot_report` documents this hazard and guards against it; page-turn
    pairing did not, which left `--all` and any single capture containing a
    sleep or a reset (so `sleep-sync` and `reader-soak` by construction)
    reporting cross-epoch samples. Splitting by run *and* by boot segment is
    the same decomposition the boot report already computes.
    """
    return merge_page_turn_stats(
        [
            page_turn_stats(segment)
            for run in split_runs(events)
            for segment, _kind in boot_segments(run)[0]
        ]
    )


def merge_page_turn_stats(parts: list[PageTurnStats]) -> PageTurnStats:
    """Sum independently paired populations into one.

    Used both for the boot segments inside a run and for the runs inside a
    pooled report: the counters add and the durations concatenate, so a
    pooled figure is exactly its parts and coverage can be judged per part
    without measuring anything twice.
    """
    merged = PageTurnStats([], 0, 0, 0, 0)
    for stats in parts:
        merged = PageTurnStats(
            merged.durations + stats.durations,
            merged.presses + stats.presses,
            merged.reading_renders + stats.reading_renders,
            merged.nav_answered + stats.nav_answered,
            merged.coalesced_presses + stats.coalesced_presses,
        )
    return merged


def page_turn_durations(events: list[dict[str, Any]]) -> list[int]:
    return page_turn_stats_over_epochs(events).durations


def event_sort_key(event: dict[str, Any]) -> tuple[int, float]:
    t_ms = event.get("t_ms")
    if isinstance(t_ms, int):
        return (0, float(t_ms))
    host_time = event.get("host_time")
    if isinstance(host_time, (int, float)):
        return (1, float(host_time) * 1000)
    return (2, 0)


# `t_ms` is device uptime, so within one boot it only grows; a decrease in
# capture (file) order marks a reboot. A reboot right after a completed deep
# sleep is the normal wake path. Any other one is a watchdog/brownout/manual
# reset, and it matters because `event_sort_key` orders events by t_ms while
# `split_runs` splits only on host-side run_start markers: the two boots'
# time bases interleave and press/render pairings straddle the reset.
_SLEEP_TERMINAL_PHASES = {"deep", "deep_sleep", "complete"}

# A t_ms decrease smaller than this is treated as clock skew, not a reboot.
# `bench: input` is stamped and printed from the interrupt-priority executor
# and can preempt the display task between its `Instant::now()` and its
# blocking UART write, so two lines can land out of order by a hair. A real
# reboot restarts the uptime clock, so its regression is the whole elapsed
# session — orders of magnitude above this. Without the guard a 1 ms
# inversion fabricates a boot, a boot-to-first-paint sample, and a --strict
# failure; no inversion of any size appears in the reference capture, so this
# guards a hazard rather than an observed defect.
_BOOT_REGRESSION_SKEW_MS = 250


def boot_segments(
    run: list[dict[str, Any]],
) -> tuple[list[tuple[list[dict[str, Any]], str]], list[str]]:
    """Split one run's events (kept in capture order) at device reboots.

    Returns ``([(segment, kind)], warnings)``. The leading segment's kind is
    "attach" (capture joined an already-running device; its t_ms values do
    not date from a witnessed boot). Later segments start at a reboot:
    "wake" when the boot marker reports a real deep-sleep wake (or, with no
    marker at all, a completed deep sleep preceded it), "cold" when the
    marker says otherwise, and "reset" for a bare t_ms regression - an
    unexplained reboot, which is also returned as a warning.
    """
    segments: list[list[Any]] = [[[], "attach"]]
    warnings: list[str] = []
    last_t: int | None = None
    slept = False
    slept_at: int | None = None
    for event in run:
        name = str(event.get("event", ""))
        if name == "boot":
            # Explicit marker line: the first print of a new boot. The
            # firmware already decided this — `deep_sleep_wake` is
            # `gpio && sleep_image`, because a wake pin that fired is only
            # half the story: a wake whose sleep image was not retained pays
            # the full waveform, so its first paint belongs with the cold
            # cluster. Take that verdict as given. A preceding sleep must not
            # overrule it: a device that browns out or resets after a
            # completed sleep reports false here, and reading it as a wake
            # files a full cold boot in the fast cluster. The heuristic is
            # kept only for markers too old to carry the fields.
            wake = event.get("deep_sleep_wake")
            gpio = event.get("gpio")
            if not isinstance(wake, bool) and isinstance(gpio, bool):
                # A marker printed before the combined field existed: the
                # same rule, spelled out from its two halves.
                wake = gpio and event.get("sleep_image") is not False
            if isinstance(wake, bool):
                boot_kind = "wake" if wake else "cold"
            else:
                # No marker fields at all — only then is a preceding sleep
                # the best evidence available.
                boot_kind = "wake" if slept else "cold"
            if segments[-1][0]:
                segments.append([[], boot_kind])
            else:
                segments[-1][1] = boot_kind
            slept = False
            slept_at = None
            last_t = None
        t_ms = event.get("t_ms")
        if isinstance(t_ms, int):
            if last_t is not None and last_t - t_ms > _BOOT_REGRESSION_SKEW_MS:
                kind = "wake" if slept else "reset"
                if kind == "reset":
                    warnings.append(
                        f"t_ms went backwards ({last_t} -> {t_ms}) with no deep sleep "
                        "recorded: unexpected reset (watchdog/brownout?); timings "
                        "straddle two boots"
                    )
                segments.append([[], kind])
                slept = False
                slept_at = None
            elif slept and slept_at is not None and t_ms > slept_at:
                # The device went on running past a sleep it reported as
                # terminal, so it did not deep-sleep: the panel handshake
                # failed and the power task stayed awake to retry. Anything
                # that follows is not a wake, and a reset after it must still
                # be reported as one.
                slept = False
                slept_at = None
            last_t = t_ms
        if (
            name == "sleep"
            and event.get("phase") in _SLEEP_TERMINAL_PHASES
            # `phase=complete` is printed on both outcomes, carrying the
            # result in `ok`. Absent (older captures) is treated as success.
            and event.get("ok") is not False
        ):
            slept = True
            slept_at = t_ms if isinstance(t_ms, int) else None
        segments[-1][0].append(event)
    return [(segment, kind) for segment, kind in segments if segment], warnings


def boot_report(
    scoped_runs: list[list[dict[str, Any]]],
) -> tuple[dict[str, list[int]], dict[str, list[int]], list[str]]:
    """Boot-to-first-paint per witnessed boot, plus boot-stage samples.

    The t_ms epoch is the esp-hal systimer (embassy_time resolves to
    esp_rtos::now() -> Instant::now().duration_since_epoch()), which starts
    a little before esp_rtos::start. Neither epoch includes the ROM and
    second-stage bootloader, so "boot to paint" is firmware-visible boot
    time and not wall-clock time from the button press. The t_ms of a boot's
    first render is that figure - but only when the capture actually witnessed
    the boot (a reset_before run start, a boot marker line, a wake, or a
    t_ms regression). An "attach" segment joined mid-session, and its first
    render says nothing about boot. Note a wake whose first-paint line was
    lost while the serial port re-enumerated can leave an inflated sample;
    it shows up as an outlier against the boot-time cluster.
    """
    paints: dict[str, list[int]] = {}
    stages: dict[str, list[int]] = {}
    warnings: list[str] = []
    for run in scoped_runs:
        run_start = next((e for e in run if e.get("event") == "run_start"), {})
        segments, segment_warnings = boot_segments(run)
        warnings.extend(segment_warnings)
        for index, (segment, kind) in enumerate(segments):
            if index == 0 and kind == "attach" and run_start.get("reset_before"):
                # `--reset-before` hard-resets after the capture opens, so
                # the run's leading segment is a witnessed cold boot.
                kind = "cold"
            if kind == "attach":
                continue
            first_render_t: int | None = None
            for event in segment:
                if event.get("event") == "render" and isinstance(event.get("t_ms"), int):
                    first_render_t = int(event["t_ms"])
                    break
            if first_render_t is None:
                continue
            paints.setdefault(kind, []).append(first_render_t)
            for event in segment:
                if event.get("event") == "boot_stage" and isinstance(event.get("t_ms"), int):
                    t_ms = int(event["t_ms"])
                    if t_ms <= first_render_t:
                        stages.setdefault(str(event.get("stage")), []).append(t_ms)
    return paints, stages, warnings


def load_budgets(path: Path | None) -> tuple[dict[str, Any], str | None]:
    """Returns (budgets, problem).

    ``problem`` is a human-readable reason the budgets could not be loaded,
    and ``budgets`` is empty whenever it is set. A ``None`` path means the
    caller intentionally disabled budgets, which is not a problem. Budgets
    silently absent is how ``--strict`` spent months verifying nothing on
    Python 3.9, so every involuntary empty result must carry its reason.
    """
    if path is None:
        return {}, None
    if tomllib is None:
        version = "%d.%d.%d" % sys.version_info[:3]
        return {}, (
            f"cannot parse {path}: tomllib needs Python >= 3.11 (this is "
            f"{version}); re-run under a newer python3 or `pip install tomli`"
        )
    if not path.exists():
        return {}, f"budgets file {path} does not exist"
    with path.open("rb") as handle:
        return tomllib.load(handle), None


def evaluate_budgets(events: list[dict[str, Any]], budgets: dict[str, Any]) -> list[str]:
    in_play, warnings = budget_sections_in_play(events, budgets)
    page_turn = budgets.get("page-turn", {})
    if page_turn and "page-turn" in in_play:
        runs = section_runs(events, "page-turn")
        per_run_turns = [page_turn_stats_over_epochs(run.events) for run in runs]
        turn_stats = merge_page_turn_stats(per_run_turns)
        if turn_stats.durations and not turn_stats.median_trusted:
            # A gate over a cadence artifact is worse than no gate: refuse
            # to compare the median rather than pass or fail on noise.
            warnings.append(
                f"page-turn median untrusted: {turn_stats.untrusted_presses}/"
                f"{turn_stats.presses} presses produced no page turn (over "
                f"{PAGE_TURN_UNTRUSTED_MAX_FRACTION:.0%}); budget not checked"
            )
        else:
            turn_median = (
                statistics.median(turn_stats.durations) if turn_stats.durations else None
            )
            warn_if_above(
                warnings,
                "page-turn median",
                turn_median,
                page_turn.get("median_press_to_settled_ms"),
            )
            warn_if_below(
                warnings,
                "page-turn median",
                turn_median,
                page_turn.get("median_press_to_settled_min_ms"),
            )
            for key in ("median_press_to_settled_ms", "median_press_to_settled_min_ms"):
                warn_if_unobserved(
                    warnings, "page-turn", page_turn, key, runs,
                    [stats.durations for stats in per_run_turns],
                    "press-to-settled pairings",
                )
        per_run_layout, reading_layout = per_run_samples(
            runs,
            lambda run_events: values(
                [
                    event
                    for event in run_events
                    if event.get("event") == "render" and event.get("view") == "Reading"
                ],
                "layout_ms",
            ),
        )
        warn_if_above(
            warnings,
            "Reading layout p95",
            percentile(reading_layout, 95) if reading_layout else None,
            page_turn.get("reading_layout_warn_ms"),
        )
        warn_if_unobserved(
            warnings, "page-turn", page_turn, "reading_layout_warn_ms",
            runs, per_run_layout, "Reading renders",
        )
        per_run_prestage, prestage = per_run_samples(runs, prestage_values)
        warn_if_above(
            warnings,
            "prestage p95",
            percentile(prestage, 95) if prestage else None,
            page_turn.get("prestage_warn_ms"),
        )
        warn_if_unobserved(
            warnings, "page-turn", page_turn, "prestage_warn_ms",
            runs, per_run_prestage, "prestage samples",
        )
        per_run_fast, fast_busy = per_run_samples(
            runs, lambda run_events: refresh_busy_values(run_events, "Fast")
        )
        warn_if_above(
            warnings,
            "Fast refresh busy p95",
            percentile(fast_busy, 95) if fast_busy else None,
            page_turn.get("fast_refresh_busy_warn_ms"),
        )
        warn_if_unobserved(
            warnings, "page-turn", page_turn, "fast_refresh_busy_warn_ms",
            runs, per_run_fast, "Fast refresh events",
        )

    sleep_sync = budgets.get("sleep-sync", {})
    if sleep_sync and "sleep-sync" in in_play:
        runs = section_runs(events, "sleep-sync")
        per_run_full, full_busy = per_run_samples(
            runs, lambda run_events: refresh_busy_values(run_events, "Full")
        )
        min_ms = sleep_sync.get("full_refresh_busy_min_ms")
        max_ms = sleep_sync.get("full_refresh_busy_max_ms")
        for busy in full_busy:
            if isinstance(min_ms, int) and busy < min_ms:
                warnings.append(f"Full refresh busy {busy}ms below budget floor {min_ms}ms")
            if isinstance(max_ms, int) and busy > max_ms:
                warnings.append(f"Full refresh busy {busy}ms above budget ceiling {max_ms}ms")
        failed_sleeps = [
            event
            for run in runs
            for event in run.events
            if event.get("event") == "sleep" and event.get("ok") is False
        ]
        if failed_sleeps:
            warnings.append(f"{len(failed_sleeps)} failed sleep phase(s)")
        for key in ("full_refresh_busy_min_ms", "full_refresh_busy_max_ms"):
            warn_if_unobserved(
                warnings, "sleep-sync", sleep_sync, key,
                runs, per_run_full, "Full refresh events",
            )
    storage_cache = budgets.get("storage-cache", {})
    if storage_cache and "storage-cache" in in_play:
        runs = section_runs(events, "storage-cache")
        per_run_open, storage_open = per_run_samples(
            runs,
            lambda run_events: values(
                [event for event in run_events if event.get("event") == "storage_open"],
                "elapsed_ms",
            ),
        )
        warn_if_above(
            warnings,
            "storage open p95",
            percentile(storage_open, 95) if storage_open else None,
            storage_cache.get("warm_book_open_warn_ms"),
        )
        warn_if_unobserved(
            warnings, "storage-cache", storage_cache, "warm_book_open_warn_ms",
            runs, per_run_open, "storage_open events",
        )
        per_run_catalog, catalog_load = per_run_samples(
            runs,
            lambda run_events: values(
                [
                    event
                    for event in run_events
                    if event.get("event") == "storage_catalog"
                    and event.get("action") == "load"
                ],
                "elapsed_ms",
            ),
        )
        warn_if_above(
            warnings,
            "catalog load p95",
            percentile(catalog_load, 95) if catalog_load else None,
            storage_cache.get("catalog_load_warn_ms"),
        )
        warn_if_unobserved(
            warnings, "storage-cache", storage_cache, "catalog_load_warn_ms",
            runs, per_run_catalog, "catalog load events",
        )
    return warnings


def evaluate_suite_signals(events: list[dict[str, Any]]) -> list[str]:
    """The telemetry each capture owes, checked against what it produced.

    Resolved by workflow rather than by suite: a `thermal-run --suite
    sleep-sync` capture owes sleep telemetry, and asking it only for the
    refresh events every thermal run has is how a deliberately selected
    workflow passed `--strict` without its own signal ever being looked for.

    Checked per run, not per workflow. Runs of one workflow used to be
    concatenated before the check, so two sleep-sync captures — one holding
    only a Full refresh, one only a completed sleep — answered for each other
    and both passed while neither was a complete capture. Runs are named
    `suite/workflow`, and by position when the log holds more than one, so a
    warning says which capture it came from.
    """
    warnings: list[str] = []
    for run in labelled_runs(events):
        workflow = run.workflow
        label = run.label
        signal_events = [
            event
            for event in run.events
            if event.get("event") not in {"run_start", "run_end"}
        ]
        if not signal_events:
            warnings.append(f"{label}: no parsed bench telemetry")
            continue
        event_names = {str(event.get("event")) for event in signal_events}
        if "warning" in event_names:
            warnings.append(f"{label}: warning events present")
        if run.suite == "thermal-run" and "refresh" not in event_names:
            # Ambient investigations live on refresh timing whatever workflow
            # they ran under.
            warnings.append(f"{label}: no refresh timing telemetry captured")
        if workflow == "page-turn":
            turn_stats = page_turn_stats_over_epochs(run.events)
            if not turn_stats.durations:
                warnings.append(f"{label}: no input-to-Reading-render duration captured")
            elif not turn_stats.median_trusted:
                warnings.append(
                    f"{label}: {turn_stats.untrusted_presses}/{turn_stats.presses} "
                    "presses produced no page turn; the press-to-settled figure "
                    "is cadence, not firmware — recapture at one press per "
                    "settled page"
                )
        elif workflow == "storage-cache":
            if not any(name.startswith("storage") for name in event_names):
                warnings.append(f"{label}: no storage telemetry captured")
        elif workflow == "sleep-sync":
            if not any(event.get("event") == "sleep" for event in signal_events):
                warnings.append(f"{label}: no sleep telemetry captured")
            if any(event.get("event") == "sleep" and event.get("ok") is False for event in signal_events):
                warnings.append(f"{label}: failed sleep phase captured")
        elif workflow == "reader-soak":
            if not {"render", "input"}.issubset(event_names):
                warnings.append(f"{label}: expected both input and render telemetry")
        elif workflow == "thermal-run":
            # No workflow recorded, so there is nothing to check beyond the
            # refresh timing above; say so rather than imply it was gated.
            warnings.append(
                f"{label}: no workflow recorded, so no workflow signal was "
                "checked; recapture with a current bench.py"
            )
        elif workflow is None:
            warnings.append(
                f"{label}: no suite label, so nothing workflow-specific was "
                "checked; report this log on its own"
            )
        else:
            # A misspelling, or a workflow newer than this bench.py. Either
            # way nothing here knows what the run owed, and silence would
            # read as a pass — the fail-closed direction is to say so.
            warnings.append(
                f"{label}: unrecognised workflow, so nothing workflow-specific "
                f"was checked; known workflows are {', '.join(sorted(SUITES))}"
            )
    return warnings


def warn_if_unobserved(
    warnings: list[str],
    section: str,
    budgets: dict[str, Any],
    key: str,
    runs: list[LabelledRun],
    per_run: list[list[int]],
    what: str,
) -> None:
    """Report a budget that is configured but had nothing to measure.

    `warn_if_above` and `warn_if_below` return silently when the value is
    `None`, which is right for an exploratory report and wrong for a gate:
    a run missing the telemetry a budget covers passed `--strict` while
    enforcing nothing, which is the same "configured but not protecting
    anything" shape this harness exists to stop. A budget with zero samples
    is now a warning, so a non-strict report says so and `--strict` fails.

    Checked per capture, not per pool. `--all` reports several runs together,
    and a run that never produced the telemetry a budget covers is not
    excused by a sibling that did: two sleep-sync captures, one holding only
    a Full refresh and the other only a completed sleep, between them
    satisfied every check while neither was a complete capture. Each run is
    named so the incomplete one can be found.

    Callers only reach this for a section whose workflow the log actually
    contains, so a page-turn capture is never faulted for holding no storage
    telemetry.
    """
    if budgets.get(key) is None:
        return
    for run, samples in zip(runs, per_run):
        if samples:
            continue
        warnings.append(
            f"[{section}] {key} is configured but nothing was measured against "
            f"it: no {what} in {run.label}"
        )


# Which workflows produce the measurements each budget section gates. A
# section is named after the workflow that owns it, but `reader-soak` is page
# turns plus navigation, so it answers to the page-turn budgets too. Nothing
# else is folded in: reader-soak sleeps once, so pointing it at `sleep-sync`
# would fault a healthy capture for a Full refresh it never owed.
#
# Keyed on workflow rather than suite so `thermal-run --suite sleep-sync`
# resolves to the sleep-sync budgets, which is the whole point of the flag.
# The map is also the isolation boundary — a section measures only the
# workflows listed for it, so pooling with `--all` cannot let one capture's
# samples decide another's verdict.
BUDGET_SECTION_WORKFLOWS: dict[str, set[str]] = {
    "page-turn": {"page-turn", "reader-soak"},
    "sleep-sync": {"sleep-sync"},
    "storage-cache": {"storage-cache"},
}


@dataclass(frozen=True)
class LabelledRun:
    events: list[dict[str, Any]]
    suite: str | None
    workflow: str | None
    # How a warning names this run. Carries its position in the log whenever
    # there is more than one, because "no Fast refresh events" is only
    # actionable if the operator can tell which capture is missing them.
    label: str


def labelled_runs(events: list[dict[str, Any]]) -> list[LabelledRun]:
    """Each run with the suite and workflow it was captured under.

    Both are properties of the run, not of the event: `run_start` is the only
    line that carries `workflow`, and hand-built fixtures and older captures
    put even `suite` only there. Every event therefore takes its run's labels
    rather than being dropped from every section for want of its own.

    A run with no recorded `workflow` falls back to its suite name. For
    `thermal-run` that resolves to nothing a budget section claims, and is
    reported — the alternative is assuming the argparse default and gating a
    `--suite sleep-sync` capture as if it were a page-turn run, which is the
    guess that made this worth fixing.
    """
    split = split_runs(events)
    runs = []
    for index, run in enumerate(split):
        suite = next(
            (str(event["suite"]) for event in run if isinstance(event.get("suite"), str)),
            None,
        )
        start = next((event for event in run if event.get("event") == "run_start"), {})
        workflow = start.get("workflow")
        workflow = str(workflow) if isinstance(workflow, str) else suite
        name = workflow or "unlabelled"
        if suite is not None and suite != workflow:
            name = f"{suite}/{name}"
        label = name if len(split) == 1 else f"{name} run {index + 1} of {len(split)}"
        runs.append(LabelledRun(run, suite, workflow, label))
    return runs


def section_runs(events: list[dict[str, Any]], section: str) -> list[LabelledRun]:
    """The runs a budget section is allowed to measure.

    Choosing *which* sections to evaluate was never enough: the measurements
    themselves were taken from the whole pooled log, so a file holding a
    `page-turn` run next to a `storage-cache` or `sleep-sync` one had its
    strict verdict decided partly by samples from a workflow that budget does
    not describe — a false pass or a false failure, either way from the wrong
    population.

    Returned as runs rather than a flat event list because coverage is a
    property of each capture too: pool first and ask "were there any
    samples?" afterwards, and one healthy run answers for a sibling that
    produced nothing. Every caller pools for its statistic and checks
    coverage per run — see `per_run_samples` and `warn_if_unobserved`.

    A log with no label anywhere measures everything: there is nothing better
    to go on. Once *any* run is labelled the unlabelled ones are left out
    rather than folded in, and `budget_sections_in_play` reports the mixture.
    """
    workflows = BUDGET_SECTION_WORKFLOWS.get(section, {section})
    runs = labelled_runs(events)
    if all(run.workflow is None for run in runs):
        return runs
    return [run for run in runs if run.workflow in workflows]


def per_run_samples(
    runs: list[LabelledRun],
    extract: Callable[[list[dict[str, Any]]], list[int]],
) -> tuple[list[list[int]], list[int]]:
    """Each run's samples, and the pooled list built out of them.

    Coverage reads the first, the percentile or median reads the second.
    Deriving the pool from the parts rather than measuring the concatenated
    stream keeps the two from ever disagreeing about what was sampled.
    """
    per_run = [extract(run.events) for run in runs]
    return per_run, [sample for samples in per_run for sample in samples]


def budget_sections_in_play(
    events: list[dict[str, Any]], budgets: dict[str, Any]
) -> tuple[set[str], list[str]]:
    """Budget sections whose workflow this log actually contains, plus warnings.

    Every section used to be evaluated against every log, which was harmless
    while an absent metric silently skipped its check and is not once that
    absence is a warning. Logs whose runs carry no label at all — hand-built
    fixtures, and any capture predating suite tagging — keep the old
    behaviour of evaluating everything, since there is nothing better to go
    on.

    Two things are reported rather than passed over, because both mean a
    capture ran and nothing gated it, which is the silently-unenforced shape
    this file exists to prevent: a known workflow no configured section
    claims, and an unlabelled run pooled with labelled ones — the latter is
    excluded from every section, and reading it into whichever run happened
    to precede it in the file list would be worse than saying so.
    """
    if not budgets:
        # Budgets deliberately disabled, or unloadable — reported elsewhere.
        return set(), []
    per_run = [run.workflow for run in labelled_runs(events)]
    labelled = {label for label in per_run if label is not None}
    if not labelled:
        return set(budgets), []
    sections = {
        name
        for name in budgets
        if labelled & BUDGET_SECTION_WORKFLOWS.get(name, {name})
    }
    warnings = []
    for workflow in sorted(labelled):
        if any(
            workflow in BUDGET_SECTION_WORKFLOWS.get(section, {section})
            for section in budgets
        ):
            continue
        # Every unclaimed workflow, not only the ones this bench.py knows: a
        # misspelled or newer name matches no section either, and skipping
        # the warning for it let malformed metadata buy silence.
        unknown = "" if workflow in SUITES else " (not a workflow this bench.py knows)"
        warnings.append(
            f"workflow {workflow} has no budget section to check it against{unknown}"
        )
    unlabelled = sum(1 for label in per_run if label is None)
    if unlabelled:
        warnings.append(
            f"{unlabelled} of {len(per_run)} runs carry no suite label and are "
            "outside every budget section; report those logs on their own"
        )
    return sections, warnings


def warn_if_above(
    warnings: list[str],
    label: str,
    actual: float | None,
    threshold: Any,
) -> None:
    if actual is None or not isinstance(threshold, int):
        return
    if actual > threshold:
        warnings.append(f"{label} {actual:.0f}ms above warning budget {threshold}ms")


def warn_if_below(
    warnings: list[str],
    label: str,
    actual: float | None,
    threshold: Any,
) -> None:
    if actual is None or not isinstance(threshold, int):
        return
    if actual < threshold:
        warnings.append(
            f"{label} {actual:.0f}ms below plausibility floor {threshold}ms "
            "(burst cadence or dropped presses, not a faster device)"
        )


def percentile(data: list[int], pct: int) -> float:
    if len(data) == 1:
        return float(data[0])
    ordered = sorted(data)
    index = (len(ordered) - 1) * pct / 100
    lower = int(index)
    upper = min(lower + 1, len(ordered) - 1)
    fraction = index - lower
    return ordered[lower] * (1 - fraction) + ordered[upper] * fraction


def run_channel_stress(args: argparse.Namespace) -> int:
    if not args.host:
        raise SystemExit("channel-stress currently requires --host")
    model = ChannelStressModel()
    model.run()
    print("channel-stress: ok")
    for check in model.checks:
        print(f"  {check}")
    return 0


class ChannelStressModel:
    """Tiny host model for the firmware's coalescing contract."""

    def __init__(self) -> None:
        self.rendering = False
        self.render_pending = False
        self.state_page = 0
        self.rendered_pages: list[int] = []
        self.pending_storage: int | None = None
        self.latest_request_id = 0
        self.reader_section_loaded = False
        self.loading_plate_painted = False
        self.checks: list[str] = []

    def run(self) -> None:
        self.input_page_turn()
        self.input_page_turn()
        self.input_page_turn()
        assert self.rendering
        assert self.render_pending
        self.display_settled()
        assert self.rendering
        assert not self.render_pending
        self.display_settled()
        assert self.rendered_pages == [1, 3]
        self.checks.append("input during render coalesces to latest reader state")

        stale = self.open_request()
        fresh = self.open_request()
        assert stale < fresh == self.latest_request_id
        assert not self.storage_request_is_current(stale)
        assert self.storage_request_is_current(fresh)
        self.checks.append("stale open/extend requests are rejected by request id")

        self.storage_wins_first_open()
        assert self.loading_plate_painted
        self.checks.append("storage-first cold book open paints a loading plate")

        self.pending_storage = 1
        self.sleep()
        assert not self.rendering
        assert not self.render_pending
        self.checks.append("sleep clears render in-flight and pending-render state")

        refused = {"OpenBook", "ExtendSection", "LoadChapters", "JumpChapter", "LoadCatalogCache"}
        admitted = {"StoreProgress", "StoreWifiCredentials", "ReceiveUpload"}
        assert all(not sync_loaned_admits(command) for command in refused)
        assert all(sync_loaned_admits(command) for command in admitted)
        self.checks.append("sync session admits only progress, credentials, and upload after loan")

    def input_page_turn(self) -> None:
        self.state_page += 1
        if self.rendering:
            self.render_pending = True
        else:
            self.rendering = True
            self.rendered_pages.append(self.state_page)

    def display_settled(self) -> None:
        self.rendering = False
        if self.render_pending:
            self.render_pending = False
            self.rendering = True
            self.rendered_pages.append(self.state_page)

    def open_request(self) -> int:
        self.latest_request_id += 1
        return self.latest_request_id

    def storage_request_is_current(self, request_id: int) -> bool:
        return request_id == self.latest_request_id

    def storage_wins_first_open(self) -> None:
        self.loading_plate_painted = False
        self.reader_section_loaded = False
        if not self.reader_section_loaded:
            self.loading_plate_painted = True
        self.reader_section_loaded = True

    def sleep(self) -> None:
        self.rendering = False
        self.render_pending = False


def sync_loaned_admits(command: str) -> bool:
    return command in {"StoreProgress", "StoreWifiCredentials", "ReceiveUpload"}


if __name__ == "__main__":
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    raise SystemExit(main())
