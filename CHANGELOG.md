# Changelog

All notable changes to this project are documented here.

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
