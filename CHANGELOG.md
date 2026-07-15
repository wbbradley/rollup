# Changelog

All notable changes to this project are documented here.

## [Unreleased]

### Added
- Authored PR labels now include the GitHub source branch as a muted `[branch/name]` suffix in the TUI, `rollup report`, and web dashboard. Empty source refs are omitted, branch names participate in incremental Authored search, and Reviewing, People, and Recently merged rows remain unchanged.
- TUI-only incremental subtree search for **Authored by me**: `/` opens an `inc search:` prompt, typing and Unicode-safe Backspace immediately filter case-insensitively over visible repo, PR, section-summary, reviewer, comment, and check text, and matching descendants retain and expose their ancestor paths even below normally collapsed sections. Enter commits a navigable filter; Esc cancels or clears it and restores the prior tree/collapse state; empty queries clear the filter, `/` replaces an active query, and committed filters survive refreshes. Reports, other views, and the web UI remain unchanged.
- A loopback-only web companion for interactive `rollup`: `http://127.0.0.1:7011/` mirrors the Authored-by-me repository/stack hierarchy with clickable checks, reviewers, comments, and PRs, while `/merged` shows the Me view's recently merged set. A Refresh button on either page starts the same single-flight fetch as TUI `r`, automatically follows progress through updated data or an error, and preserves tab-local expanded/folded section state across reloads. It retains last-good data after refresh failures, performs no GitHub mutations, and exits cleanly if the port is unavailable. `rollup report` does not start the server.
- Per-PR **Checks** section in the `Authored by me` pane: a collapsible node (first under each PR, collapsed by default) whose header shows an at-a-glance merge-readiness signal computed from **branch-protection-required checks only** — `✓` green (all required pass, or none required), `✗` red (a required check failed), `◉` pending, `○` unknown — plus a `passed/total required` ratio. A failing *non-required* check never turns the signal red; expanding lists every check in attention-first order, with failures and incomplete checks before skipped/successful checks. `Enter` on a check opens its details URL (falling back to the PR); `h`/`l` collapse/expand like the other sections. Required flags come from one extra batched GraphQL call per refresh; PRs with no checks omit the section. Applies to both the TUI and `rollup report`.
- `Authored by me` pane now groups each PR's children into up to three ordered sections — **Reviewers**, **Open comments**, **Stacked PRs** — drawn as a classic ASCII tree. Every non-empty section shows a selectable, collapsible `▸`/`▾` header. `Open comments` lists the first comment of every unresolved review thread (`@author excerpt (path)`), including outdated ones tagged `[outdated]`; `Enter` on a comment opens its permalink. Applies to both the TUI and `rollup report`.
- Collapsible section nodes in the `Authored by me` pane: `l`/Right expands and `h`/Left collapses the selected section header; `h`/Left on a child row (reviewer, comment, or nested PR) collapses its enclosing section and reselects that header. **Reviewers is collapsed by default** (Open comments and Stacked PRs expanded), and its header carries a compact response-state summary — e.g. `Reviewers [req, ✗ changes]` — so a changes-requested review is visible without expanding. Collapse state is per-`(PR, section)` and survives background refreshes. Section headers are now landable; `Enter` on one opens the PR. `rollup report` renders the same nodes with text tokens in the summary.

### Changed
- `rollup report` now expands every Checks, Reviewers, Open comments, and
  Stacked PRs section so all report details are visible in non-interactive
  output.
- Superseded check runs are omitted from PR check summaries and details; when a
  workflow check is retried or re-run, only its latest instance is shown.
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
