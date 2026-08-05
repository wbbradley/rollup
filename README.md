# rollup

A terminal dashboard for your GitHub PR review workload. Panes:

- **Review requested of me** — open PRs waiting on your review (Radar page, `e`).
- **Authored by me** — your open PRs and where each reviewer stands, nested into a merge-target tree (stacked PRs shown under their base branch), with each source branch shown as a muted `[branch/name]` suffix. Fills the full width of the Me view.
- **Recent releases** — for every repo in your config, the three most recent releases per repo as a tree (Radar page, `e`).
- **Recently merged PRs** — recent merges by the authors visible in the current view (you and the authors of the PRs awaiting your review).

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
expanded or folded state within that tab. Authored PRs with children are also
disclosures: folding a PR keeps its heading visible while hiding every nested
section and stacked descendant, without changing their individual folds. A
failed refresh leaves the last good
data visible alongside the current error. Refresh only reads GitHub data; the
web UI has no GitHub mutation actions. A port conflict at
`127.0.0.1:7011` exits cleanly before the terminal enters raw mode. The web UI's
scope intentionally excludes the Reviewing, Releases, and reviewer-removal
interfaces; those remain in the TUI and `rollup report`.

## Install

```sh
cargo install --path .
```

Requires [`gh`](https://cli.github.com) on your `PATH` and already authenticated (`gh auth status`).

## Run

```sh
rollup
```

Then use the TUI directly or visit <http://127.0.0.1:7011/> in a browser. The
TUI footer continuously shows the actual bound endpoint as `web
localhost:7011` for the default loopback listener.

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
| `l` / `h`       | Expand / collapse the selected PR subtree, section, or repo grouping (Authored pane; Right/Left also work) |
| `g` / `G`       | Jump to top / bottom of the pane                      |
| `e`             | Open the Radar page (Review requested + Recent releases) |
| `Tab`           | Cycle focus between Reviewing / Releases on the Radar page (`Shift+Tab` reverses) |
| `p`             | Copy a terse review-request line per PR in the selected Authored subtree — `{url} - {title}` with the conventional-commit prefix stripped and ` - DRAFT` appended for drafts, joined by newlines (Me view; same subtree semantics as `c`) |
| `/`             | Incrementally search/filter the Authored tree (Me view) |
| `Enter`         | Open the selected PR (or comment / check details / repo / release/tag page) in your browser |
| `c`             | Copy one aggregate agent prompt for the selected Authored node's subtree — every unresolved comment and failing check it contains, grouped per PR (a PR includes its whole stack; a Stacked PRs header only its descendants; a repo header every PR in the repo) (Me view) |
| `v`             | Resolve every outdated inline comment in the selected Authored scope, then refresh |
| `x`             | Remove the selected reviewer from the PR              |
| `r`             | Refresh                                               |
| `Esc`           | Cancel/clear Authored search, or return to Me from Radar |
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
   `▸ Checks ✓ 4/4 required`. Failure/Error rows sit directly under Checks in
   attention-first order. Pending rows are grouped under a nested,
   default-collapsed **Pending** node; Success, Skipped, and Neutral rows are
   grouped under a separate, default-collapsed **Valid Results** node. When a
   check has been retried or re-run, only its latest run is shown. `Enter` on a
   check opens its details page (falling back to the PR). See
   [Checks signal](#checks-signal) below.
2. **Reviewers** — where each reviewer stands (see the glyph table above).
   Non-empty human review summaries are nested beneath their reviewer; GitHub
   gives these comments no resolve action. Bot review summaries are omitted.
3. **Open comments** — the first comment of every *unresolved* inline review
   thread (`isResolved == false`), shown as `@author excerpt (path)`. Excerpts
   use the full pane width and end in `…` only when they overflow. Threads whose
   diff hunk has moved or collapsed are still listed, tagged `[outdated]`.
4. **Stacked PRs** — PRs stacked on this one, each recursing into its own
   sections.

An Authored PR with any rendered children has its own `▾`/`▸` disclosure.
Every section beneath it is indented one full tree level past the PR label;
stacked PRs repeat that hierarchy for their own sections.
`h`/Left on the PR keeps that PR row visible while hiding all four sections and
every stacked descendant; `l`/Right restores them with their prior inner fold
choices intact. A nested PR folds itself, while the parent **Stacked PRs**
header remains the control for hiding all sibling stacks together. The web
dashboard provides the same outer PR disclosure and preserves its state within
the browser tab.

Only non-empty sections appear, in that order. Every non-empty section shows a
selectable `▸`/`▾` header that is also a **collapse control**: `l`/Right expands
it, `h`/Left collapses it. `h`/Left on a child row (check, reviewer, comment, or
nested PR) collapses its enclosing section and moves the cursor back to that
section's header. A pending check collapses back to **Pending**, a valid check
to **Valid Results**, and a failing/errored check to **Checks**. Checks
conditionally expands as described above, while Pending, Valid Results, and
Reviewers start collapsed unless a review summary needs attention (Open
comments and Stacked PRs start expanded).
The Reviewers header carries a compact
response-state summary — e.g. `▸ Reviewers [req, ✗ changes]` — so a
changes-requested review (`✗`) is visible at a glance without expanding.
Explicit fold state is per-`(PR, section)` and survives background refreshes,
including a check changing between failing and non-failing. The per-repo
grouping header is itself landable and collapsible: `l`/Right and `h`/Left
expand or collapse the whole repo, hiding or showing all of its PR subtrees
(gather, below, ignores this fold). `Enter` on a comment opens that comment's
permalink; `Enter` on a check opens its details; `Enter` on a PR, reviewer, or
section header opens the PR; `Enter` on a repo header opens the repository.

Pressing `c` on **any** Authored node copies one aggregate agent prompt for that
node's subtree to the system clipboard, gathering every unresolved comment and
failing check it contains, grouped per PR. A PR includes its whole stack; a
**Stacked PRs** header covers only the PRs stacked on it; a **repo header**
covers every PR in the repo; the **Reviewers**, **Open comments**, and **Checks**
headers cover that one PR's review summaries, inline comments, or failing
checks; and a reviewer, single comment, or single check copies just that scope
(a single check works even when it is passing). Nodes with no prompting notion
(such as **Pending** or **Valid Results**) and empty subtrees report
`c: nothing to address here`. Review comments are listed by API ID with a
100-character excerpt of their actual text, followed by a reusable `gh api`
command containing a literal `$comment_id` placeholder. The prompt closes by
asking for a worktree when any relevant branch is not already active. The
section shape also appears in `rollup report`, with every
section expanded so all details are visible (and with text tokens in the
summary); `rollup report` and the web UI have no keybindings, so the `c`
aggregation is TUI-only.

Pressing `v` resolves outdated inline review threads within the selected scope.
An outdated comment resolves just itself; **Open comments** covers its PR; a PR
row covers that PR and its complete stack; **Stacked PRs** covers descendants
only; and a repo header covers every PR in the repo. Current (non-outdated)
threads and review-summary comments are never changed. Successful or partial
resolution triggers a refresh, so resolved threads disappear from **Open
comments**. This mutation is TUI-only.

Press `/` in the Me view to start an incremental Authored-tree search. The
footer changes to `inc search: <query>`, and every printable character or
Backspace immediately recomputes the tree. Matching is case-insensitive over
visible text: repository and PR labels, section labels and summaries,
reviewers, comments, and checks. PR labels include their displayed source branch,
so branch names are searchable; URLs and undisplayed metadata are not searched.
Only matching rows and the ancestor path needed to reach them remain; matches
inside normally collapsed sections are temporarily exposed without changing
your saved collapse state. A matching pending or valid check retains Checks and
its **Pending** or **Valid Results** ancestor; matching either nested section's
label retains Checks. Enter
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
