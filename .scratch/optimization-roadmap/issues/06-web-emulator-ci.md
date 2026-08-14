# WS-F: Web emulator, CI & the bench harness

Status (2026-07-30): F1–F4 and F6 are done and **independently re-verified by
rebuilding the site** — initial transfer is 809 KB gz against a 1.45 MB
baseline (−47%; the −49% claim holds), `wasm-opt` is genuinely running, and
the reading goldens are genuinely in CI.

**Scope widened 2026-07-30 to include `tools/bench/`.** Three rounds of this
roadmap were misranked by instrumentation defects, and the harness that
produces every device number in the project had no owner. F7–F9 below are now
the highest-value items in this workstream by a wide margin — they are worth
more than any web-page byte, because everything else in the roadmap is ranked
using their output.

Owns: `web/`, `tools/web-emulator/`, `tools/build-web.sh`, `tools/bench/`,
`.github/workflows/`, and test-harness fixes in `tools/emulator`. Fully
disjoint from the firmware workstreams — safe to run in parallel with
everything.

## Open

Order: F7 → F8 → F9 (all measurement integrity, all cheap) → F10 → F11 → F5.

### F7 (S): `bench.py report --strict` is a silent no-op on Python < 3.11

`tools/bench/bench.py` falls back to `tomllib = None` on ImportError,
`load_budgets` then returns `{}`, and `evaluate_budgets` checks nothing —
**with no diagnostic and exit code 0.** macOS system `python3` is 3.9.6 and
`#!/usr/bin/env python3` selects it. Verified on the owner's machine:
`python3 -VV` → 3.9.6, `import tomllib` → `ModuleNotFoundError`.

Demonstrated on one log, same command:

```
/usr/bin/python3      (3.9.6)   report --strict -> exit 0, no warnings
/opt/homebrew/bin/python3 (3.14) report --strict -> "page-turn median 9190ms
                                  above warning budget 550ms", exit 1
```

A page-turn median **16.7× over budget passes clean.** Every `--strict` gate
in `tools/bench/README.md`, and every capture previously described as
"verified with `--strict`", is only real if the operator happened to have
Homebrew's python first on PATH.

- Fix: hard-fail when `--strict` is requested and `tomllib` is unavailable;
  state the required Python in the README. A few lines.
- **Anything previously signed off with `--strict` should be re-checked under
  Python ≥ 3.11 before being trusted.**

### F8 (M): the headline page-turn number is a function of operator cadence, not firmware

`page_turn_durations` pairs presses to renders **FIFO**. The firmware logs
every debounced press whether or not the reader acts on it, and the input
channel drops the oldest event on overflow — so presses genuinely go
unrendered during a burst, the pending queue never drains, and **every later
turn in the run is charged from a stale, older press**, with the error growing
monotonically in run length.

Demonstrated with the real reporter over two synthetic logs in which
press-to-settled is **exactly 500 ms in both**:

| log | reported `page turn` median | p95 |
|---|---|---|
| one press per turn | **500 ms** | 500 ms |
| operator triple-taps | **9190 ms** | 15464 ms |

**An 18× swing with zero change in firmware latency.** Two gaps make it
undetectable: the report never surfaces how many presses went unmatched (it
warns only when the duration list is *completely* empty), and `warn_if_above`
is one-sided, so a change that *drops* page turns can only make the number
look better.

This is a stronger statement than the cadence caveat already recorded in the
method rules. That rule says the number is only meaningful at deliberate
cadence; this says the error is **unbounded and grows with run length**, and
that a genuine +50 ms regression is invisible underneath it.

- Fix: report `unmatched presses` / renders-per-press beside the median,
  refuse to emit a median when the unmatched fraction is high, and give
  `page-turn` a floor as well as a ceiling.
- Lower confidence, same file: `event_sort_key` sorts globally by device
  uptime while `split_runs` splits only on host-side `run_start`, so a
  mid-capture reset interleaves two time bases with no monotonicity check.
  Three synthetic reset logs failed to produce a wrong number, so this is an
  unproven hazard, not a demonstrated bug. A `t_ms` regression check is two
  lines.

### F9 (S): the data needed to rank the rest of this roadmap is already captured and never read

This is the round's most consequential finding and it costs nothing to fix.
The firmware already prints, on **every** build and every replay:

```
bench: storage_build elapsed_ms= spine_ms= write_ms= sections= pages=
       rd_calls= rd_blocks= wr_calls= wr_blocks= key=
```

plus `storage_first_page` and `storage_background_build`. `parse_bench_line`
stores them into the JSONL generically — and `grep -c` for
`storage_build|storage_first_page|storage_background` in `bench.py` returns
**0**. `summarize_paths` prints only storage *open*, catalog and progress
write; `evaluate_budgets` budgets none of them.

So the split that WS-B has been *guessing* at for three rounds — how much of
the 24–27 s replay is wrap versus section writes — has been emitted on every
build for months. Same shape elsewhere: **boot-to-first-paint** is the `t_ms`
on the first `bench: render` line of any boot (the `Instant` epoch is
`esp_rtos::start`), present in every `sleep-sync` capture ever taken, never
extracted. And B4's own headline numbers, the reason #53 exists, are captured
and unreported.

- Fix: add `storage_build` / `storage_first_page` / `storage_background_build`
  and a boot stage table to `summarize_paths`; add `t_ms=` to ~8 existing boot
  printlns so the 2.5 s can be *attributed* rather than just totalled.
- Also here: `section_extend_warn_ms` in `benches.toml` is **dead** — the
  `storage-cache` branch reads only `warm_book_open_warn_ms` and
  `catalog_load_warn_ms`. A budget key that looks enforced and is not is worse
  than no key. And `benches.toml`'s header comment ("the CLI currently reports
  timings only") is stale.
- Board asymmetry worth recording: X4 emits `bench: refresh mode busy_ms
  screen_on` while X3 emits `mode busy_ms busy_low elapsed_ms screen_on t_ms`.
  X4 refresh events carry no `t_ms`, so they sort to the end of the stream and
  X4/X3 refresh figures are not the same measurement.
- New 2026-08-13: #75 added a sixth `catalog_load` result, **`reclaimed`** —
  the snapshot was retired on purpose because upload recovery deleted a file it
  might name. `bench.py` classifies it as non-fault, correctly. Worth knowing
  when reading boot captures taken across an interrupted upload: the rescan
  that follows is the repair, not a regression, and it will show up as a slow
  boot that no code change caused.

### F10 (S): `tools/web-emulator` is built by no PR gate

It is its own cargo workspace, so the root `cargo fmt --all`,
`clippy --workspace` and `test --workspace` never reach it. The only thing
that builds it is `pages.yml`, which is `on: push: branches: [main]` with no
`pull_request` trigger. **A PR that breaks the wasm build merges green and
fails only on the post-merge deploy** — and there is precedent for post-merge
Pages failures being the first signal.

Measured cost to close: the two board wasms build in 5.1 s and 2.7 s, against
a 45 s `golden-frames` job and a 98 s `clippy-firmware` critical path — so
attaching them adds **zero CI wall clock**. Exactly the trick that closed F2,
one level up. (Same paragraph, hygiene: `tools/emulator` is never linted or
fmt-checked either.)

### F11 (S): the bench harness's own 25 tests run nowhere

`python3 -m unittest discover -s tools/bench -p 'test_*.py'` → **25 tests,
0.065 s, all pass.** Nothing runs them: the only Python test invocation in the
repo is `tools/stack_frames.py` in `check.sh`. This is F2's pattern applied to
the measurement harness — 25 tests guarding the parser, the reporter and
`split_runs`, gating nothing. Adding them to `golden-frames` costs 0.065 s,
and running them under the *shipping* interpreter is what surfaces F7.

### F5 (L; only after F1): shared `fonts.bin` across boards — **re-scope, the deferral rationale was wrong**

F5 has been parked as "only worth it if board-switching matters", and board
switching is rare, so it never moved. That framing missed the larger win: the
same refactor cuts **every first visit**, not just a board switch.

Measured over the built artifact: the wasm `data` section is 2,838,672 B and
is essentially 100% font tables. **Merriweather is 1,218,408 B of it —
42.9% — and it is not the default face.** `FontFamily::Literata` is
`#[default]`; Merriweather is reachable only through the Font setting. At the
measured data-section gzip ratio that is ~305 KB gz, taking first-visit
transfer from **764 KB → ~443 KB gz (−41%)**. *(Byte arithmetic exact; the gz
estimate assumes the section-wide ratio holds for glyph bitmaps.)*

Testability is good — Merriweather is already exercised by the
`settings-cycle` scenario and the reading goldens, so pixel-identical goldens
prove the refactor.

Both board wasms embed byte-identical font tables, so switching boards
re-downloads everything. After F1, move the fonts into one fetched
`fonts.bin` shared by both builds. This requires the `display` font statics to
become runtime-initialized references under a `web` feature — medium surgery:
`literata()` and `body_font` return `&'static BitmapFont`.

- Impact: a board switch drops from 1.45 MB gz to ~70 KB (code only) once
  `fonts.bin` is cached; the Pages artifact roughly halves.
- Risk: touches the firmware-shared `display` crate and must not perturb the
  `no_std` device build — feature-gate it. Pixel-identical goldens on both
  boards are what prove the refactor.
- The alternative (a single wasm with runtime geometry) is **blocked**:
  `display::{WIDTH, HEIGHT}` are compile-time constants flipped by
  `device-x3`.

## Done

- **F1** — books are fetched at runtime instead of `include_str!`ed into each
  wasm. All 8 texts were compiled into *both* board builds, but boot needs
  only the "Continue" book, and the emulator already shows a loading plate
  behind a simulated 650 ms card latency — real fetch latency hides behind
  exactly that UX. The raw C ABI gained a delivery pair
  (`x4_book_alloc(len) -> ptr` + `x4_book_ready(index)`), preserving the
  deliberate no-wasm-bindgen design.
- **F2** — closed the CI golden coverage hole. `tools/emulator` is its own
  cargo workspace, so `cargo test --workspace` never touched it: the 14
  `fixtures/golden/reading-*.png` typography goldens and 8 emulator unit tests
  ran in **no workflow at all**. Now in the golden-frames job for both boards,
  with the duplicate checks dropped from pages.yml.
- **F3** — the wasm download starts earlier. The fetch used to begin only when
  the bottom-of-page module script ran, after a render-blocking Google Fonts
  stylesheet round-trip; a head script now reads `?board=` and injects the
  right preload, and the fonts CSS is non-blocking.
- **F4** — `wasm-opt -Oz --strip-debug --strip-producers` in the build, and
  the retained `name` / `producers` / `target_features` sections stripped.
  Honest but small (~20–40 KB raw per board) — data dominates.
- **F6** — golden-harness robustness: documented per-board `--target-dir`
  aliases (alternating X4/X3 checks shared `tools/emulator/target` and the
  feature flip recompiled display→ui→emulator each way), and `compare_png`
  now compares decoded pixels rather than encoded PNG bytes, so a `png` crate
  encoder change cannot fail every golden misleadingly.

## Do not re-propose

- **Optimizing the golden runner's speed.** Measured: 24 scenarios in 0.52 s,
  full tests 1.3 s warm. Dev-loop speed is not a problem here.
- **A single wasm with runtime board geometry** — blocked by the compile-time
  `WIDTH`/`HEIGHT` constants. See F5.

## Prior art

`docs/plans/2026-07-08-custom-fonts-investigation.md` quantifies the font
weight from the flash side; F5 is its web-specific form.
