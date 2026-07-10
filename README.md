# rollup

A terminal dashboard for your GitHub PR review workload. Panes:

- **Review requested of me** — open PRs waiting on your review (Radar page, `e`).
- **Authored by me** — your open PRs and where each reviewer stands, nested into a merge-target tree (stacked PRs shown under their base branch). Fills the full width of the Me view.
- **Recent releases** — for every repo in your config, the three most recent releases per repo as a tree (Radar page, `e`).
- **Recently merged PRs** — recently merged PRs by people in the current view.

Data comes from `gh api graphql`, so auth is whatever `gh` already has.

There's also a non-interactive `rollup report` subcommand that prints the same
data to stdout.

## Install

```sh
cargo install --path .
```

Requires [`gh`](https://cli.github.com) on your `PATH` and already authenticated (`gh auth status`).

## Run

```sh
rollup
```

## Keys

| Key             | Action                                                |
|-----------------|-------------------------------------------------------|
| `↑` `↓` `k` `j` | Move selection (PR rows *and* reviewer sub-rows)      |
| `g` / `G`       | Jump to top / bottom of the pane                      |
| `e`             | Open the Radar page (Review requested + Recent releases) |
| `Tab`           | Cycle focus between Reviewing / Releases on the Radar page (`Shift+Tab` reverses) |
| `p`             | Switch to People view                                 |
| `Enter`         | Open the selected PR (or release/tag page) in your browser |
| `x`             | Remove the selected reviewer from the PR              |
| `r`             | Refresh                                               |
| `Esc`           | Back to Me from People **or** Radar                   |
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
top level under its repo header.

Under each PR its children are grouped into up to three ordered sections:

1. **Reviewers** — where each reviewer stands (see the glyph table above).
2. **Open comments** — the first comment of every *unresolved* review thread
   (`isResolved == false`), shown as `@author excerpt (path)`. Threads whose
   diff hunk has moved or collapsed are still listed, tagged `[outdated]`.
3. **Stacked PRs** — PRs stacked on this one, each recursing into its own
   sections.

Only non-empty sections appear, in that order. When a PR has two or more
non-empty sections, each gets a dim header; when it has exactly one, the header
is suppressed and the items hang directly under the PR (e.g. a reviewers-only
PR looks unchanged). `Enter` on a comment opens that comment's permalink;
`Enter` on a PR or reviewer opens the PR. Navigation and selection work across
all the nested rows (section headers are not landable). The same shape appears
in `rollup report`.

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
