# Issue tracker: GitHub Issues

Issues and PRDs for this repo are GitHub issues on `chongfun/calendula-os`.
Pass `-R chongfun/calendula-os` to every `gh issue` command. The repo is a
fork of `Jon-Vii/marigold-os`, and in a checkout with no default repo set,
`gh` can resolve to the parent and file or read the issue there.

## Labels

Every issue has these by the end of triage. The issue forms set only
`needs-triage`, plus `bug` on the bug form, so triage adds the kind and area
before it replaces `needs-triage` with another triage label:

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
- A PRD is one `enhancement` issue, and each of its milestones is a
  sub-issue, so the parent shows progress. PRDs finished before the
  migration keep a milestone table instead, as their record.
- Record blockers with GitHub's "blocked by" links, not in prose alone.

## Project board

Open issues are planned on the "Calendula roadmap" project,
https://github.com/users/chongfun/projects/3. The issue holds the technical
record, and the board holds the plan.

- Every new issue in this repo joins the board on its own, with Status
  `Todo`. Set `In Progress` when work starts. Closing an issue moves it to
  `Done`.
- Moving a card to `Done` closes its issue as completed. Close a declined
  issue from the issue page instead, as not planned.
- Triage sets two fields when it takes `needs-triage` off:
  - **Priority.** `P1` is next: it blocks other work or trustworthy
    measurement, or it is a regression on main. `P2` comes after the P1s.
    `P3` waits until nearby work touches it.
  - **Effort.** `S` is one small PR. `M` is one larger PR, or a PR and a
    device run. `L` takes several PRs or milestones.

To set the fields from a shell, the token needs the `project` scope
(`gh auth refresh -h github.com -s project`). `item-add` returns the
existing item when the issue is already on the board.

```sh
item=$(gh project item-add 3 --owner chongfun --format json --jq .id \
  --url https://github.com/chongfun/calendula-os/issues/<number>)
gh project item-edit --project-id PVT_kwHOABxOVc4BqbUq --id "$item" \
  --field-id PVTSSF_lAHOABxOVc4BqbUqzhoygdM --single-select-option-id <priority>
gh project item-edit --project-id PVT_kwHOABxOVc4BqbUq --id "$item" \
  --field-id PVTSSF_lAHOABxOVc4BqbUqzhoygdQ --single-select-option-id <effort>
```

| Field | Option IDs |
|---|---|
| Priority | `P1` 5d3c04c6, `P2` becf3ef0, `P3` 47eccec9 |
| Effort | `S` 0c329106, `M` 27e88536, `L` 305b8675 |

## When a skill says "publish to the issue tracker"

Create the issue with `gh issue create -R chongfun/calendula-os`, following
the rules above. Add `needs-triage` unless the skill sets a different triage
label. The board picks the issue up on its own. When the skill also triages
it, set Priority and Effort as above.

## When a skill says "fetch the relevant ticket"

Run `gh issue view <number> -R chongfun/calendula-os --comments`. The user will
normally pass the issue number.

## Before proposing an optimization

Search the prior decisions first. An issue closed as not planned carries
`wontfix` (declined), `reverted` (landed, then reverted) or `obsolete` (main
changed so it no longer applies), and one search covers all three:

```sh
gh issue list -R chongfun/calendula-os --state closed --search 'reason:"not planned" <term>'
```

Drop `reason:"not planned"` to include work that already landed. Read the
matching issues for why each idea stopped, then read
`docs/performance-method.md`.

## Upstream sweeps

Three pinned issues, one per upstream, hold the last swept commit and the
steps for the next sweep.
