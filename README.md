# rollup

A terminal dashboard for your GitHub PR review workload. Panes:

- **Review requested of me** — open PRs waiting on your review (Radar page, `e`).
- **Authored by me** — your open PRs and where each reviewer stands, nested into a merge-target tree (stacked PRs shown under their base branch), with each source branch shown as a muted `[branch/name]` suffix. Fills the full width of the Me view.
- **Recent releases** — for every repo in your config, the three most recent releases per repo as a tree (Radar page, `e`).
- **Recently merged PRs** — recently merged PRs by people in the current view.

Data comes from `gh api graphql`, so auth is whatever `gh` already has.

There's also a non-interactive `rollup report` subcommand that prints the same
data to stdout.

While the interactive dashboard is running, it also serves a web companion UI:

- <http://127.0.0.1:7011/> — **Authored by me**, with the same repository,
  merge-target, checks, reviewers, comments, and stacked-PR hierarchy.
- <http://127.0.0.1:7011/merged> — the Me view's **Recently merged PRs**.

The server binds only to loopback, starts and stops with the TUI, and never
opens a browser automatically. Use **Refresh** on either page (or press `r` in
the TUI) to start one shared GitHub fetch for both interfaces. The browser shows
loading progress, reloads when the fetch finishes, and keeps each PR section's
expanded or folded state within that tab. A failed refresh leaves the last good
data visible alongside the current error. Refresh only reads GitHub data; the
web UI has no GitHub mutation actions. A port conflict at
`127.0.0.1:7011` exits cleanly before the terminal enters raw mode. The web UI's
scope intentionally excludes the Reviewing, Releases, People, and
reviewer-removal interfaces; those remain in the TUI and `rollup report`.

## Install

```sh
cargo install --path .
```

Requires [`gh`](https://cli.github.com) on your `PATH` and already authenticated (`gh auth status`).

## Run

```sh
rollup
```

Then use the TUI directly or visit <http://127.0.0.1:7011/> in a browser.

The interactive process owns three concurrent pieces: a TUI event loop, worker
threads that fetch GitHub data, and a small synchronous loopback HTTP listener.
The listener sends browser refresh requests to the event loop, which atomically
publishes an immutable loading snapshot before acknowledging the request and
starting the fetch. This keeps HTTP rendering independent of GitHub latency,
prevents overlapping refreshes, and preserves the last successful snapshot when
a later refresh fails. `rollup report` does not start the listener.

## Keys

| Key             | Action                                                |
|-----------------|-------------------------------------------------------|
| `↑` `↓` `k` `j` | Move selection (PR rows *and* reviewer sub-rows)      |
| `l` / `h`       | Expand / collapse the selected section (Authored pane; Right/Left also work) |
| `g` / `G`       | Jump to top / bottom of the pane                      |
| `e`             | Open the Radar page (Review requested + Recent releases) |
| `Tab`           | Cycle focus between Reviewing / Releases on the Radar page (`Shift+Tab` reverses) |
| `p`             | Switch to People view                                 |
| `/`             | Incrementally search/filter the Authored tree (Me view) |
| `Enter`         | Open the selected PR (or comment / check details / release/tag page) in your browser |
| `c`             | Copy an "address this …" agent prompt to the clipboard — the selected open comment (or every open comment under the "Open comments" header), or the selected check (or every failing check under the "Checks" header) (Me view) |
| `x`             | Remove the selected reviewer from the PR              |
| `r`             | Refresh                                               |
| `Esc`           | Cancel/clear Authored search, or return to Me from People/Radar |
| `q`             | Quit                                                  |

## Reviewer rows

Each reviewer appears as a sub-row under its PR:

| Glyph | Meaning             |
|-------|---------------------|
| `+`   | Approved            |
| `x`   | Changes requested   |
| `.`   | Commented           |
| `?`   | No review yet       |
| `-`   | Dismissed           |

Plus a trailing badge:

- `[req]` — GitHub is still asking this person for a review. Removable with `x`.
- `(reviewed)` — in the list because they already submitted a review. `x` can't remove them (GitHub's DELETE endpoint no-ops here — you'd have to dismiss the review on the web UI).

Login names get stable, hash-derived colors so you can scan for a particular person quickly.

## Authored tree

Within each repo, the `Authored by me` pane nests your PRs by **merge target**:
a PR whose base branch is another of your PRs' branch is drawn as a child of
that PR, with `├─`/`└─`/`│` connectors — so stacked PRs read at a glance. A PR
that targets a branch you don't have an open PR for (e.g. `main`) sits at the
top level under its repo header. Each PR label ends with its GitHub source
branch in muted square brackets, such as `Improve rendering [feature/render]`.

Under each PR its children are grouped into up to four ordered sections:

1. **Checks** — a merge-readiness rollup for the PR's head commit. It starts
   expanded when any required or optional check has failed/errored, and starts
   collapsed otherwise; the header shows a glyph + required ratio, e.g.
   `▸ Checks ✓ 4/4 required`. Failure/Error and Pending rows sit directly under
   Checks in attention-first order. Success, Skipped, and Neutral rows are
   grouped under a nested, default-collapsed **Valid Results** node. When a
   check has been retried or re-run, only its latest run is shown. `Enter` on a
   check opens its details page (falling back to the PR). See
   [Checks signal](#checks-signal) below.
2. **Reviewers** — where each reviewer stands (see the glyph table above).
3. **Open comments** — the first comment of every *unresolved* review thread
   (`isResolved == false`), shown as `@author excerpt (path)`. Threads whose
   diff hunk has moved or collapsed are still listed, tagged `[outdated]`.
4. **Stacked PRs** — PRs stacked on this one, each recursing into its own
   sections.

Only non-empty sections appear, in that order. Every non-empty section shows a
selectable `▸`/`▾` header that is also a **collapse control**: `l`/Right expands
it, `h`/Left collapses it. `h`/Left on a child row (check, reviewer, comment, or
nested PR) collapses its enclosing section and moves the cursor back to that
section's header. A valid check collapses back to **Valid Results**; an
actionable check collapses back to **Checks**. Checks conditionally expands as
described above, while Valid Results and Reviewers start collapsed (Open
comments and Stacked PRs start expanded). The Reviewers header carries a compact
response-state summary — e.g. `▸ Reviewers [req, ✗ changes]` — so a
changes-requested review (`✗`) is visible at a glance without expanding.
Explicit fold state is per-`(PR, section)` and survives background refreshes,
including a check changing between failing and non-failing. `Enter`
on a comment opens that comment's permalink; `Enter` on a check opens its
details; `Enter` on a PR, reviewer, or section header opens the PR. Pressing
`c` on an open comment copies an "address this comment" agent prompt (the
comment's permalink and the PR's source branch) to the system clipboard;
pressing `c` on the "Open comments" header copies one prompt that lists every
open comment under that PR. Likewise, `c` on a single check copies an "address
this check" prompt (its name, its details URL — falling back to the PR URL —
and the PR's source branch, and it works on passing checks too); `c` on the
"Checks" header copies one prompt bulleting every *failing* check under that PR
(and is a no-op when none are failing). The same shape appears in `rollup
report`, with every section expanded so all details are visible (and with text
tokens in the summary).

Press `/` in the Me view to start an incremental Authored-tree search. The
footer changes to `inc search: <query>`, and every printable character or
Backspace immediately recomputes the tree. Matching is case-insensitive over
visible text: repository and PR labels, section labels and summaries,
reviewers, comments, and checks. PR labels include their displayed source branch,
so branch names are searchable; URLs and undisplayed metadata are not searched.
Only matching rows and the ancestor path needed to reach them remain; matches
inside normally collapsed sections are temporarily exposed without changing
your saved collapse state. A matching valid check retains both Checks and Valid
Results as ancestors; matching the Valid Results label retains Checks. Enter
commits the filter so navigation, opening,
and temporary `h`/`l` folding continue to work. Esc cancels an edit or clears a
committed filter and restores the full tree and its prior folds. An empty Enter
means no filter, and `/` while filtered starts a replacement query. The filter
survives background refreshes; `rollup report` and the web UI remain unfiltered.

### Checks signal

The Checks header answers one question: *is this PR allowed to merge, ignoring
the review requirement?* The signal is computed from **branch-protection-required
checks only**:

| Glyph | Meaning                                                                   |
|-------|---------------------------------------------------------------------------|
| `✓`   | Green — every required check passed (or the base branch has none required). |
| `✗`   | Red — at least one required check failed or errored.                      |
| `◉`   | Pending — a required check is still queued/running and none have failed.  |
| `○`   | Unknown — GitHub hasn't computed mergeability/the rollup yet; resolves on refresh. |

A **failing non-required check never turns the signal red**, but it does open
the Checks section so the failure is visible; its row is dimmed and marked
`(not required)`. A PR whose base branch has
no required checks (common for stacked PRs targeting an unprotected feature
branch) shows green `no required checks`. A PR with no checks at all omits the
section entirely. Because it ignores the review requirement, a PR that is only
waiting on a review but whose required checks all pass shows **green**.

## Config

`rollup` reads `~/.config/rollup/config.yaml` (or `$XDG_CONFIG_HOME/rollup/config.yaml`)
at startup. The only field today is `repos`, a list of `owner/name` entries that
drives the Recent releases pane:

```yaml
repos:
  - MystenLabs/walrus
  - MystenLabs/sui
```

The file is optional — without it, the Recent releases pane just shows
`(no configured repos)` and everything else keeps working. Parse errors surface
in the footer status line rather than crashing the app.

The `Recent releases` pane renders as a tree: one header per configured repo,
with up to three of its most recent releases beneath (newest first, e.g.
`v1.2.3 (3d)`). Prereleases carry a trailing `[pre]` marker. Repos with no
releases but at least one tag show a single `tag: v… (…)` row; repos with
neither show `(no releases or tags)`. `Enter` opens the URL of the highlighted
row — each release line points to its own release page. The pane also appears
as a section in `rollup report`.

## License

MIT — see [LICENSE](LICENSE).
