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
  section cache, folder browsing, or progress-write changes:

```sh
tools/bench/bench.py folder-nav --port /dev/cu.usbmodem101 --entries 20
tools/bench/bench.py page-turn --port /dev/cu.usbmodem101 --turns 50
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 5
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --reset-before --seconds 20 --strict
```

## Unattended captures: the `bench-selftest` build

`bench.py` listens; it cannot press a key. Every suite below is therefore
operator-driven, which puts the operator's cadence inside the measurement.
A firmware built with the `bench-selftest` feature presses the keys itself:

```sh
tools/cargo.sh build --release -p fw --features device-x3,bench-selftest
espflash flash --chip esp32c3 --flash-size 16mb --partition-table partitions.csv \
  --ignore-app-descriptor --port /dev/cu.usbmodemXXXX \
  target/riscv32imc-unknown-none-elf/release/fw
tools/bench/bench.py page-turn --port /dev/cu.usbmodemXXXX --reset-before \
  --turns 50 --seconds 200 --strict
```

The scenario waits out the boot paint, walks to Reading by watching which
view each press actually lands on, waits for the device to stop painting on
its own, then turns pages one settled render at a time. Cadence comes from
the settle rather than a host timer, so presses cannot land mid-refresh.

Read the numbers with two things in mind.

- **Cadence is part of the measurement, and the injector now waits for
  quiet.** Calibrated 2026-09-09 on the X3 against a hand-pressed run on the
  same book in the same warm regime, 50 clean pairings every leg:

  | injector cadence | median | min | p95 | max | queue wait | refreshes / 50 |
  |---|---|---|---|---|---|---|
  | hand-pressed, ~2 s | 418 | 411 | 434 | 456 | 0 ms | 82 |
  | press at settle + 0 | 433 | 411 | 434 | 455 | 22 ms | 56 |
  | press at settle + 50 ms | 845 | 425 | 857 | 867 | 397 ms | 96 |
  | press after 1.5 s quiet | 426 | 412 | 427 | 428 | 1 ms | 68 |

  Three things this settled. Pressing the instant a render settles lands the
  press behind the display task's 24 ms prestage, which is the 22 ms of queue
  wait and the whole of the first 15 ms gap; the ADC stage the injector skips
  is worth a few milliseconds the other way. Pressing 50 ms later is worse,
  because a page turn that crosses a section boundary sends an extend and
  `loaded_repaints` repaints the page when the section loads: a second Fast
  refresh, about 400 ms, requested a median of 2 ms after the settle but with
  a tail to 735 ms in an injected run and 1,071 ms in the hand-pressed one.
  A press at settle plus zero supersedes that repaint, so the tight injector
  saw one refresh per turn where the hand saw 82 in 50; a press at settle
  plus 50 waits behind it; a 600 ms quiet window tried in between returned
  inside the tail on 6 of 50 turns and paid two flushes each. Waiting until
  no frame has settled for 1.5 s clears the measured tail and matches the
  deliberate cadence a reader used, and the residual against the hand-pressed
  run is 8 ms at the median with minima one millisecond apart.

  So an injected median is comparable to an operator baseline as it stands,
  and the refresh count now shows the repaints rather than hiding them. Two
  findings belong to the roadmap rather than here: on a book whose sections
  are a page long, every turn at reading cadence costs two Fast refreshes,
  and the repaint that carries the replaced text can arrive more than a
  second after the page first painted.
- **The book and the card still decide the figure.** The scenario opens
  whatever the first row offers. A capture meant to compare against the
  11.7 MB baseline book needs that book on the card and reachable, exactly
  as a hand-driven one does.

A measured example, X3, 50 turns, nobody touching the device:

```
page turn      median=433ms p95=453ms min=412ms max=472ms
page inputs:   presses=50 page_turns=50 nav=0 coalesced=0 unmatched=0
```

Fifty presses, fifty pairings, nothing coalesced, `--strict` clean. The
pooled operator captures on the same harness report `presses=101
page_turns=70 nav=28 coalesced=3`, a 28-second maximum, and three runs
excluded for cadence. That example predates the quiet-cadence change; the
calibration table above is the current shape, with a 16 ms spread across
fifty turns.

One regime caveat the calibration also surfaced. A book opened cold by the
injector is built by B4 and then read from RAM, two storage opens in fifty
turns; the same book after a reboot pages from the card's cache, a dozen or
more warm opens. Both legs of a comparison must be in the same regime, which
in practice means a reset before each with the cache already built.

**It presses keys on every boot**, so it is a bench build and not a reading
one. Reflash without the feature to get the device back.

### Re-verified on the squashed tip, 2026-09-09

Every scenario ran on the X3 from the single squashed commit, with the folder
card in and the quiet cadence on:

| scenario | strict | what the run showed |
|---|---|---|
| `page-turn` | pass | median 427, min 411, p95 429, 50 of 50, queue wait 0 |
| `storage-cache` | budget | 3 cycles, 36 of 36, `result=done`; warm open p95 274 ms against the 150 ms budget |
| `folder-nav` | pass | 20 of 20 round trips, every leave ok, depth held, timings unchanged |
| `reader-soak` | pass | two passes across two self-wakes, `jumped=true returned=true` on both, no invalid records |
| `sleep-sync` | floor | 3 cycles, first-attempt sleeps, 1 Full across 3 boots, wake-to-paint 2,188 ms |

The two non-passes are not the scenarios. sleep-sync fails only the
pre-existing `full_refresh_busy_min_ms = 3000` floor, seven times over against
a measured 929 ms Full, which the roadmap already records as stale. The
storage-cache warm-open p95 is dragged by one 639 ms open: with folders on the
card, `reach_reading` pressed Confirm three times in Library, descending into
the tree, before a book opened, and the open that followed that walk was the
slow one. That path did not exist when the 150 ms budget was set on a flat
card. In the same run B4's background build finished a cache that had been
partial through every earlier capture, 71 s during a Library idle, which is
expected and worth knowing when reading warm-open percentiles near it.

A second review round at the squashed tip found four boundary holes, fixed
and re-run on the device the same day:

- **The last leave's repaint is the render frozen after the leave, not the
  first to settle.** The Back press renders at once, showing the folder as
  Leaving with the depth unchanged, and the SD leave prints its line in about
  40 ms, inside that render's flush. Accepting the first settle took the Back
  render as the parent listing. The host now requires `req_ms` after the
  leave's `t_ms`; on the device the completing render was requested 2 ms
  after the last leave, and the Back render did not end the run.
- **An injected press logs `action=` beside `button=`.** The key is what the
  reducer sees; the action is what the scenario meant. Under `PagesLeft` the
  key that turns a page is Confirm, so pairing on the key alone made fifty
  real turns invisible. The host pairs on the action when present and reads
  a manual capture exactly as before.
- **A turn is a page that moved.** A `Next` at the last page redraws the page
  because every input marks the frame dirty, and the settle counted as a
  turn. The page is published beside the view now, and a settle that moved
  nothing reports `invalid=end-of-book` and stops. Driven to the end of a
  393-page book on the device: turn 21 redrew page 392, 21 turns counted,
  `result=short-turns`, strict failed on three named reasons. The host's own
  pairing still counts that redraw, 22 against 21, because a press did
  produce a render; the record stops certification.
- **A quiet wait that runs out its budget is a record, not prose.** Four
  callers were noting it and carrying on. They now write
  `invalid=not-quiescent` and carry on, so the evidence is kept and cannot
  certify.

A third review round found the last boundary: the host stopped a count-bounded
capture before the firmware had decided the final operation, so a failing
verdict on the last turn or leave could be lost and the run certified. The
`completed=` checkpoint above is the fix. Re-run on the device: folder-nav
stopped on `completed=folder_leave count=20` with the verdict in hand;
page-turn from a book left on its last page met the end of the book on its
first press, wrote `invalid=end-of-book`, ended on `result=short-turns`, and
refused certification, and from a book opened mid-way it stopped on
`completed=page_turn count=50`, strict clean.

One harness rule came out of the pass. A scenario that ends its own capture
with `result=done` owes no `--seconds` window: the seconds were the ceiling
these docs tell you to pass, and holding the run to them failed every
storage-cache selftest on "185s of the 500s requested" with `done` in the
same log. Only `done` earns that; a clock stop or a non-done result keeps
the contract.

### A full sequence eats the book

Running all five suites back to back turns about 150 pages: 50 for
page-turn, 18 for sleep-sync across its boots, 36 for storage-cache, and
whatever the soak reaches. The suites share the card and the reading
position, so the soak run last opens a book the others have already
advanced.

On 2026-09-12 that ended the soak at page 302 with `invalid=end-of-book`
twice and `result=short-turns`, and it refused to certify. Nothing was
wrong with the device. Either start from a book at least 150 pages from its
end, or run `reader-soak` first and the page-turn suites after it. The
other four suites certify in any order.

### Choosing a scenario

`BENCH_SCENARIO` picks one at build time, defaulting to `page-turn`. Every
scenario is compiled into every bench build, so the variable selects rather
than gates, and `fw/build.rs` reruns on it so a changed value actually
changes the image:

```sh
BENCH_SCENARIO=folder-nav tools/cargo.sh build --release -p fw \
  --features device-x3,bench-selftest
```

Compile time and not run time because there is nothing to ask. The radio is
off by design, and the firmware reads no serial, so a flash is the control
channel. Naming a scenario that does not exist prints the valid list and
does nothing, rather than quietly running the default under the wrong name.

| `BENCH_SCENARIO` | What it drives | What the card needs |
|---|---|---|
| `page-turn` | Opens a book, turns 50 pages | The book you mean to time, reachable from the first row |
| `storage-cache` | Three open/read/back cycles, 12 turns each | Enough pages to cross a section boundary |
| `folder-nav` | 20 enter-and-leave round trips, 3 cursor steps apart | **Folders.** On a flat card every row is a book, so the run reports 0 folder entries and `--strict` fails, correctly. First measured 2026-09-09 on a foldered card: 20 of 20 round trips, every leave `ok`, depth held at 1 throughout, `--strict` clean |
| `reader-soak` | Turns, a chapter jump, Home and Library returns, then sleep | A book with chapters |
| `sleep-sync` | Six fast turns, then sleep | Nothing particular |

### The terminal protocol

A selftest scenario says which one it is, how far it has got, and how it
ended, in four records with one rule each.

- **Every record names its scenario, and all of them are checked.** The
  announcement is `bench-selftest: scenario=X view=...`, printed once per
  boot, and `--strict` fails if X is not the workflow the capture was taken
  as. The same comparison runs on the terminal and invalid records, because
  the announcement prints four seconds after boot while a finite scenario
  works for minutes: a capture attached late sees a terminal record and no
  announcement, and a `page-turn` image captured as `storage-cache` supplies
  storage telemetry from opening its book. The check matters most for
  `reader-soak`, whose gate asks for input and render telemetry plus a
  completed sleep and a later wake: a `sleep-sync` image produces every one
  of those, and a successful sleeping scenario writes no terminal record, so
  nothing else would notice. For a `thermal-run`, the comparison is against
  the workflow it selected rather than `thermal-run` itself.

- **It reports each operation once every postcondition has been checked**:
  `bench-selftest: scenario=X completed=<kind> count=N`, where the kind is
  `page_turn` or `folder_leave`. A checkpoint may stop a selftest capture
  only when its kind is the one the capture is counting. The operation's own
  telemetry, a paired render or a `folder_leave`, arrives before the
  firmware's verdict: a turn still has its quiet check ahead, and a leave
  its depth wait and quiet check, either of which can write an `invalid=` or
  turn the run into `leave-failed`. A host that stopped on the telemetry
  left that verdict unsent and certified the capture. The kind matters
  because every scenario that opens a book turns pages first: an untyped
  checkpoint made `sleep-sync --cycles 3` stop on its third page turn, in
  the first boot, before a single sleep. On the device with typed
  checkpoints: page-turn `--turns 50` stopped on `completed=page_turn
  count=50`, one line after the checkpoint, no `invalid=`; folder-nav
  `--entries 20` stopped on `completed=folder_leave count=20`; sleep-sync
  `--cycles 3` saw eighteen page-turn checkpoints across its three boots and
  stopped on the third `sleep_complete`, as before the checkpoints existed.

  Checkpoints count from where the capture joined. The announcement is one
  line at boot and a scenario works for minutes after it, so a capture that
  attaches late sees no announcement. It still knows the stream is
  self-driven, because an injected press logs `action=`, which a hand on the
  buttons cannot produce, and the press comes before the operation it
  drives. A late attach owes N operations from the point it joined rather
  than stopping on a total it did not watch, and the first checkpoint it
  sees is the baseline rather than a sample: the operation that checkpoint
  certifies may have begun before the port was open. Only a capture that saw
  the announcement counts its first checkpoint. On the device, attaching to
  a folder-nav image 45 s after flashing and asking for `--entries 5` saw
  checkpoints 4 through 9, counted five, and passed `--strict`; without the
  baseline rule it stopped one checkpoint early and strict reported four of
  five round trips captured. The report judges a self-driven capture on two
  counts, and it owes both. The checkpoints, from the same baseline, say the
  device completed N operations: a late attach one checkpoint short of its
  target that ran on to `result=done` used to pass strict, because the
  shortfall check counted `folder_leave` telemetry and that population held
  the partial round trip the stop rule had excluded. The telemetry inside
  the counted window, after the baseline, says the host measured N of them:
  a checkpoint survives a dropped render line, and fifty completed turns
  over forty-nine paired renders is reported as forty-nine measured, not
  certified as fifty. A folder round trip is measured only with both halves
  present, a successful `folder_enter` and the successful `folder_leave`
  that follows it: enter and leave feed separate budgets, so five leaves
  over four enters is four round trips, and the warning names both counts.
  A manual capture carries no checkpoint and the host counts its round
  trips the same way, with the parent-render boundary for folder leaves.

- **One terminal record per run**, written by the driver rather than
  the scenario, so a second one cannot happen: `bench-selftest: scenario=X
  result=W`. `result=done` means the scenario finished everything it set out
  to do. Any other word names what stopped it (`nav-failed`, `no-folders`,
  `short-turns`, `lost-library`, `sleep-refused`). The host stops the capture
  on any of them, because the device has stopped talking, and `--strict`
  certifies only `done`.
- **A phase that did not run** reports `bench-selftest: scenario=X
  invalid=REASON` the moment it happens, and the scenario carries on.
  Reported at the moment and not summarized at the end, because a count
  target can stop the capture mid-scenario: `folder-nav --entries 20` ends on
  the twentieth completed round trip, so a stall summarized after the walk
  goes to a host that has stopped listening.

The first device capture of this suite (2026-09-09, X3, a card with two
folders of 6 and 14 rows) measured entry at 37-38 ms into the 6-row folder
and 86-87 ms into the 14-row one, with leave at 38-40 ms throughout. Those
populations size the `[folder-nav]` budgets in `benches.toml`.

The entry gap between those two folders was settled by counting rather than
timing. A bench build reports `bench: folder_walks walks= resolve_entries=
iterate_entries=` after every folder operation: how many directory walks it
took, how many parent entries were scanned resolving the path, and how many
entries were iterated inside the folder. Measured on the same card:

| operation | rows shown | walks | resolve entries | iterate entries | ms |
|---|---|---|---|---|---|
| enter, 6 books + 1 empty folder | 7 | 3 | 23 | 45 | 49 to 51 |
| enter, 13 books + 1 folder | 14 | 3 | 17 | 96 | 86 to 109 |
| leave, into a 2-row parent | 2 | 4 | 12 | 23 | 40 to 41 |

Two things fall out. Path resolution is cheaper for the slow folder, so where
it sits in `/BOOKS` is not the cause. And the slow folder iterates 32
directory entries per walk to show 14 rows, while the fast one iterates 15 to
show 7: both hold about one hidden entry per visible row, and the slow one
has 17 more of them per walk, three walks over. A model of about 6 ms per
walk plus 0.73 ms per iterated entry fits both entries, and predicts the
leave at 40 ms with nothing left to tune.

The hidden entries are filtered by `is_hidden_entry` before they reach the
listing, so they cost iteration and show nothing. On a card organized from a
Mac they are most likely AppleDouble `._` sidecars and `.DS_Store`, which is
the population #65 stopped cataloguing as books; `ls -la` on the folder
confirms it. Name length is a second-order cost on top: an LFN entry is one
slot per 13 characters, and the fast folder has the longer names and is still
faster because it has half the entries.

**Build the image you flash, alone.** `tools/check.sh all` builds `fw` for
both boards into the same `target/.../release/fw`, so a check running in the
background while you flash hands espflash whichever board it wrote last. An
X4 image on the X3 halts in the board guard with `board: halted` and a
capture full of nothing. Check the ELF before it goes near the port: a bench
build contains the string `folder_walks`, an X3 build contains `x3 init done`.

`folder-nav` counts round trips rather than entries, because entering and
leaving are separate storage operations with separate telemetry and the suite
promises both. `--entries N` therefore owes N completed leaves, a capture
holding entries and no leaves fails as a walk that went down and did not come
back, and a refused leave fails as a card fault rather than counting as a
sample.

The Nth leave does not end the capture by itself. `folder_leave` is printed
by the storage call the moment its SD work is done, before the listing has
reached the app, been folded into state, or been drawn, so stopping there
would end the run mid-round-trip on the very sample the operator asked for.
The capture waits for the repaint that completes it, on the same principle as
the page-turn prestage.

Two boundaries, and telling them apart matters for anything driving the
device. A press is answered by a render of the state that press produced. A
storage operation is answered later, by its own event. Back inside a folder
sets the browse to Leaving and touches the depth not at all, so a leave has
to be waited for on the depth, not on the settle. Pressing Back again while
that move is in flight is a deliberate escape hatch in the reducer: it
abandons the move and leaves for Home. `--strict` fails on it too,
  but the rest of the capture survives, which matters for a soak whose sleep
  and wake are still worth having.

Both matter because a suite's own strict signals are weaker than they look.
reader-soak asks for input and render telemetry plus a completed sleep and a
later wake, and a pass that skipped its chapter jump, its Home and Library
return, or half its page turns produces all of that. So every advertised
phase reports itself rather than relying on the suite gate to notice.

The two sleep suites reach no terminal record on success, deliberately: the
sleep does not return, and their capture is meant to continue across the wake
into the next cycle. A terminal record from one of them means the sleep was
refused.

### The sleep suites reboot

Deep sleep is terminal on this firmware: waking is a fresh boot. So
`sleep-sync` and `reader-soak` do one cycle per boot, and the scenario runs
again on the other side. bench.py reconnects across the re-enumeration and
counts `sleep_complete` until it has the cycles it asked for.

The wake comes from an RTC timer armed beside the button, which
`bench-selftest` adds to `hal_ext::rtc`. It is off in every shipped build,
and it needs to be: a reader that wakes itself would spend the battery this
firmware is careful with.

**A wake's own boot marker is unobservable, so read the waveform instead.**
`main: deep_sleep_wake=` prints before `esp_rtos::start`, which is before the
USB device re-enumerates, so on exactly the boots where it matters the host
misses the line. bench.py then falls back to "a sleep preceded this, so call
it a wake", which is a label rather than evidence. To confirm a wake really
took the fast path, count Full refreshes: a wake with a settled sleep image
owes none, so three boots with three Fulls means three cold paths whatever
the labels say. Measured on the X3, that mislabelling was worth about 860 ms
of wake timing before the timer wake was recognized.

**A sleeping device cannot be reached at all.** Deep sleep powers down the
USB Serial/JTAG peripheral, so the port disappears from the host and neither
espflash nor bench.py can do anything until someone presses Power. A plain
build left idle will do this on its own after 3 minutes in menus or 10 in
Reading. Flash the bench build before walking away, or expect to press the
button once.

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
  request, so a `--cycles 10` run interrupted after one cycle, a `--minutes 30`
  soak stopped at 95 seconds, and a log truncated before its `run_end` all fail
  rather than passing on having produced *some* expected telemetry. Ctrl-C
  completes a capture that asked for no other stop condition and cuts short one
  that did. Captures predating this are reported as unverified, not assumed
  complete.
- **A count and a duration are not both minimums.** Whichever the operator
  *typed* is the contract. `page-turn --turns 50 --seconds 60` owes 50 turns
  and treats `--seconds` as a ceiling; the banner names both stop conditions
  and says which is which. `page-turn --seconds 60` owes 60 seconds and
  nothing else — 50 is the suite's default, not a request, so it neither stops
  the capture nor gates it, which otherwise reported almost every time-boxed
  capture as short of a count nobody asked for. Suites with no count keep the
  duration as their contract.
- **Durations and counts must be positive.** `--seconds 0`, `--minutes 0`,
  `--turns 0` and `--cycles 0` are rejected at the command line; zero used to
  disable the deadline and capture forever. Omit the flag to capture without
  that limit.
- **`--reset-before` is setup, not telemetry.** The capture window opens once
  the reset returns, so `--reset-before --seconds 20` collects twenty seconds
  rather than twenty minus espflash and re-enumeration. `run_end` carries
  both: `elapsed_s` is the telemetry window a requested duration is checked
  against, `command_elapsed_s` the whole command.
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
  which paths the run will exercise, are recorded in `run_start`, and
  `--strict` fails if the capture never took one. Cold is shown by a book open
  that had to build its cache, or a catalog scan that *succeeded*; warm by an
  open served from an already-built cache or the loaded RAM window, or a
  catalog loaded from its snapshot. Neither flag means an unrestricted capture
  owing no particular path. Until 2026-07-31 the flags were read once by
  argparse and never checked, so `--cold --warm --strict` proved neither.
- **A failed storage operation is not evidence, and not a sample.** The scan
  line carries its own `ok`, stamped before the firmware's UI fallback can
  replace a failed scan's `Error` with `Ready` — that fallback keeps the
  reader on an older in-memory catalog, which made the marker read as success
  and let a failed scan evidence the cold path. Failed operations are out of
  mode evidence and out of the `catalog_load_warn_ms` population, counted on
  their own report line, and one in a `storage-cache` run fails `--strict` the
  way a failed sleep phase fails a sleep suite.
- **A missing snapshot is the cold path; anything else is a fault.** The
  catalog load reports `result=hit|miss|stale|invalid|error`, because its `ok`
  could not tell a card with nothing to hand over from a card that failed.
  Two of those are *expected*, and neither fails `--strict`:

  - `miss` — no catalog directory, or no file in it. `load_catalog_cache`
    returning false is what queues the scan, so a card whose catalog has not
    been built yet prints one immediately before the scan that builds it, and
    `--reset-before` makes that the common case.
  - `stale` — a catalog written by another `CATALOG_VERSION`. Bumping that
    version *is* how the on-card format migrates (the old snapshot stops
    loading, the scan rebuilds it, no migration code), so this is the designed
    first boot after a firmware upgrade.

  The other two are findings and fail `--strict` even when a later scan
  succeeds: `invalid` (wrong magic, the version-0 placeholder an interrupted
  scan leaves, a length disagreeing with its header, or a record that ended
  early) and `error` (a refused open, seek or read). The firmware used to
  reduce the whole read to a bool *inside* the SD session, so every one of
  those surfaced as the benign miss. No non-`hit` result enters the
  `catalog_load_warn_ms` population, where it would measure how fast the card
  said no. A `result=` outside the vocabulary — a typo, or a log from newer
  firmware — also fails `--strict`: it is neither a success, nor a fault this
  tool recognises, nor legacy telemetry, so silence would read as a pass.
- **Strict evidence needs confirmed success; the figures tolerate old logs.**
  A requested `--cold`/`--warm` path is proven only by an operation that says
  it succeeded. Telemetry too old to carry a result gets `cannot be verified
  from this capture` rather than a silent pass: the host records the requested
  modes whatever firmware is on the device, so otherwise a current bench.py
  against an older build would certify a path from a line that cannot support
  the claim. Nothing regresses — such a capture never had its mode verified.
  The duration figures and budgets take the opposite policy on purpose and
  still include result-less lines, because a budget asks how long the working
  path took rather than claiming what ran.
- **Not every cache build belongs to an open.** A background walk's last step
  publishes through the same path, emitting `storage_build` with no open in
  flight, and is announced as `storage_background_build` right after. The
  announcement consumes the pending build, so the next ordinary open — perhaps
  minutes later — stays warm. Without it that open was filed as cold: a real
  warm sample lost to the budget, a `--warm` path reported missing, and a
  72 ms open described as a 14-64 second one.
- **A book open is reported per path, never pooled.** `storage open (ram)`,
  `(warm)` and `(cold)` are different work — 0-15 ms, 57-95 ms and 14-64
  *seconds* on this repo's captures — so a pooled percentile describes none of
  them. `warm_book_open_warn_ms` measures the warm population alone: an open
  that read the card with no cache build in the same transaction. Computed
  over every `storage_open`, a deliberately cold open failed the *warm*
  ceiling and a RAM hit pulled the percentile back under it. Cold opens scale
  with book size and are reported without a budget.
- **A malformed budget file is a configuration error.** Sections, key
  spelling and value types are validated against `BUDGET_SCHEMA` before any
  capture is read, so a typo like `median_press_to_settledd_ms` fails the load
  instead of silently leaving page-turn with no latency threshold. Unknown
  keys, strings and booleans go the same way (`isinstance(True, int)` holds,
  so a bool would have reached the comparison as 1) — as do values that are
  the right type and still gate nothing: a negative threshold, a floor above
  its own ceiling, an empty section, and a document with no sections at all.
  `--strict` refuses to run; a plain report says budgets were not checked.
  Adding a budget means adding it to the schema and reading it somewhere.
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
- **One interpreter series, named in `.python-version`** (3.14). macOS ships
  3.9 as `python3`, so `tools/check.sh` prefers `python3.14` and fails with
  instructions if it cannot find it; `PYTHON=...` overrides, and a capture run
  off the shebang checks itself the same way. `tomllib` is imported directly
  rather than falling back, so a budget cannot be enforced in CI and silently
  skipped on the bench — which is what happened while the parser was optional:
  any result signed off "with `--strict`" without one verified nothing.
- **The X4's two refresh lines count once.** Its legacy busy line followed
  immediately by the structured refresh event describes one panel refresh.
  The report keeps the structured event. Pairing uses the original event
  stream, so a run, boot, render, or other event between equal-duration
  refreshes keeps both samples; a legacy refresh on its own still counts.
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
