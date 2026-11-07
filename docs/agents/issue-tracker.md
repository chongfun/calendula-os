# Issue tracker: GitHub Issues

Issues and PRDs for this repo are GitHub issues on `chongfun/calendula-os`.
Pass `-R chongfun/calendula-os` to every `gh issue` command. The repo is a
fork of `Jon-Vii/marigold-os`, and in a checkout with no default repo set,
`gh` can resolve to the parent and file or read the issue there.

## Labels

Every issue gets:

- **One kind:** `bug`, `enhancement` (features and PRDs) or `perf`
  (optimizations).
- **At least one area:** `area:display`, `area:reader`, `area:power`,
  `area:storage`, `area:network`, `area:memory`, `area:tooling`, `area:app`,
  `area:typography`, `area:library`, `area:input` or `area:platform`.

Open issues also get exactly one triage label from `triage-labels.md`.
Other labels:

- `needs-x3-run` when the issue owes a check on the X3.
- `needs-hardware` when it needs hardware the project does not have (X4,
  a UC8279d panel, an S3 board). A bug filed under "Other / experimental
  hardware" usually gets `needs-hardware` at triage.
- `source:marigold-os`, `source:crosspoint` or `source:freeink` when the idea
  came from upstream.
- On closing as not planned, one of `wontfix` (declined), `reverted` (landed,
  then reverted) or `obsolete` (main changed so it no longer applies).

## Writing an issue

- Title the change, in the imperative. An upstream item is titled
  `[<source> <sha>] <summary>`. Issues migrated from the old optimization
  roadmap keep their roadmap ID, like `[A12]`.
- Start the body with a dated status line, and edit it when the state changes.
  Put measurements and discussion in comments.
- Write our PRs bare, like #92, so they link. Write upstream PRs and commits
  in backticks, like `crosspoint #3463`. A bare `#3463` links to this repo,
  and a full upstream URL adds a "mentioned this" entry on their PR.
- A PRD is one `enhancement` issue. Its milestones are sub-issues.
- Record blockers with GitHub's "blocked by" links, not in prose alone.
- Priority (P1 to P3) and effort (S, M, L) are fields on the project board.

## When a skill says "publish to the issue tracker"

Create the issue with `gh issue create -R chongfun/calendula-os`, following
the rules above. Add `needs-triage` unless the skill sets a different triage
label.

## When a skill says "fetch the relevant ticket"

Run `gh issue view <number> -R chongfun/calendula-os --comments`. The user will
normally pass the issue number.

## Before proposing an optimization

Search the declines first:
`gh issue list -R chongfun/calendula-os --state closed --label wontfix --search "<term>"`.
Many ideas were measured and rejected, and the closed issue says why. Then
read `docs/performance-method.md`.

## Upstream sweeps

Three pinned issues, one per upstream, hold the last swept commit and the
steps for the next sweep.
