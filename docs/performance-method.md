# Performance method

Optimization work is tracked in GitHub issues with the `perf` label. Before you
propose one, search closed `wontfix` and `reverted` issues: declined ideas may
have failed measurements, and reverted optimizations may have caused regressions.
Use both commands in `docs/agents/issue-tracker.md` (with a relevant search term)
and read the reasons recorded in the matching issues.

This doc holds the rules that decide whether a measurement can be trusted, and
how each kind of change is verified. It holds no baselines. Those are being
re-taken in [#372](https://github.com/chongfun/calendula-os/issues/372), so quote that and not older figures.

## Rules

The numbers match the optimization roadmap this replaces, so older references
to "rule 4" or "rule 12" still resolve.

1. **Measure the split before ranking a "cache the layout" item.** A
   layout-caching item sat at the top of the queue for three revisions before
   anyone checked. Layout was under 2% of a portrait page render, and the rest
   was rasterization.

2. **Check that a measurement brackets what the reader experiences.** One
   misplaced `println!` put the prestage inside the reported page turn and made
   a slowdown look like a win. Compare time-to-first-content with a plain
   build, not with zero.

3. **`page-turn` figures depend on cadence.** A press that lands during a
   render, or while a repaint from the last turn is pending, waits behind it,
   so the figure moves with the rhythm of the presses. Quote it from a steady,
   deliberate cadence, or from the `bench-selftest` build, which presses only
   after the device goes quiet (`docs/agents/bench.md`). `layout_ms`,
   `flush_ms` and `busy_ms` are timed per render or refresh, so cadence
   does not stretch each sample. Cadence can still change the refresh-mode
   mix, so compare flush and BUSY figures by mode (rule 12). On the X3,
   a Full refresh has two BUSY samples both logged as `Full`: a white
   scrub using the FastClean bank, then the target Full waveform. Compare
   those intervals separately, not as one Full-mode median.

4. **A partial cache must record on disk whether the rest is coming.** The
   reducer clamps the reader to the advertised page count, so without that
   record a truncated index stops the reader at the last cached page, and it
   looks exactly like a short book.

5. **The compiler names what compiles, not what should be public.** Making
   public whatever `rustc` complained about during a crate extraction once
   widened eighteen fields that only mean anything as a set.

6. **Host gates cannot see a stack overflow.** The device has no guard page, so
   an overflow runs into `.bss` and shows up later as an unrelated panic.
   `tools/check.sh stack-frames` disassembles the release build and fails on
   any frame over 24 KB. It bounds one frame, not call depth.

7. **Check whether the answer is already being printed.** Before designing an
   experiment, grep the firmware for what it prints and `tools/bench/bench.py`
   for what it reads. They are not the same set. Several headline numbers were
   once printed on every run and read by nothing.

8. **A gate that can be silently absent is worse than none.** `report --strict`
   once checked nothing on an old Python and still exited 0. Any gate this
   project relies on should fail loudly when its preconditions are missing.

9. **Do not carry a conclusion across boards.** "The BUSY floor is OTP and
   cannot be touched" is true of the X4's SSD1677 and false of the X3's UC8253,
   which uploads its own LUTs and sets its own frame timing. That mix-up hid a
   fixed cost in every X3 refresh until someone checked. When a number and a
   mechanism come from different boards, at least one of them is wrong.

10. **Suppress the repaint, not the event.** An event that carries state the
    reducer needs must always be applied, and only the repaint may be skipped.
    Suppressing the event is how rule 4's failure came back once. A guard
    below the reducer, at the flush seam, cannot cause it and is the safer
    default.

11. **A pin protects bytes, not version numbers.** Before regenerating a frozen
    artifact, run the generator unmodified and require a clean diff. A font
    toolchain pin once could not rebuild the shipped tables, and it sat
    unnoticed for a week under a commit that said reproduction was verified.

12. **Split flush figures by refresh mode.** `bench.py`'s `render flush` and
    `refresh busy` lines pool Fast, FastClean and Full, so their medians follow
    the mix of the capture as much as any change. Quote the `refresh modes`
    counts next to any flush figure. A change that moves every mode at once, or
    removes whole renders, changes the mix too. The built-in report does not
    print per-mode duration medians; use the JSONL extraction below.

### Inspecting per-mode timings

After collecting an X3 capture, run this from the repository root to print
per-mode sample counts and medians for its **latest run**:

```sh
python3.14 - target/bench/latest.jsonl <<'PY'
import json
import statistics
import sys

with open(sys.argv[1], encoding="utf-8") as capture:
    events = [json.loads(line) for line in capture if line.strip()]
starts = [i for i, e in enumerate(events) if e.get("event") == "run_start"]
if starts:
    events = events[starts[-1]:]

for kind, field in (("render", "flush_ms"), ("refresh", "busy_ms")):
    groups = {}
    full_count = 0
    for event in events:
        if event.get("event") != kind or not isinstance(event.get(field), int):
            continue
        mode = event.get("mode")
        if mode is None:
            continue  # Legacy refresh records have no mode.
        name = str(mode)
        if kind == "refresh" and mode == "Full":
            # X3 FULL_STEPS: white scrub first, target waveform second.
            phase = "white scrub" if full_count % 2 == 0 else "target waveform"
            name = f"Full ({phase})"
            full_count += 1
        groups.setdefault(name, []).append(event[field])
    print(f"{kind} {field}:")
    for name, samples in sorted(groups.items()):
        print(f"  {name}: n={len(samples)}, median={statistics.median(samples):g} ms")
    if kind == "refresh" and full_count % 2:
        print("  WARNING: unmatched Full sample; inspect capture boundaries")
PY
```

The X3 phase split relies on the order of complete Full refresh pairs, not
a phase field in the log: both `bench: refresh` events have `mode=Full`.
Check the underlying chronological events when a capture starts or ends
mid-refresh, since an incomplete pair can mislabel subsequent samples.
Use `bench.py report target/bench/latest.jsonl --board x3` for the normal
summary and budgets; the extraction above is supplementary.

13. **Take the before-capture on today's `main`, in the same session.** A figure
    from an older doc may have drifted with the merges since, and that drift
    would be credited to the change. An A/B on one card and one book, minutes
    apart, also makes controls such as `busy_ms` and prestage mean something.

## Verification

- **Timing.** The `tools/bench/bench.py` suites are `page-turn`, `reader-soak`,
  `storage-cache`, `sleep-sync`, `folder-nav` and `channel-stress --host`, run
  as `docs/agents/bench.md` describes. `tools/bench/benches.toml` holds the
  default budgets, loose enough for either board, and the X4 uses them. The
  X3's tighter budgets are in `tools/bench/benches-x3.toml`. Pass
  `--board x3` on X3 captures. The device does not report its board, so
  without the flag an X3 capture is checked against the default budgets.
  Until [#372](https://github.com/chongfun/calendula-os/issues/372) resizes
  the X3 budgets for `page-turn`, `reader-soak` and `sleep-sync`, they still
  assume A12, so strict X3 runs of those suites can fail against stale
  thresholds. In particular, a `sleep-sync` Full-refresh BUSY sample above
  900 ms exceeds its stale ceiling; `folder-nav` and `storage-cache` have
  separate budgets. Run Python from the series in `.python-version`;
  `tools/check.sh` finds it.
- **Visual.** Emulator golden frames on both boards, per
  `docs/agents/visual-verification.md`.
- **Size and stack.** `llvm-size -A`, `llvm-nm` on `_stack_start` and
  `_stack_end`, the link-time stack ASSERT in `fw/build.rs`, and
  `tools/check.sh stack-frames`. Check both boards. The X3 is the tight one.
- **Upload.** A timed `curl --data-binary @book.epub` A/B against the device's
  `/upload?name=` endpoint. The `sd_stats` counters show whether writes are
  batched, by comparing `wr_calls` with `wr_blocks`. `storage_build` prints
  them, and the upload path does not yet.
- **Power.** The first instrument is the X3's BQ27220 fuel gauge, read as a
  coulomb counter across a long deep sleep. It stays powered while the SoC
  sleeps and needs no meter and no disassembly. Do not meter the 4-pin pogo
  cable. It is USB, so it sits on the wrong side of the charger, and plugging
  it in changes the state under test. If the gauge shows an unexpected drain, put a
  series meter on the battery lead, and use a PPK2-class instrument, not a DMM.
  A DMM's burden voltage on a µA range can brown the device out on a wake
  spike, and the span needed runs from about 15 µA to 100 mA. `bench.py` has no
  power channel.
- **Boards.** The maintainer has no X4. X4 verification is build, clippy and
  goldens, and an on-device X4 check needs a contributor with the hardware.
