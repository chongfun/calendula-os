# Bench workflow

Use `tools/bench/bench.py` for hardware-facing development benches. The
emulator and golden frames remain the fast behavior oracle; bench runs answer
board-specific timing, SD/cache, sleep, and soak questions.

## When to run

- Run `tools/bench/bench.py channel-stress --host` during normal development
  when changing reader state, display command, storage command, sync session,
  refresh plan, or queue/coalescing behavior. This needs no hardware.
- Run short hardware confidence checks before trusting a flashed firmware after
  display flush, input debounce, sleep/power, reader rendering, SD session,
  section cache, or progress-write changes:

```sh
tools/bench/bench.py page-turn --port /dev/cu.usbmodem101 --turns 50
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 5
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --reset-before --seconds 20 --strict
```

- Run longer hardware checks before releases or risky merges:

```sh
tools/bench/bench.py reader-soak --port /dev/cu.usbmodem101 --minutes 30
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --cold --warm
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 20
```

- Run `thermal-run` only for targeted refresh, ghosting, sleep-screen,
  enclosure, power, SD-card, or ambient-temperature investigations.
- **A capture is held to what you asked it for.** `run_start` records the
  request — seconds, turns, cycles, storage modes — and `run_end` records how
  the capture ended (`stop_reason`) and whether that was a stop condition
  anyone asked for (`completed`). `--strict` checks the run against its own
  request, so a `--cycles 10` run interrupted after one cycle, a `--minutes
  30` soak stopped at 95 seconds, and a log truncated before its `run_end` all
  fail instead of passing on the strength of having produced *some* expected
  telemetry. Ctrl-C completes a capture that asked for no other stop condition
  and cuts short one that did. Captures predating this carry no completion
  status and are reported as unverified rather than assumed complete.
- **A count and a duration are not both minimums.** `page-turn` and
  `sleep-sync` always carry a count target, and `--seconds` is offered by
  every capture command, so for those two suites the count is the contract
  and `--seconds` is a ceiling — an unattended capture's way out, not a window
  it owes. `page-turn --seconds 60` is held to its 50 turns; if the deadline
  cuts it short, the report says it is short of its *samples* rather than
  faulting it for a duration it never requested. The startup banner names both
  stop conditions and says which one is the contract. Suites with no count
  (`reader-soak`, `thermal-run`, `storage-cache`) keep the duration as theirs.
- **Durations and counts must be positive.** `--seconds 0`, `--minutes 0`,
  `--turns 0` and `--cycles 0` are rejected at the command line; zero used to
  disable the deadline and capture forever. Omit the flag to capture without
  that limit.
- **`--reset-before` is setup, not telemetry.** The capture window opens once
  the reset has returned, so `--reset-before --seconds 20` collects twenty
  seconds of telemetry rather than twenty seconds minus espflash and device
  re-enumeration. `run_end` carries both: `elapsed_s` is the telemetry window
  a requested duration is checked against, `command_elapsed_s` the whole
  command.
- `reader-soak` is a passive capture: the operator runs the described
  reading workflow on the device by hand while bench.py records. Menus
  idle-sleep after 3 minutes (Reading after 10), so keep interacting. **Do
  the sleep/wake cycle, and do it inside the capture** — `--strict` asks for
  a completed sleep with a wake later in the same run, because that path is
  the part of the workflow nothing else exercises and a soak without it is a
  page-turn run wearing another name. Waking the device to *start* the
  capture does not count; sleep, wake, and keep reading. A failed sleep
  phase fails the run even if a later cycle completed.
- **`page-turn` is operator-driven too.** bench.py only listens; a human
  presses Next until the requested turn count lands. The count is *paired
  turns*, not Reading renders: an unprompted repaint no longer eats one of
  them, `run_start` records what you asked for, and `--strict` says so if
  the capture came home short. **Still capture at
  deliberate cadence — one press per fully settled page** — but the
  statistic now defends itself, and the report tells you when it could not:

  - Each press is credited with the first render whose request was frozen
    after it — `req_ms`, stamped by the app as it builds the request, not
    when the display task dequeues it. A render can wait in the channel
    behind a flush, a prestage, a storage command or a background build
    step, and a press arriving during that wait belongs to the next frame.
    (`deq_ms` is the dequeue instant; `deq_ms - req_ms` is that queue wait,
    reported for diagnosis and never used for pairing.) So a press landing
    mid-render is no longer charged the remainder of a frame it did not
    cause. This is what used to produce 2 ms
    durations. Captures older than `req_ms` fall back to
    `t_ms - layout_ms - flush_ms`, which runs *late* because it omits the
    catalog and TOC reads before layout and the chapter-tracking read after
    the flush; treat their page-turn minima as suspect.
  - Pairing happens **within each run and each boot**, never across them.
    `t_ms` is device uptime, so it restarts at every reboot and in every
    capture; sorting a pooled log by it interleaves clocks and can measure
    from one run's press to another run's render.
  - A render yields at most one duration, from the newest press it answers.
    Presses a newer press superseded before any render began are reported as
    `coalesced` — the app coalesces input while a refresh is in flight, so
    those presses never had a frame of their own.
  - The `page inputs:` line accounts for every press: `page_turns`, `nav`,
    `coalesced`, `unmatched`. When more than 10% produced no page turn the
    median is **suppressed** rather than printed, and `--strict` fails
    instead of gating on noise. A `median_press_to_settled_min_ms` floor
    catches an implausibly fast median from the other side.
  - Trust is judged **per capture, before the runs are pooled**. A pooled
    report names every run it left out of the median — one whose cadence
    failed the test, or one whose presses produced no pairing at all — and
    does not average it into the others, where a 1-turn, 50%-untrusted run
    disappears behind a clean 20-turn one. If no run is left, the median is
    suppressed. The `page inputs:` line still counts every press, including
    the excluded runs'.

  `layout_ms`, `flush_ms`, `busy_ms`, and prestage remain per-render and safe
  to read from any cadence. The history is why this matters: a 354 ms median
  recorded at burst cadence went into the optimization roadmap as a baseline,
  could never be reconciled with a 408 ms flush, and cost a later change a
  phantom 94 ms "regression". A subsequent capture reported a median of
  477 ms alongside a 2 ms minimum and an 88,670 ms maximum, all marked
  trusted — the median was right and the tails were fiction.
- **`storage-cache --cold` and `--warm` are checked, not decorative.**
  bench.py only listens, so the flags cannot steer the device; they declare
  which paths the run will exercise, get recorded in `run_start`, and
  `--strict` fails if the capture never took one of them. Cold is shown by a
  book open that had to build its cache or by a catalog scan that *succeeded*;
  warm by an open served from an already-built cache or from the loaded RAM
  window, or by a catalog loaded from its snapshot. Passing neither flag is an
  unrestricted operator-guided capture and requires no particular path. Until
  2026-07-31 the flags were read once by argparse and never written down or
  checked, so `--cold --warm --strict` passed without proving either path.
- **A failed storage operation is not evidence, and not a sample.** The scan
  line carries its own `ok`, stamped before the firmware's UI fallback can
  replace a failed scan's `Error` status with `Ready` — that fallback keeps
  the reader on an older in-memory catalog, which is right for the reader and
  made the marker read as a success, so a failed scan could evidence the cold
  path. Failed scans and loads are excluded from mode evidence and from the
  `catalog_load_warn_ms` population, counted on their own report line, and a
  `storage-cache` run containing one fails `--strict` the way a failed sleep
  phase fails a sleep suite. A capture predating the scan's `ok` field cannot
  evidence the failure either way and is read as it always was.
- **Not every cache build belongs to an open.** A background walk's last step
  publishes through the same path, emitting `storage_build` with no open in
  flight, and is announced as `storage_background_build` immediately after.
  The announcement consumes the pending build, so the next ordinary open —
  possibly minutes later — is still classified as warm. Without that it was
  filed as cold: a real warm sample lost to the budget, a requested `--warm`
  path reported missing, and a 72 ms open described as a 14-64 second one.
- **A book open is reported per path, never pooled.** `storage open (ram)`,
  `(warm)` and `(cold)` are three different pieces of work — 0-15 ms, 57-95 ms
  and 14-64 *seconds* on this repo's captures — so a pooled percentile
  describes none of them. `warm_book_open_warn_ms` measures the warm
  population alone: an open that read the card with no cache build inside the
  same transaction. It used to be computed over every `storage_open`, where a
  deliberately cold open failed the *warm* ceiling and a RAM hit pulled the
  percentile back under it. Cold opens scale with book size by construction
  and are reported without a budget.
- **A malformed budget file is a configuration error.** Sections, key
  spelling, and value types are validated against `BUDGET_SCHEMA` in bench.py
  before any capture is read, so a typo like `median_press_to_settledd_ms`
  fails the load instead of silently leaving page-turn with no operative
  latency threshold. Empty sections, unknown keys, strings and booleans are
  rejected the same way (`isinstance(True, int)` holds in Python, so a bool
  would otherwise have reached the comparison as 1). So are values that are
  the right type and still gate nothing: a negative millisecond threshold, and
  a floor above its own ceiling — `full_refresh_busy_min_ms` over
  `full_refresh_busy_max_ms`, or the two `median_press_to_settled` bounds
  inverted — which no measurement can satisfy. `--strict` refuses to run; a
  non-strict report says the budgets were not checked. Adding a budget to
  `benches.toml` means adding it to the schema and reading it somewhere.
- **A budget with nothing to measure is now a warning**, not a silent pass.
  If a run does not produce the telemetry a configured budget covers — a
  page-turn capture with no refresh events, say — the report says so and
  `--strict` fails, because a budget that gates nothing is exactly the
  failure this harness exists to stop. Either capture the missing telemetry
  or delete the key. Only a section whose suite the log contains is checked,
  so a page-turn capture is never faulted for holding no storage telemetry.
- **Coverage is judged per capture; the statistic is still pooled.** Under
  `--all` a run that produced none of a budget's telemetry is not excused by
  a sibling run that did — two sleep-sync captures, one holding only a Full
  refresh and one only a completed sleep, used to satisfy every check
  between them while neither was complete. The same goes for the signal
  checks. Warnings name the run (`sleep-sync run 2 of 3`) so the incomplete
  capture can be found. Medians and percentiles are still taken across every
  run the section owns.
- **A sleep-sync capture owes a *completed* sleep.** `phase=requested` opens
  the transition and `refresh`/`power_down_*` are steps inside it that a
  failed handshake reaches too; only `phase=complete ok=true` (or, in logs
  old enough to predate it, the X3 driver's `phase=deep_sleep`) says the
  panel went down. A capture holding only a request now fails `--strict`
  instead of satisfying the check with its Full refresh. *Counting* cycles for
  `--cycles N` is narrower still: only `phase=complete` with `ok` not false,
  which is both what ends the capture and what the report checks it against.
  A failed completion no longer ends the capture as though a cycle landed, and
  the X3's `phase=deep_sleep` — printed beside `complete` on that device — is
  not counted a second time.
- **Budgets measure only their own workflows.** A section is checked against
  the workflows that exercise it (`reader-soak` turns pages, so it answers to
  the page-turn budgets) and against nothing else, so pooling a file with
  `--all` cannot let one capture's samples decide another's verdict. A
  workflow no section claims is reported rather than passed over silently.
- **`thermal-run` records the workflow it ran.** `--suite` picks the
  underlying workload, and that choice is now stored in `run_start` and
  decides both the budgets and the signal check: a `--suite sleep-sync`
  thermal run owes sleep telemetry and answers to the Full-refresh budgets.
  Captures made before this carry no `workflow` and are reported as ungated
  rather than assumed to be page-turn runs. A workflow name this bench.py
  does not know — a typo, or a log from a newer harness — is reported the
  same way, on both the budget and the signal side: nothing knows what that
  run owed, and silence would read as a pass.
- **Report one log's paths at a time when they predate suite labels.**
  Pooled paths are concatenated, and a log that opens without a `run_start`
  gets a synthetic boundary so it cannot join the previous file's run. It
  stays unlabelled, sits outside every budget section, and `--strict` says
  so — labelled and unlabelled captures in one report is not something the
  harness will guess about.
- **Budgets need Python ≥ 3.11**, or the optional `tomli` package. macOS
  system `python3` is 3.9, where `tomllib` does not exist. `--strict` now
  refuses to run without a parser rather than passing everything silently;
  a non-strict report prints a `budgets not checked` warning and carries on.
  Any result previously signed off "with `--strict`" on an older interpreter
  verified nothing.
- **Boot and wake timings** come from the `t_ms` on a boot's first render, so
  they only appear for boots the capture witnessed (`--reset-before`, a boot
  marker, or a wake). They are reported **per kind** — `boot to paint (cold)`
  and `boot to paint (wake)` are separate lines, because a cold boot pays the
  full waveform and a wake does not, and a pooled median matches no boot that
  ever happened. A wake that lost its sleep image counts as cold, since that
  is what it costs.
- Deep sleep drops the USB-JTAG serial port mid-capture; bench.py
  announces the loss and waits for the port to re-enumerate — wake the
  device to resume. The capture window keeps counting while it is away.

## Logs

Raw bench logs are written under `target/bench/` by default and should not be
committed. Captures append to the same file, so a log usually holds several
runs; `report` (and the summary each capture prints) covers only the latest
run — pass `--all` to pool the whole log.

```sh
tools/bench/bench.py report target/bench/latest.jsonl
```

The harness has host tests covering the parser, the report, and the trust
rules; `tools/check.sh fast` runs them (`tools/check.sh test-bench` alone).
They need no hardware, and a change to bench.py should come with one — the
harness produces every device number this project has, so a defect here is
indistinguishable from a firmware regression until someone re-derives it.

Keep notable hardware findings in `.scratch/` issues or dated docs notes.
