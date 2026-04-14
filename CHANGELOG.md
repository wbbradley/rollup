# Changelog

All notable changes to this project are documented here.

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
