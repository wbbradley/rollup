# rollup

A terminal dashboard for your GitHub PR review workload. Two side-by-side panes:

- **Review requested of me** — open PRs waiting on your review.
- **Authored by me** — your open PRs and where each reviewer stands.
- **Recent releases** — for every repo in your config, the latest release and tag plus age (Me mode only).

Data comes from `gh api graphql`, so auth is whatever `gh` already has.

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
| `Tab`           | Cycle focus between Reviewing / Authored / Releases (Me mode; `Shift+Tab` reverses) |
| `p`             | Switch to People view                                 |
| `Enter`         | Open the selected PR (or release/tag page) in your browser |
| `x`             | Remove the selected reviewer from the PR              |
| `r`             | Refresh                                               |
| `Esc`           | Back to Me mode from People view                      |
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

## Config

`rollup` reads `~/.config/rollup/config.yaml` (or `$XDG_CONFIG_HOME/rollup/config.yaml`)
at startup. The only field today is `repos`, a list of `owner/name` entries that
drives the Recent releases pane:

```yaml
repos:
  - MystenLabs/walrus
  - MystenLabs/sui
```

The file is optional — without it, the Recent releases pane is hidden and
everything else keeps working. Parse errors surface in the footer status line
rather than crashing the app.

The `Recent releases` pane shows one row per repo with the latest release name
and age, optionally followed by the most recent tag and its age (e.g.
`owner/name  v1.2.3 (3d)  tag: v1.2.4 (1d)`). Prereleases carry a trailing
`[pre]` marker. `Enter` opens the release URL; tag-only rows open
`/releases/tag/<tag>`; rows with neither fall back to the repo's `/tags` page.
The pane also appears as a section in `rollup report`.

## License

MIT — see [LICENSE](LICENSE).
