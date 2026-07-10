# Changelog

All notable changes to this project are documented here.

## [Unreleased]

### Added
- `Authored by me` pane now groups each PR's children into up to three ordered sections — **Reviewers**, **Open comments**, **Stacked PRs** — drawn as a classic ASCII tree. A PR with two or more non-empty sections shows a dim header per section; a PR with exactly one shows no header and hangs its items directly (the previous reviewers-only look). `Open comments` lists the first comment of every unresolved review thread (`@author excerpt (path)`), including outdated ones tagged `[outdated]`; `Enter` on a comment opens its permalink. Applies to both the TUI and `rollup report`.

### Changed
- Split the `Review requested of me` and `Recent releases` panes out of the `Me` view onto a new dedicated **Radar** page, reached with `e` (Review requested on top, Recent releases below, both full-width). With its two siblings gone, `Authored by me` now fills the full width of the `Me` view. `Tab`/`Shift+Tab` cycle focus between the two Radar panes (skipping an empty Releases pane); they're no-ops in Me and People. `e` (Radar), `p` (People), and `Esc` (back to Me) now switch pages directly from anywhere. The Recently-merged pane still shows below on every page.
- `Authored by me` pane now nests PRs into a merge-target tree: a PR whose base branch is another of your open PRs' branch renders as a child of that PR, with `├─`/`└─`/`│` connectors. PRs targeting a branch you don't have an open PR for (e.g. `main`) stay at the top level under their repo header. The flat single-PR case gains the same connector glyphs. Applies to both the TUI and `rollup report`.

## [0.2.0] - 2026-04-21

### Breaking Changes
- `Esc` no longer quits the app; use `q`. `Esc` is now the "back" key for People view.

### Added
- `rollup report` subcommand: non-interactive, pipe-friendly stdout rendering of the same panes.
- People-pivot view (`p` to enter, `Esc` to return) grouping PRs by person, showing what each person has authored and is reviewing.
- "Recently merged PRs" pane listing recent merges by people visible in the current view.
- "Recent releases" pane showing, per configured repo, up to the three most recent releases as a tree (newest first). Repos with no releases fall back to the latest tag, or `(no releases or tags)`. Prereleases marked `[pre]`. `Enter` opens the URL of the highlighted row.
- Config file support at `~/.config/rollup/config.yaml` (respects `$XDG_CONFIG_HOME`) with a `repos:` list of `owner/name` entries; parse errors surface in the footer, missing file is fine.
- `Shift+Tab` cycles pane focus in reverse.
- Unicode glyphs for reviewer state.
- `Loading…` placeholder in Recent releases and Recently merged PRs panes during the initial fetch.

### Changed
- Left column now splits 50/50 between `Review requested of me` and `Recent releases` (was: releases got a tiny sliver sized to the repo count).
- `Tab` cycles focus across Reviewing / Authored / Releases (was: toggle between two panes). Empty Releases pane is skipped.
- Reviewer-removal error message tightened when the reviewer already reviewed.
- README updated to document the new panes, keys, config file, and `rollup report`.

### Fixed
- Merged-PR fetch set now includes the viewer, so your own recent merges show up in the Recently merged pane.

## [0.1.0] - 2026-04-14

### Added
- Terminal UI that fetches your open GitHub PRs via `gh api graphql` in a single round trip, split into two side-by-side panes: review-requested and authored.
- PRs grouped by repo, ordered newest-updated first (both within a repo and across repos).
- Reviewer sub-rows under each PR with status glyphs: `+` approved, `x` changes requested, `.` commented, `?` no review yet, `-` dismissed.
- `[req]` / `(reviewed)` badges to distinguish reviewers GitHub is still asking (removable) from reviewers who appear only because they already submitted a review.
- `x` key removes the selected reviewer via `DELETE /pulls/{n}/requested_reviewers` (refuses non-requested rows with a footer message instead of silently no-op'ing).
- Stable hash-derived HSL colors per login, anchored so `wbbradley` lands on a muted orange; every other name is shifted by the same amount so colors stay consistent across runs.
- Vim-style `scrolloff=4` so the viewport only shifts when the selection is within 4 rows of an edge.
- Keybindings: `↑`/`↓`/`j`/`k` move, `g`/`G` jump, `Tab` switch panes, `Enter` opens the PR in the default browser, `r` refreshes, `q`/`Esc` quits.
- Non-blocking fetches on a worker thread so the UI stays responsive during refresh.
