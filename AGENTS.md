# Working contract

Before changing code:

- Read `docs/ARCHITECTURE.md`, `docs/IMPLEMENTATION_PLAN.md`, and any relevant ADRs or files under `docs/agents/`.
- Inspect the existing implementation and nearby tests before designing a replacement.
- Keep changes scoped to the requested work. Do not rewrite unrelated code or regenerate golden files unless the change intentionally affects them.
- The repository defaults Cargo to the firmware target. Determine the local host with `rustc -vV` and pass an explicit `--target` to every host-side Cargo command.

## Definition of done

Do not declare a task complete until you have:

1. Run the checks applicable to the changed code.
2. Fixed failures introduced by your change.
3. Reviewed the final diff for accidental files, formatting churn, generated artifacts, and unrelated edits.
4. Reported every verification command you ran and whether it passed.
5. Explicitly reported any applicable check you could not run and the reason.

Never say that tests, lint, builds, or visual checks pass unless you actually ran them.

Use the repository verification entry points:

- `tools/check.sh fmt` for formatting.
- `tools/check.sh fast` for normal Rust changes.
- `tools/check.sh emulator` for UI, layout, rendering, typography, reader-state, or golden-frame changes.
- `tools/check.sh firmware` for firmware, HAL, board-specific, feature-gated, or release-sensitive changes.
- `tools/check.sh all` before a pull request is considered ready.

Changes affecting boards, rendering, or firmware must be checked for both X4 and X3.

Do not push, tag, publish a release, rewrite history, or discard user changes without explicit permission.

## Agent skills

### Issue tracker

Issues and PRDs are tracked as local markdown under `.scratch/`. See `docs/agents/issue-tracker.md`.

### Triage labels

This repo uses the default mattpocock/skills triage vocabulary. See `docs/agents/triage-labels.md`.

### Domain docs

This is a single-context repo: read the domain and architecture docs in `docs/`, plus `docs/adr/` if present. See `docs/agents/domain.md`.

### Cutting a release

Releases are tag-triggered and CI-built; `tools/prepare-release.sh <version>`
first syncs the crate version and the site's version/size labels so the
descriptor stamp and page don't lie. Never pre-create the GitHub release — the
workflow creates it, and Pages can't deploy without a populated release. See
`docs/agents/release.md`.

### Bench workflow

Development bench runs use `tools/bench/bench.py` and structured `bench:` serial
telemetry. See `docs/agents/bench.md`.

### Visual & Layout Changes Verification

Visual, layout, rendering, and typography changes are verified locally against the
emulator's golden frames, on both the X4 and the X3. Host-side cargo commands need
an explicit `--target`. See `docs/agents/visual-verification.md`.
