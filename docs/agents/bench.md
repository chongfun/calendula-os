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
  instead of satisfying the check with its Full refresh.
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
