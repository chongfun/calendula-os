# WS-F: Web emulator & CI — page weight, load time, golden coverage

Status: ready-for-agent

Owns: `web/`, `tools/web-emulator/`, `tools/build-web.sh`, `.github/workflows/`, `tools/emulator` (test harness fixes only). Fully disjoint from the firmware workstreams — safe to run in parallel with everything.

Measured baseline (2026-07-09, `_site/`): each board wasm 4.9 MB raw / 1.45 MB gz (Pages serves gzip only, max-age=600). Section split: **data 5.07 MB (98.5%)** — books 1.98 MB (8 texts via `include_str!`; `lastmen.txt` alone 690 KB), fonts ~3.0 MB — code 66 KB, `name` section 12.7 KB. The two board wasms are 99.9% identical data; switching boards re-downloads everything. Golden runner is already fast (24 scenarios in 0.52 s; full tests 1.3 s warm) — dev-loop speed is NOT a problem.

## F1 (Tier 1, M): Fetch books at runtime instead of compiling them in

All 8 texts are `include_str!`ed into *each* wasm (`tools/web-emulator/src/books.rs`), but boot needs only the "Continue" book, and the emulator already shows a loading plate behind a simulated 650 ms card latency (`OPEN_BOOK_MS`) — real fetch latency hides behind exactly that UX. Ship `.txt` files as static assets (`_site/books/`), keep `SHELF` metadata compiled in, extend the raw C ABI with a delivery pair (`x4_book_alloc(len) -> ptr` + `x4_book_ready(index)`); JS fetches and copies into wasm memory (preserves the deliberate no-wasm-bindgen design; `LoadStatus` plumbing already exists). Fetch the default book (alice.txt, ~52 KB gz) in parallel with the wasm.

- Files: `tools/web-emulator/src/books.rs`, `tools/web-emulator/src/lib.rs`, `web/index.html`, `tools/build-web.sh`.
- Impact: wasm 5.15 → ~3.17 MB raw per board; initial transfer 1.45 → ~0.80 MB gz (−45%); Pages artifact −~4 MB (books currently duplicated in both wasms). Other books load per-open behind the existing plate.
- Risk: low-medium — ABI change; boot must tolerate a not-yet-delivered default book (loading-plate state already exists).
- Verify: `ls -l _site/*.wasm`; network waterfall/Lighthouse A/B; manually open all 8 books on both boards; golden checks unaffected (books aren't in `tools/emulator`).

## F2 (Tier 1, S): Close the CI golden coverage hole

`tools/emulator` is its own cargo workspace, so ci.yml's `cargo test --workspace` never touches it and the `golden-frames` job only runs the scenario runner. Result: the 14 `fixtures/golden/reading-*.png` typography goldens (`tools/emulator/tests/reading_golden.rs`) and 8 emulator unit tests run in **no workflow** — a reading-surface regression deploys cleanly. Add `cargo test --manifest-path tools/emulator/Cargo.toml --target "$HOST_TARGET" --no-default-features` plus the `--features device-x3` variant to the golden-frames job (`.github/workflows/ci.yml:60-69`); drop the duplicate golden checks from pages.yml (`:46-49`) — ci.yml already gates the same commit (~20 s/deploy saved). Also: `gh workflow list` showed only Pages and Release registered — confirm the CI workflow is actually active on GitHub.

- Verify: push a deliberate 1-px golden change on a branch; CI must fail. Cost: ~15 s cold.

## F3 (Tier 1, S): Start the wasm download earlier + unblock fonts

The wasm fetch starts only when the bottom-of-page module script runs, after the render-blocking Google Fonts stylesheet round-trip (`web/index.html` head, lines 21-23; script at ~580). Add a tiny head script reading `?board=` that injects `<link rel="preload" as="fetch" type="application/wasm" crossorigin>` for the right board (static preload can't pick it), and make the fonts CSS non-blocking (`media="print" onload` swap, or self-host a subset woff2 — also removes the third-party dependency).

- Impact: the 1.45 MB (later 0.8 MB) download starts ~200–500 ms earlier; more on high-latency links.
- Verify: DevTools waterfall — wasm request begins during HTML parse.

## F4 (Tier 1, S): Strip + `wasm-opt` in the build

Shipped wasm retains `name` (12.7 KB), `producers`, `target_features` sections and has never seen `wasm-opt`. Add best-effort `wasm-opt -Oz --strip-debug --strip-producers` to `tools/build-web.sh` (install binaryen in pages.yml; skip gracefully locally).

- Impact: honest but small — ~20–40 KB raw per board (data dominates). Hygiene win.
- Verify: size diff; load both boards and exercise a full flow (exports must survive).

## F5 (Tier 3, L; only after F1, if board-switching matters): Shared `fonts.bin` across boards

Both wasms embed byte-identical font tables. After F1, move fonts into one fetched `fonts.bin` shared by both boards — requires the `display` font statics to become runtime-initialized references under a `web` feature (medium surgery: `literata()`/`body_font` return `&'static BitmapFont`). Alternative (single wasm, runtime geometry) is blocked: `display::{WIDTH, HEIGHT}` are compile-time consts flipped by `device-x3`.

- Impact: board switch 1.45 MB gz → ~70 KB (code only) once fonts.bin is cached; Pages artifact roughly halves.
- Risk: touches the firmware-shared `display` crate — must not perturb the no_std device build (feature-gate). Pixel-identical goldens on both boards prove the refactor.

## F6 (S, opportunistic): Golden-harness robustness micro-fixes

(1) Alternating X4/X3 local checks share `tools/emulator/target` and the feature flip recompiles display→ui→emulator (~2–6 s each way) — document per-board `--target-dir` aliases. (2) `compare_png` (`tools/emulator/src/main.rs:171-180`) byte-compares *encoded* PNGs — a `png` crate encoder change would fail every golden misleadingly; compare decoded pixels (keep strict equality).

## Prior art

`docs/plans/2026-07-08-custom-fonts-investigation.md` quantifies the font weight (flash-focused; F5 is its web-specific form). No existing doc proposes wasm size reduction, lazy books, or emulator CI coverage — F1–F4 are new. Golden-runner *speed* is a non-problem (measured) — don't optimize it.

Suggested order: F1 → F2 → F3 + F4 (an afternoon combined) → F6 → F5 only if justified.
