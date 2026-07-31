# CalendulaOS bench harness

`tools/bench/bench.py` captures hardware serial logs, parses structured
`bench:` telemetry, and summarizes development bench runs. It does not control
the reader over serial; current hardware suites are guided workflows.

## Common runs

```sh
tools/bench/bench.py channel-stress --host
tools/bench/bench.py page-turn --port /dev/cu.usbmodem101 --turns 50
tools/bench/bench.py page-turn --port /dev/cu.usbmodem101 --reset-before --seconds 20
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 10
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --reset-before --seconds 20 --strict
tools/bench/bench.py report target/bench/latest.jsonl
tools/bench/bench.py report --strict target/bench/latest.jsonl
```

Longer release/risky-merge runs:

```sh
tools/bench/bench.py reader-soak --port /dev/cu.usbmodem101 --minutes 30
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --cold --warm
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 20
```

## Output

Raw serial remains visible in the terminal. Parsed records are appended as
JSONL under `target/bench/` by default:

```json
{"event":"render","flush_ms":405,"layout_ms":13,"mode":"Fast","page":42,"suite":"page-turn"}
{"event":"prestage","staged":true,"elapsed_ms":24,"suite":"page-turn"}
```

The `render` event is emitted at the settle — the moment `DisplayEvent::Settled`
goes out and the glass is done — so the `page turn` figure it anchors is true
press-to-settled. The RED/DTM1 prestage that runs afterwards is real work on the
display task and still gates the next command, but the reader does not wait on
it, so it is its own event (the capture loop consumes this paired prestage record
after reaching the requested reading render count). Firmware before 2026-07-27
printed one combined render line after the prestage; `report` still summarizes
those logs, but their `page turn` runs ~24 ms long and must not be compared
against a newer run's.

Do not commit run logs. Keep only parser code, suite docs, and stable budgets in
the repo.

`--reset-before` uses `espflash reset` before opening the raw serial capture.
This is useful for boot, catalog-cache, and sleep/wake smoke runs because it
does not rely on catching a manual button press at the right moment.

`report --strict` exits non-zero when checked-in warning budgets are exceeded or
when the selected suite did not capture its expected signal, such as storage
telemetry for `storage-cache` or input-to-Reading-render timing for
`page-turn`. Capture commands also accept `--strict`, applying the same gate to
the log they just wrote.

**Budget checking needs Python >= 3.11 (`tomllib`) or the `tomli` package.**
Capture and plain reporting run on any Python 3.9+, but `--strict` refuses to
run without a TOML parser — macOS system `python3` is 3.9, and a strict gate
that silently checks nothing is how a 16.7x budget overrun once passed clean.
Non-strict reports print a warning when budgets could not be loaded.

The `page turn` statistic is guarded against operator cadence: the report
prints a `page inputs:` line (presses / matched / unmatched), suppresses the
median when more than 10% of presses went unrendered (burst pressing strands
presses in the FIFO pairing and charges later renders to stale presses), and
budgets give the median a plausibility floor as well as a ceiling. The report
also warns when `t_ms` goes backwards without a completed deep sleep — an
unexplained mid-capture reset interleaves two boot time bases.

`report` additionally summarizes cache-build telemetry (`storage build` with
its spine/write split and read/write block totals, `first page`, `bg build`)
and, for captures that witnessed a boot (`--reset-before`, a wake, or a
reset), `boot to paint` — the `t_ms` of the boot's first render, i.e.
boot-to-first-paint — with per-stage medians when the firmware's stamped
boot-stage lines are present.

## When to use

- Use `channel-stress --host` during normal development when touching reader
  state, display command, storage command, sync session, refresh plan, or
  queue/coalescing behavior.
- Use short `page-turn` and `sleep-sync` runs before trusting a flashed firmware
  after display, input, sleep, reader rendering, SD session, section cache, or
  progress-write changes.
- Use `reader-soak`, `storage-cache`, and longer `sleep-sync` runs before
  releases or risky merges.
- Use `thermal-run` for targeted refresh, ghosting, sleep-screen, enclosure,
  power, SD-card, or ambient-temperature investigations.
