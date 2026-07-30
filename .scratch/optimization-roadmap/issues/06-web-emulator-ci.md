# WS-F: Web emulator & CI — page weight, load time, golden coverage

Status (2026-07-30): F1–F4 and F6 are done (#13) — **initial transfer −49% gz,
and the reading goldens now run in CI**, closing a hole where a
reading-surface regression could deploy cleanly. F5 is deferred; it is only
worth it if board-switching matters to anyone.

Owns: `web/`, `tools/web-emulator/`, `tools/build-web.sh`,
`.github/workflows/`, and test-harness fixes in `tools/emulator`. Fully
disjoint from the firmware workstreams — safe to run in parallel with
everything.

## Open

### F5 (L; only after F1, and only if board-switching matters): shared `fonts.bin` across boards

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
