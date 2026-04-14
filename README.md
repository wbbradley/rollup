# rollup

A terminal dashboard for your GitHub PR review workload. Two side-by-side panes:

- **Review requested of me** — open PRs waiting on your review.
- **Authored by me** — your open PRs and where each reviewer stands.

Data comes from a single `gh api graphql` call, so auth is whatever `gh` already has.

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
| `Tab`           | Switch focus between the two panes (Me mode only)     |
| `p`             | Switch to People view                                 |
| `Enter`         | Open the selected PR in your browser                  |
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

## License

MIT — see [LICENSE](LICENSE).
