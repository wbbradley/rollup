use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use chrono::{DateTime, Local};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{DefaultTerminal, widgets::ListState};

use crate::{
    config,
    github::{self, Data},
    model::{
        CheckState, CheckStatus, Pr, PrComment, PrTreeNode, RepoReleaseInfo, ReviewerKind,
        ReviewerStatus, authored_tree, group_by_repo,
    },
    report::{self, Row, Section, SectionId},
    ui, web,
};

fn selected_release_url(state: &AppState) -> Option<String> {
    let section = state.releases_section();
    let sel = state.releases_sel;
    let mut idx = 0usize;
    for row in &section.rows {
        if !row.is_selectable() {
            continue;
        }
        if idx == sel {
            return match row {
                Row::RepoHeader { repo, .. } => Some(format!("https://github.com/{repo}")),
                Row::ReleaseEntry { release, .. } => {
                    if release.url.is_empty() {
                        None
                    } else {
                        Some(release.url.clone())
                    }
                }
                Row::ReleaseTag { repo, tag, .. } => Some(format!(
                    "https://github.com/{}/releases/tag/{}",
                    repo, tag.name
                )),
                _ => None,
            };
        }
        idx += 1;
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Authored,
    Reviewing,
    Releases,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Me,
    People,
    Radar,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AuthoredSearch {
    #[default]
    Normal,
    Editing(String),
    Filtered(String),
}

impl AuthoredSearch {
    pub fn query(&self) -> Option<&str> {
        match self {
            Self::Normal => None,
            Self::Editing(query) | Self::Filtered(query) => Some(query),
        }
    }
}

pub enum Msg {
    Fetched(Result<Data>),
    Action { label: String, result: Result<()> },
    WebRefresh { acknowledged: Sender<()> },
}

pub struct AppState {
    pub viewer: Option<String>,
    pub authored: Vec<Pr>,
    pub reviewing: Vec<Pr>,
    /// Recently merged PRs authored by people visible in the current view.
    /// Fetched as a superset (People-mode set) and filtered per view at
    /// render time. Non-interactive.
    pub merged: Vec<Pr>,
    pub loaded_at: Option<DateTime<Local>>,
    pub error: Option<String>,
    pub status: Option<String>,
    pub loading: bool,
    pub focus: Focus,
    pub mode: ViewMode,
    pub authored_sel: usize,
    pub reviewing_sel: usize,
    pub people_sel: usize,
    pub releases_sel: usize,
    pub releases: Vec<RepoReleaseInfo>,
    /// Explicit collapse-state choices for Authored section nodes, keyed by
    /// `(repo, number, SectionId)`. Not reset by `apply`, so user choices
    /// survive background refetches and data-driven default changes.
    pub toggled: report::ToggledSet,
    /// Incremental/committed filter state for the TUI's Authored pane.
    pub authored_search: AuthoredSearch,
    /// Temporary folds made inside a committed filtered tree. Kept separate
    /// from `toggled` so clearing search restores the prior collapse state.
    pub search_collapsed: report::ToggledSet,
    /// Persisted across frames so scroll offset is sticky — `ui` only moves
    /// the viewport when the selection crosses a scroll-margin boundary.
    pub authored_list_state: ListState,
    pub reviewing_list_state: ListState,
    pub people_list_state: ListState,
    pub releases_list_state: ListState,
    /// Each interactive pane's visible inner height (rows), recorded by `ui`
    /// during the last draw. Used to size half-page jumps (`PageUp`/`PageDown`,
    /// `Ctrl-U`/`Ctrl-D`). Zero until the first frame is drawn.
    pub authored_page: usize,
    pub reviewing_page: usize,
    pub people_page: usize,
    pub releases_page: usize,
}

impl AppState {
    fn new() -> Self {
        Self {
            viewer: None,
            authored: Vec::new(),
            reviewing: Vec::new(),
            merged: Vec::new(),
            loaded_at: None,
            error: None,
            status: None,
            loading: true,
            focus: Focus::Authored,
            mode: ViewMode::Me,
            authored_sel: 0,
            reviewing_sel: 0,
            people_sel: 0,
            releases_sel: 0,
            releases: Vec::new(),
            toggled: report::ToggledSet::new(),
            authored_search: AuthoredSearch::Normal,
            search_collapsed: report::ToggledSet::new(),
            authored_list_state: ListState::default(),
            reviewing_list_state: ListState::default(),
            people_list_state: ListState::default(),
            releases_list_state: ListState::default(),
            authored_page: 0,
            reviewing_page: 0,
            people_page: 0,
            releases_page: 0,
        }
    }

    fn apply(&mut self, data: Data) {
        self.viewer = Some(data.viewer);
        self.authored = data.authored;
        self.reviewing = data.reviewing;
        self.merged = data.merged;
        self.releases = data.releases;
        self.loaded_at = Some(Local::now());
        self.error = None;
        let mut notes: Vec<String> = Vec::new();
        if let Some(err) = data.config_error {
            notes.push(format!("config: {err}"));
        }
        notes.extend(data.warnings.into_iter().map(|w| format!("warning: {w}")));
        self.status = (!notes.is_empty()).then(|| notes.join(" · "));
        self.loading = false;
        self.clamp_selection();
    }

    fn fail(&mut self, err: String) {
        self.error = Some(err);
        self.loading = false;
    }

    fn clamp_selection(&mut self) {
        let a_len = count_selectable(&self.authored_section());
        let r_len = count_selectable(&self.reviewing_section());
        let rel_len = count_selectable(&self.releases_section());
        if a_len == 0 {
            self.authored_sel = 0;
        } else if self.authored_sel >= a_len {
            self.authored_sel = a_len - 1;
        }
        if r_len == 0 {
            self.reviewing_sel = 0;
        } else if self.reviewing_sel >= r_len {
            self.reviewing_sel = r_len - 1;
        }
        if rel_len == 0 {
            self.releases_sel = 0;
            // If focus landed on an empty Releases pane, bounce back to
            // Reviewing so the user isn't stuck on a pane with no rows.
            if self.focus == Focus::Releases {
                self.focus = Focus::Reviewing;
            }
        } else if self.releases_sel >= rel_len {
            self.releases_sel = rel_len - 1;
        }
        // PLAN "Refresh while in People mode: Reset selection to top." —
        // easiest correct answer when the underlying data just changed.
        self.people_sel = 0;
    }

    pub fn viewer_str(&self) -> &str {
        self.viewer.as_deref().unwrap_or("")
    }

    pub fn reviewing_section(&self) -> Section<'_> {
        report::build_section_reviewing(&self.reviewing)
    }

    pub fn authored_section(&self) -> Section<'_> {
        match self
            .authored_search
            .query()
            .filter(|query| !query.is_empty())
        {
            Some(query) => report::build_section_authored_filtered(
                &self.authored,
                self.viewer_str(),
                query,
                &self.search_collapsed,
            ),
            None => {
                report::build_section_authored(&self.authored, self.viewer_str(), &self.toggled)
            }
        }
    }

    pub fn people_section(&self) -> Section<'_> {
        report::build_section_people(&self.authored, &self.reviewing, self.viewer_str())
    }

    pub fn releases_section(&self) -> Section<'_> {
        report::build_section_releases(&self.releases, chrono::Utc::now())
    }

    fn current_section(&self) -> Section<'_> {
        match self.mode {
            ViewMode::Me => self.authored_section(),
            ViewMode::People => self.people_section(),
            ViewMode::Radar => match self.focus {
                Focus::Releases => self.releases_section(),
                _ => self.reviewing_section(),
            },
        }
    }

    fn current_len(&self) -> usize {
        count_selectable(&self.current_section())
    }

    fn current_sel(&self) -> usize {
        match self.mode {
            ViewMode::Me => self.authored_sel,
            ViewMode::People => self.people_sel,
            ViewMode::Radar => match self.focus {
                Focus::Releases => self.releases_sel,
                _ => self.reviewing_sel,
            },
        }
    }

    fn current_sel_mut(&mut self) -> &mut usize {
        match self.mode {
            ViewMode::Me => &mut self.authored_sel,
            ViewMode::People => &mut self.people_sel,
            ViewMode::Radar => match self.focus {
                Focus::Releases => &mut self.releases_sel,
                _ => &mut self.reviewing_sel,
            },
        }
    }

    /// Visible inner height of the pane the selection currently drives, as last
    /// recorded by `ui`. Mirrors `current_sel`'s routing.
    fn current_page(&self) -> usize {
        match self.mode {
            ViewMode::Me => self.authored_page,
            ViewMode::People => self.people_page,
            ViewMode::Radar => match self.focus {
                Focus::Releases => self.releases_page,
                _ => self.reviewing_page,
            },
        }
    }

    /// Selection delta for a half-page jump: half the focused pane's visible
    /// height, floored at 1 so the cursor always moves even in a tiny pane.
    fn page_step(&self) -> i32 {
        (self.current_page() / 2).max(1) as i32
    }
}

/// The row the user has selected, resolved to a semantic target.
enum Selected<'a> {
    /// A per-repo grouping header (Authored/Reviewing/Releases panes). Carries
    /// the repo's `owner/name` as an owned `String` — the header's repo name is
    /// owned by the `Row`, not by the borrowed PR data, so it can't be a `&'a
    /// str`. `open_selected` opens `https://github.com/{repo}`.
    Repo(String),
    Pr(&'a Pr),
    Reviewer(&'a Pr, &'a ReviewerStatus),
    // The `SectionId` documents which header resolved and is asserted in tests;
    // `open_selected` opens the parent PR regardless, so prod code ignores it.
    Section(&'a Pr, #[allow(dead_code)] SectionId),
    /// A comment row. Carries its PR so the copy-prompt handler can reach the
    /// owning PR's source branch (`head_ref`); mirrors `Check`.
    Comment(&'a Pr, &'a PrComment),
    /// A check row. Carries its PR so `open_selected` can fall back to the PR
    /// URL when the check has no details URL.
    Check(&'a Pr, &'a CheckStatus),
}

/// Walk `rows`' selectable entries, tracking each PR's parent (so reviewer and
/// section-header rows can locate their PR), and return the `sel`-th selectable
/// row as a [`Selected`]. A free function (not an `&self` method) so it borrows
/// the caller-owned `Section` local, keeping the returned `'a` references tied
/// to it. It counts Pr, Reviewer, SectionHeader, Comment, **and** Check rows so
/// its index stays in lock-step with `ui`'s `is_selectable()` count.
fn selected_row<'a>(rows: &[Row<'a>], sel: usize) -> Option<Selected<'a>> {
    let mut idx = 0usize;
    let mut current_pr: Option<&'a Pr> = None;
    for row in rows {
        match row {
            Row::RepoHeader { repo, .. } => {
                // A new repo grouping starts; drop the prior PR so a stray
                // section header can't attribute to it.
                current_pr = None;
                if idx == sel {
                    return Some(Selected::Repo(repo.clone()));
                }
                idx += 1;
            }
            Row::Pr { pr, .. } => {
                current_pr = Some(pr);
                if idx == sel {
                    return Some(Selected::Pr(pr));
                }
                idx += 1;
            }
            Row::Reviewer { r, .. } => {
                if idx == sel {
                    return current_pr.map(|pr| Selected::Reviewer(pr, r));
                }
                idx += 1;
            }
            Row::SectionHeader { section, .. } => {
                if idx == sel {
                    return current_pr.map(|pr| Selected::Section(pr, *section));
                }
                idx += 1;
            }
            Row::Comment { c, .. } => {
                if idx == sel {
                    return current_pr.map(|pr| Selected::Comment(pr, c));
                }
                idx += 1;
            }
            Row::Check { c, .. } => {
                if idx == sel {
                    return current_pr.map(|pr| Selected::Check(pr, c));
                }
                idx += 1;
            }
            _ => {}
        }
    }
    None
}

/// A collapse/expand target resolved from the selected row (or the parent
/// context of a child row). `is_header` is true when the selected row is the
/// section header itself; false when it's a child of the section (a reviewer,
/// comment, or nested PR) whose enclosing section should be collapsed.
struct SectionCtx {
    repo: String,
    number: u64,
    section: SectionId,
    is_header: bool,
}

/// Resolve the selected row to the section it belongs to. Walks in lock-step
/// with `is_selectable`, tracking the current PR. A repo header resolves to its
/// own `(repo, 0, Repo)` node; a nested child PR resolves to its parent's
/// Stacked PRs node (via `stacked_under`); a root PR resolves to `None` (PRs
/// aren't collapsible).
fn section_ctx_at(rows: &[Row<'_>], sel: usize) -> Option<SectionCtx> {
    let mut idx = 0usize;
    let mut current_pr: Option<&Pr> = None;
    let mut current_check_section = SectionId::Checks;
    for row in rows {
        match row {
            Row::RepoHeader { repo, expanded } => {
                current_pr = None;
                current_check_section = SectionId::Checks;
                if idx == sel {
                    // Only a collapsible repo header (Authored pane) is a
                    // toggle target; a plain header (`expanded: None`) is not.
                    return expanded.map(|_| SectionCtx {
                        repo: repo.clone(),
                        number: 0,
                        section: SectionId::Repo,
                        is_header: true,
                    });
                }
                idx += 1;
            }
            Row::Pr {
                pr, stacked_under, ..
            } => {
                current_pr = Some(pr);
                current_check_section = SectionId::Checks;
                if idx == sel {
                    return stacked_under.map(|parent| SectionCtx {
                        repo: pr.repo.clone(),
                        number: parent,
                        section: SectionId::Stacked,
                        is_header: false,
                    });
                }
                idx += 1;
            }
            Row::SectionHeader { section, .. } => {
                if matches!(section, SectionId::Checks | SectionId::ValidResults) {
                    current_check_section = *section;
                }
                if idx == sel {
                    return current_pr.map(|pr| SectionCtx {
                        repo: pr.repo.clone(),
                        number: pr.number,
                        section: *section,
                        is_header: true,
                    });
                }
                idx += 1;
            }
            Row::Reviewer { .. } => {
                if idx == sel {
                    return current_pr.map(|pr| SectionCtx {
                        repo: pr.repo.clone(),
                        number: pr.number,
                        section: SectionId::Reviewers,
                        is_header: false,
                    });
                }
                idx += 1;
            }
            Row::Comment { .. } => {
                if idx == sel {
                    return current_pr.map(|pr| SectionCtx {
                        repo: pr.repo.clone(),
                        number: pr.number,
                        section: SectionId::Comments,
                        is_header: false,
                    });
                }
                idx += 1;
            }
            Row::Check { .. } => {
                if idx == sel {
                    return current_pr.map(|pr| SectionCtx {
                        repo: pr.repo.clone(),
                        number: pr.number,
                        section: current_check_section,
                        is_header: false,
                    });
                }
                idx += 1;
            }
            _ => {}
        }
    }
    None
}

/// The selectable index of the section header matching `(repo, number,
/// section)`, or `None` if no such header is present.
fn header_sel_index(
    rows: &[Row<'_>],
    repo: &str,
    number: u64,
    section: SectionId,
) -> Option<usize> {
    let mut idx = 0usize;
    let mut current_pr: Option<&Pr> = None;
    for row in rows {
        match row {
            Row::RepoHeader { repo: r, .. } => {
                current_pr = None;
                if section == SectionId::Repo && r == repo {
                    return Some(idx);
                }
                idx += 1;
            }
            Row::Pr { pr, .. } => {
                current_pr = Some(pr);
                idx += 1;
            }
            Row::SectionHeader { section: s, .. } => {
                if *s == section
                    && current_pr.is_some_and(|pr| pr.repo == repo && pr.number == number)
                {
                    return Some(idx);
                }
                idx += 1;
            }
            other if other.is_selectable() => idx += 1,
            _ => {}
        }
    }
    None
}

fn count_selectable(section: &Section<'_>) -> usize {
    section.rows.iter().filter(|r| r.is_selectable()).count()
}

pub fn run() -> Result<()> {
    // Read the auto-refresh cadence once at startup. A malformed config still
    // surfaces to the user via the per-fetch `config_error` path (github::fetch
    // reloads config itself); here we only need the interval, so fall back to
    // the default rather than fail the whole app.
    let refresh_interval = config::load()
        .map(|cfg| cfg.refresh_interval)
        .unwrap_or(config::DEFAULT_REFRESH_INTERVAL);
    let (tx, rx) = mpsc::channel::<Msg>();
    // Bind before entering raw terminal mode so a port conflict returns a clean
    // contextual error without requiring terminal restoration.
    let web_server = web::start(web::DEFAULT_ADDRESS, tx.clone())?;
    let web_snapshots = web_server.snapshots();
    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal, &web_snapshots, &tx, &rx, refresh_interval);
    ratatui::restore();
    result
}

fn run_app(
    terminal: &mut DefaultTerminal,
    web_snapshots: &web::SnapshotStore,
    tx: &Sender<Msg>,
    rx: &Receiver<Msg>,
    refresh_interval: Duration,
) -> Result<()> {
    let mut state = AppState::new();
    web_snapshots.publish(web::WebSnapshot::from_app(&state));
    spawn_fetch(tx);
    // The startup fetch counts as a refresh, so the first auto-refresh is one
    // interval out. `request_refresh` resets this whenever a refresh actually
    // launches (timer, manual `r`, web Refresh, or a post-action reload), so the
    // deadline always means "one interval since the last refresh."
    let mut next_refresh = Instant::now() + refresh_interval;

    let mut dirty = true;
    loop {
        let drained = drain_msgs(rx, &mut state, tx, web_snapshots, &mut spawn_fetch);
        if drained.changed {
            web_snapshots.publish(web::WebSnapshot::from_app(&state));
        }
        // A web Refresh or post-action reload launched inside `drain_msgs`
        // restarts the interval clock too.
        if drained.launched_refresh {
            next_refresh = Instant::now() + refresh_interval;
        }

        if should_redraw(dirty, drained.changed, state.loading) {
            terminal.draw(|f| ui::draw(f, &mut state))?;
            dirty = false;
        }

        // Auto-refresh: when the interval elapses, launch a GitHub reload that
        // updates both the TUI and the web snapshot. Runs before event handling
        // so it fires regardless of input state (no pause while editing the
        // Authored search). `request_refresh` is single-flight, so an in-flight
        // fetch is never duplicated; the deadline advances only on a real launch.
        if Instant::now() >= next_refresh
            && request_refresh(&mut state, web_snapshots, || spawn_fetch(tx))
        {
            next_refresh = Instant::now() + refresh_interval;
            dirty = true;
        }

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if handle_authored_search_key(&mut state, key) {
                        web_snapshots.publish(web::WebSnapshot::from_app(&state));
                        dirty = true;
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Esc => {
                            if state.mode == ViewMode::People || state.mode == ViewMode::Radar {
                                state.mode = ViewMode::Me;
                                state.focus = Focus::Authored;
                            }
                        }
                        KeyCode::Char('p') => {
                            state.mode = ViewMode::People;
                            state.people_sel = 0;
                        }
                        KeyCode::Char('e') => {
                            state.mode = ViewMode::Radar;
                            state.focus = Focus::Reviewing;
                            state.reviewing_sel = 0;
                            state.releases_sel = 0;
                        }
                        KeyCode::Char('j') | KeyCode::Down => move_selection(&mut state, 1),
                        KeyCode::Char('k') | KeyCode::Up => move_selection(&mut state, -1),
                        // Half-page jumps sized to the focused pane's height.
                        KeyCode::PageDown => {
                            let step = state.page_step();
                            move_selection(&mut state, step);
                        }
                        KeyCode::PageUp => {
                            let step = state.page_step();
                            move_selection(&mut state, -step);
                        }
                        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            let step = state.page_step();
                            move_selection(&mut state, step);
                        }
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            let step = state.page_step();
                            move_selection(&mut state, -step);
                        }
                        KeyCode::Char('g') => jump(&mut state, true),
                        KeyCode::Char('G') => jump(&mut state, false),
                        KeyCode::Tab => {
                            if state.mode == ViewMode::Radar {
                                focus_next(&mut state);
                            }
                        }
                        KeyCode::BackTab => {
                            if state.mode == ViewMode::Radar {
                                focus_prev(&mut state);
                            }
                        }
                        KeyCode::Enter => open_selected(&state),
                        KeyCode::Char('r') => {
                            if request_refresh(&mut state, web_snapshots, || spawn_fetch(tx)) {
                                next_refresh = Instant::now() + refresh_interval;
                            }
                        }
                        KeyCode::Char('x') => remove_selected_reviewer(&mut state, tx),
                        KeyCode::Char('c') => copy_prompt(&mut state),
                        KeyCode::Char('l') | KeyCode::Right => toggle_section(&mut state, true),
                        KeyCode::Char('h') | KeyCode::Left => toggle_section(&mut state, false),
                        _ => {}
                    }
                    // This includes the transition to loading on manual `r`,
                    // so browsers reflect refresh state before the fetch ends.
                    web_snapshots.publish(web::WebSnapshot::from_app(&state));
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
    }
    Ok(())
}

/// Handle Authored search before global bindings. While editing, every key is
/// consumed so query characters such as `q`, `r`, `p`, and `e` cannot trigger
/// application commands.
fn handle_authored_search_key(state: &mut AppState, key: KeyEvent) -> bool {
    if state.mode != ViewMode::Me {
        return false;
    }

    let editing = matches!(state.authored_search, AuthoredSearch::Editing(_));
    let filtered = matches!(state.authored_search, AuthoredSearch::Filtered(_));
    let plain_char = !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT);

    if editing {
        match key.code {
            KeyCode::Esc => state.authored_search = AuthoredSearch::Normal,
            KeyCode::Enter => {
                let query = match std::mem::take(&mut state.authored_search) {
                    AuthoredSearch::Editing(query) => query,
                    _ => unreachable!("editing state checked above"),
                };
                state.authored_search = if query.is_empty() {
                    AuthoredSearch::Normal
                } else {
                    AuthoredSearch::Filtered(query)
                };
            }
            KeyCode::Backspace => {
                if let AuthoredSearch::Editing(query) = &mut state.authored_search {
                    query.pop();
                }
            }
            KeyCode::Char(character) if plain_char => {
                if let AuthoredSearch::Editing(query) = &mut state.authored_search {
                    query.push(character);
                }
            }
            _ => {}
        }
        reset_authored_search_layout(state);
        return true;
    }

    if filtered && key.code == KeyCode::Esc {
        state.authored_search = AuthoredSearch::Normal;
        state.search_collapsed.clear();
        reset_authored_search_layout(state);
        return true;
    }

    if key.code == KeyCode::Char('/') && plain_char {
        state.authored_search = AuthoredSearch::Editing(String::new());
        state.search_collapsed.clear();
        reset_authored_search_layout(state);
        return true;
    }

    false
}

fn reset_authored_search_layout(state: &mut AppState) {
    let len = count_selectable(&state.authored_section());
    state.authored_sel = state.authored_sel.min(len.saturating_sub(1));
    *state.authored_list_state.offset_mut() = 0;
}

fn should_redraw(dirty: bool, changed: bool, loading: bool) -> bool {
    dirty || changed || loading
}

/// What `drain_msgs` observed this iteration: whether app state changed (drives
/// republish/redraw) and whether a refresh actually launched (drives the
/// auto-refresh deadline reset in `run_app`).
struct Drained {
    changed: bool,
    launched_refresh: bool,
}

fn drain_msgs<F>(
    rx: &Receiver<Msg>,
    state: &mut AppState,
    tx: &Sender<Msg>,
    web_snapshots: &web::SnapshotStore,
    launch_fetch: &mut F,
) -> Drained
where
    F: FnMut(&Sender<Msg>),
{
    let mut changed = false;
    let mut launched_refresh = false;
    while let Ok(msg) = rx.try_recv() {
        match msg {
            Msg::Fetched(Ok(data)) => {
                state.apply(data);
                changed = true;
            }
            Msg::Fetched(Err(e)) => {
                state.fail(format!("{e:#}"));
                changed = true;
            }
            Msg::Action { label, result } => match result {
                Ok(()) => {
                    state.status = Some(format!("{label}: ok"));
                    launched_refresh |= request_refresh(state, web_snapshots, || launch_fetch(tx));
                    changed = true;
                }
                Err(e) => {
                    state.status = Some(format!("{label}: {e:#}"));
                    changed = true;
                }
            },
            Msg::WebRefresh { acknowledged } => {
                let launched = request_refresh(state, web_snapshots, || launch_fetch(tx));
                changed |= launched;
                launched_refresh |= launched;
                // `request_refresh` publishes the loading snapshot before it
                // returns, so the listener can now safely redirect the browser.
                let _ = acknowledged.send(());
            }
        }
    }
    Drained {
        changed,
        launched_refresh,
    }
}

/// Start a full GitHub refresh if one is not already running. Publishing the
/// loading snapshot is part of the transition so every caller has the same
/// ordering guarantee.
fn request_refresh<F>(
    state: &mut AppState,
    web_snapshots: &web::SnapshotStore,
    launch_fetch: F,
) -> bool
where
    F: FnOnce(),
{
    if state.loading {
        return false;
    }
    state.loading = true;
    web_snapshots.publish(web::WebSnapshot::from_app(state));
    launch_fetch();
    true
}

fn spawn_fetch(tx: &Sender<Msg>) {
    let tx = tx.clone();
    thread::spawn(move || {
        let _ = tx.send(Msg::Fetched(github::fetch()));
    });
}

fn move_selection(state: &mut AppState, delta: i32) {
    let len = state.current_len();
    if len == 0 {
        return;
    }
    let sel = state.current_sel() as i32 + delta;
    let sel = sel.clamp(0, len as i32 - 1) as usize;
    *state.current_sel_mut() = sel;
}

fn jump(state: &mut AppState, to_top: bool) {
    let len = state.current_len();
    if len == 0 {
        return;
    }
    *state.current_sel_mut() = if to_top { 0 } else { len - 1 };
}

fn focus_next(state: &mut AppState) {
    let order = [Focus::Reviewing, Focus::Releases];
    focus_step(state, &order);
}

fn focus_prev(state: &mut AppState) {
    let order = [Focus::Releases, Focus::Reviewing];
    focus_step(state, &order);
}

fn focus_step(state: &mut AppState, order: &[Focus]) {
    // Skip the Releases pane when it has no rows — lands on the next
    // non-empty pane instead of stranding the user on an empty list.
    let start = order.iter().position(|f| *f == state.focus).unwrap_or(0);
    for step in 1..=order.len() {
        let next = order[(start + step) % order.len()];
        if next == Focus::Releases && state.releases.is_empty() {
            continue;
        }
        state.focus = next;
        return;
    }
}

fn open_selected(state: &AppState) {
    if state.mode == ViewMode::Radar && state.focus == Focus::Releases {
        if let Some(url) = selected_release_url(state) {
            let _ = open::that(url);
        }
        return;
    }
    let section = state.current_section();
    let url = match selected_row(&section.rows, state.current_sel()) {
        Some(Selected::Repo(repo)) => Some(format!("https://github.com/{repo}")),
        Some(Selected::Pr(pr))
        | Some(Selected::Reviewer(pr, _))
        | Some(Selected::Section(pr, _)) => Some(pr.url.clone()),
        Some(Selected::Comment(_, c)) => Some(c.url.clone()),
        // A check opens its details/target URL, falling back to the PR.
        Some(Selected::Check(pr, c)) => Some(
            c.url
                .clone()
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| pr.url.clone()),
        ),
        None => None,
    };
    if let Some(url) = url {
        let _ = open::that(url);
    }
}

/// Expand (`expand == true`) or collapse (`expand == false`) the section node
/// resolved from the current Authored-pane selection. Expanding acts only on a
/// header row; collapsing also acts on a section's child (reviewer, comment, or
/// nested PR), folding the enclosing section. After toggling, the selection is
/// recomputed against freshly built rows so the cursor lands on the toggled
/// section's header. No-op outside `ViewMode::Me`.
fn toggle_section(state: &mut AppState, expand: bool) {
    if state.mode != ViewMode::Me {
        return;
    }
    // Extract owned key parts so the immutable borrow of the built section ends
    // (NLL) before we mutate `state.toggled` — mirrors `remove_selected_reviewer`.
    let section = state.authored_section();
    let extracted = section_ctx_at(&section.rows, state.authored_sel)
        .map(|c| (c.repo, c.number, c.section, c.is_header));
    let Some((repo, number, sec, is_header)) = extracted else {
        return;
    };
    // Expanding only makes sense on a header; a child row can't be "expanded".
    if expand && !is_header {
        return;
    }
    if matches!(state.authored_search, AuthoredSearch::Filtered(_)) {
        // Repo groupings aren't collapsible while a committed filter is active
        // (the filtered builder always shows them), so `h`/`l` are no-ops there.
        if sec == SectionId::Repo {
            return;
        }
        let key = (repo.clone(), number, sec);
        if expand {
            state.search_collapsed.remove(&key);
        } else {
            state.search_collapsed.insert(key, false);
        }
    } else {
        report::set_expanded(&mut state.toggled, &repo, number, sec, expand);
    }
    // Land the cursor on the toggled section's header in the rebuilt rows.
    let section = state.authored_section();
    if let Some(idx) = header_sel_index(&section.rows, &repo, number, sec) {
        state.authored_sel = idx;
    }
    state.clamp_selection();
}

fn remove_selected_reviewer(state: &mut AppState, tx: &Sender<Msg>) {
    let section = state.current_section();
    // Extract owned copies for the reviewer case only; anything else is a
    // no-op. The owned clones let the immutable borrow of `section` end (NLL)
    // before we write `state.status`.
    let extracted = match selected_row(&section.rows, state.current_sel()) {
        Some(Selected::Reviewer(pr, rv)) => Some((
            pr.repo.clone(),
            pr.number,
            rv.login.clone(),
            rv.kind,
            rv.requested,
        )),
        _ => None,
    };
    let Some((repo_full, number, login, kind, requested)) = extracted else {
        state.status = Some("x: select a reviewer row first".into());
        return;
    };
    if !requested {
        // GitHub's DELETE requested_reviewers endpoint silently no-ops for
        // reviewers that aren't currently in reviewRequests. Don't pretend we
        // did anything — explain.
        state.status = Some(format!(
            "x: {login} already reviewed; nothing to un-request (dismiss the review on GitHub to clear it)"
        ));
        return;
    }
    let Some((owner, repo)) = repo_full.split_once('/') else {
        state.status = Some(format!("x: bad repo '{repo_full}'"));
        return;
    };
    let owner = owner.to_string();
    let repo = repo.to_string();
    let label = format!("remove {login} from {owner}/{repo}#{number}");

    state.status = Some(format!("{label}…"));

    let tx = tx.clone();
    thread::spawn(move || {
        let result = match kind {
            ReviewerKind::User => github::remove_user_reviewer(&owner, &repo, number, &login),
            ReviewerKind::Team => {
                github::remove_team_reviewer(&owner, &repo, number, login.trim_start_matches('@'))
            }
        };
        let _ = tx.send(Msg::Action { label, result });
    });
}

/// The actionable items gathered from a single PR for the aggregate copy
/// prompt: its unresolved comments and its checks-to-address. For container
/// scopes (a PR subtree, a Stacked PRs header, a repo header) `checks` holds the
/// PR's *failing* checks (`Failure`/`Error`); for a single-check selection it
/// holds just that one check regardless of state. A `PrActions` is only created
/// for a PR with at least one item.
struct PrActions<'a> {
    pr: &'a Pr,
    comments: Vec<&'a PrComment>,
    checks: Vec<&'a CheckStatus>,
}

/// Gather one PR's own actionable items — every unresolved comment plus every
/// failing check — returning `None` when the PR has neither.
fn pr_actions_for(pr: &Pr) -> Option<PrActions<'_>> {
    let comments: Vec<&PrComment> = pr.unresolved_comments.iter().collect();
    let checks: Vec<&CheckStatus> = pr
        .checks
        .iter()
        .filter(|c| matches!(c.state, CheckState::Failure | CheckState::Error))
        .collect();
    (!comments.is_empty() || !checks.is_empty()).then_some(PrActions {
        pr,
        comments,
        checks,
    })
}

/// Recursively gather actionable items from a PR node and all its descendant
/// stacked PRs, pre-order (the node itself first, then each child's subtree).
fn gather_actionable<'a>(node: &PrTreeNode<'a>) -> Vec<PrActions<'a>> {
    let mut out = Vec::new();
    if let Some(actions) = pr_actions_for(node.pr) {
        out.push(actions);
    }
    for child in &node.children {
        out.extend(gather_actionable(child));
    }
    out
}

/// Gather actionable items from a node's *descendant* subtrees only — the
/// node's own items are excluded. Used by the Stacked PRs header, which
/// addresses the children stacked on a PR, not the PR itself.
fn gather_children<'a>(node: &PrTreeNode<'a>) -> Vec<PrActions<'a>> {
    node.children.iter().flat_map(gather_actionable).collect()
}

/// Find the tree node for `(repo, number)` anywhere in `nodes` (depth-first).
fn find_node<'t, 'a>(
    nodes: &'t [PrTreeNode<'a>],
    repo: &str,
    number: u64,
) -> Option<&'t PrTreeNode<'a>> {
    for node in nodes {
        if node.pr.repo == repo && node.pr.number == number {
            return Some(node);
        }
        if let Some(found) = find_node(&node.children, repo, number) {
            return Some(found);
        }
    }
    None
}

/// Build one repo's merge-target tree from `authored`, using the same grouping
/// and ordering as the Authored pane so gathered PR groups match what the user
/// sees. The returned nodes borrow `authored` directly, so the intermediate
/// per-repo slice can be dropped.
fn repo_tree<'a>(authored: &'a [Pr], repo: &str) -> Vec<PrTreeNode<'a>> {
    let group = group_by_repo(authored)
        .into_iter()
        .find(|(r, _)| r == repo)
        .map(|(_, prs)| prs)
        .unwrap_or_default();
    authored_tree(&group)
}

/// Build the aggregate "please address the following" agent prompt from a set
/// of per-PR actionable items, or `None` when nothing is actionable. Each PR
/// with ≥1 item becomes a group: an `In {head_ref} (#{number} {title}):`
/// sub-header (the `In {head_ref} ` prefix and parentheses are dropped when the
/// source branch is empty, leaving `#{number} {title}:`), a `- {url}` bullet per
/// comment, then a `- check {name} ({url})` bullet per check (the check's URL,
/// falling back to the owning PR's). The trailer is branch-count aware: with a
/// single distinct branch it asks for one worktree; with more than one it asks
/// for a worktree per branch and to consider a sub-agent per branch.
fn combined_prompt(items: &[PrActions<'_>]) -> Option<String> {
    let groups: Vec<&PrActions> = items
        .iter()
        .filter(|item| !item.comments.is_empty() || !item.checks.is_empty())
        .collect();
    if groups.is_empty() {
        return None;
    }

    let mut out = String::from("Please address the following:\n\n");
    for item in &groups {
        let pr = item.pr;
        if pr.head_ref.is_empty() {
            out.push_str(&format!("#{} {}:\n", pr.number, pr.title));
        } else {
            out.push_str(&format!(
                "In {} (#{} {}):\n",
                pr.head_ref, pr.number, pr.title
            ));
        }
        for comment in &item.comments {
            out.push_str("- ");
            out.push_str(&comment.url);
            out.push('\n');
        }
        for check in &item.checks {
            let url = check.url.as_deref().unwrap_or(pr.url.as_str());
            out.push_str(&format!("- check {} ({url})\n", check.name));
        }
        out.push('\n');
    }

    let mut branches: Vec<&str> = groups
        .iter()
        .map(|item| item.pr.head_ref.as_str())
        .filter(|branch| !branch.is_empty())
        .collect();
    branches.sort_unstable();
    branches.dedup();
    out.push_str(if branches.len() > 1 {
        "Use a worktree for each branch if it is not already active in the current worktree, and consider spawning a separate sub-agent per branch."
    } else {
        "Use a worktree if this branch is not already active in the current worktree."
    });
    Some(out)
}

/// Resolve the current Authored-pane selection to its aggregate copy prompt, or
/// `None` when the selected node's subtree has nothing actionable. Single-PR
/// scopes (a comment, a check, the Open comments / Checks headers) format
/// directly from the already-resolved references; container scopes (a PR
/// subtree, a Stacked PRs header, a repo header) rebuild the merge-target tree
/// so the gather is independent of collapse state.
fn build_copy_prompt(state: &AppState) -> Option<String> {
    let section = state.authored_section();
    let selected = selected_row(&section.rows, state.authored_sel)?;
    match selected {
        // A single comment: one one-comment group.
        Selected::Comment(pr, c) => combined_prompt(&[PrActions {
            pr,
            comments: vec![c],
            checks: vec![],
        }]),
        // A single check, addressed regardless of its state.
        Selected::Check(pr, c) => combined_prompt(&[PrActions {
            pr,
            comments: vec![],
            checks: vec![c],
        }]),
        // The Open comments header: this PR's unresolved comments only.
        Selected::Section(pr, SectionId::Comments) => {
            let comments: Vec<&PrComment> = pr.unresolved_comments.iter().collect();
            combined_prompt(&[PrActions {
                pr,
                comments,
                checks: vec![],
            }])
        }
        // The Checks header: this PR's failing checks only.
        Selected::Section(pr, SectionId::Checks) => {
            let checks: Vec<&CheckStatus> = pr
                .checks
                .iter()
                .filter(|c| matches!(c.state, CheckState::Failure | CheckState::Error))
                .collect();
            combined_prompt(&[PrActions {
                pr,
                comments: vec![],
                checks,
            }])
        }
        // A PR row: that PR plus every descendant stacked PR.
        Selected::Pr(pr) => {
            let tree = repo_tree(&state.authored, &pr.repo);
            let items = find_node(&tree, &pr.repo, pr.number)
                .map(gather_actionable)
                .unwrap_or_default();
            combined_prompt(&items)
        }
        // The Stacked PRs header: the parent's child subtrees only.
        Selected::Section(pr, SectionId::Stacked) => {
            let tree = repo_tree(&state.authored, &pr.repo);
            let items = find_node(&tree, &pr.repo, pr.number)
                .map(gather_children)
                .unwrap_or_default();
            combined_prompt(&items)
        }
        // A repo header: every PR under that repo (all stacks).
        Selected::Repo(repo) => {
            let tree = repo_tree(&state.authored, &repo);
            let items: Vec<PrActions> = tree.iter().flat_map(gather_actionable).collect();
            combined_prompt(&items)
        }
        // Reviewers header, Valid Results header, a reviewer — no prompt notion.
        Selected::Section(_, _) | Selected::Reviewer(_, _) => None,
    }
}

/// Copy `text` to the system clipboard. Creates a fresh handle per call — fine
/// on macOS (the primary platform), where the pasteboard outlives the handle.
fn copy_to_clipboard(text: &str) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text.to_owned())?;
    Ok(())
}

/// `c`: copy one aggregate "please address the following" agent prompt for the
/// selected Authored node's subtree, confirming via the footer status line. The
/// prompt gathers every unresolved comment and failing check in that subtree
/// (a PR includes its whole stack; a Stacked PRs header only its descendants; a
/// repo header every PR in the repo), grouped per PR. Single comments, single
/// checks, and section headers use the same unified format. When the subtree
/// has nothing actionable — including prompt-notion-free nodes (Reviewers, a
/// reviewer, Valid Results) — the footer reads `c: nothing to address here`.
/// Only acts in the Me/Authored pane; a no-op elsewhere (no comment/check/repo
/// gather notion exists in the other views).
fn copy_prompt(state: &mut AppState) {
    if state.mode != ViewMode::Me {
        return;
    }
    state.status = Some(match build_copy_prompt(state) {
        None => "c: nothing to address here".to_string(),
        Some(prompt) => match copy_to_clipboard(&prompt) {
            Ok(()) => "copied to clipboard".to_string(),
            Err(e) => format!("c: clipboard error: {e:#}"),
        },
    });
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use chrono::TimeZone;

    use super::*;
    use crate::model::{ReviewState, ReviewerKind};

    fn authored_pr_with_reviewer_and_comment() -> Pr {
        Pr {
            number: 12,
            title: "Fix the thing".to_string(),
            url: "https://github.com/o/r/pull/12".to_string(),
            is_draft: false,
            repo: "o/r".to_string(),
            base_ref: "main".to_string(),
            head_ref: "feature".to_string(),
            author: "me".to_string(),
            reviewers: vec![ReviewerStatus {
                login: "alice".to_string(),
                kind: ReviewerKind::User,
                state: ReviewState::NoReview,
                requested: true,
            }],
            updated_at: chrono::Utc.timestamp_opt(100, 0).unwrap(),
            merged_at: None,
            unresolved_comments: vec![PrComment {
                author: "carol".to_string(),
                body: "add a test here".to_string(),
                url: "https://github.com/o/r/pull/12#discussion_r1".to_string(),
                path: Some("src/foo.rs".to_string()),
                is_outdated: false,
            }],
            checks: vec![],
            checks_rollup: crate::model::ChecksRollup::Unknown,
        }
    }

    #[test]
    fn selected_row_maps_repo_pr_section_and_comment() {
        // A PR with a reviewer AND a comment. The repo header is now selectable
        // at index 0, so at defaults (Reviewers collapsed, its reviewer row
        // hidden) the selectable order is:
        //   Repo(0), Pr(1), Section(Reviewers,2), Section(Open comments,3),
        //   Comment(4).
        let authored = vec![authored_pr_with_reviewer_and_comment()];
        let section = report::build_section_authored(&authored, "me", &report::ToggledSet::new());

        match selected_row(&section.rows, 0) {
            Some(Selected::Repo(repo)) => assert_eq!(repo, "o/r"),
            _ => panic!("index 0 should be the repo header"),
        }
        match selected_row(&section.rows, 1) {
            Some(Selected::Pr(pr)) => assert_eq!(pr.number, 12),
            _ => panic!("index 1 should be the PR"),
        }
        match selected_row(&section.rows, 2) {
            Some(Selected::Section(pr, id)) => {
                assert_eq!(pr.number, 12);
                assert_eq!(id, SectionId::Reviewers);
            }
            _ => panic!("index 2 should be the Reviewers header"),
        }
        match selected_row(&section.rows, 3) {
            Some(Selected::Section(pr, id)) => {
                assert_eq!(pr.number, 12);
                assert_eq!(id, SectionId::Comments);
            }
            _ => panic!("index 3 should be the Open comments header"),
        }
        match selected_row(&section.rows, 4) {
            Some(Selected::Comment(pr, c)) => {
                assert_eq!(pr.number, 12);
                assert_eq!(c.url, "https://github.com/o/r/pull/12#discussion_r1");
                assert_eq!(c.author, "carol");
            }
            _ => panic!("index 4 should be the comment"),
        }
        // Out of range → None.
        assert!(selected_row(&section.rows, 5).is_none());
    }

    #[test]
    fn selected_row_reaches_reviewer_when_expanded() {
        // Repo(0), Pr(1), Reviewers header(2), Reviewer(3) once expanded.
        let authored = vec![authored_pr_with_reviewer_and_comment()];
        let mut toggled = report::ToggledSet::new();
        report::set_expanded(&mut toggled, "o/r", 12, SectionId::Reviewers, true);
        let section = report::build_section_authored(&authored, "me", &toggled);

        match selected_row(&section.rows, 3) {
            Some(Selected::Reviewer(pr, rv)) => {
                assert_eq!(pr.number, 12);
                assert_eq!(rv.login, "alice");
            }
            _ => panic!("index 3 should be the reviewer once Reviewers is expanded"),
        }
    }

    fn me_state(authored: Vec<Pr>) -> AppState {
        let mut state = AppState::new();
        state.viewer = Some("me".to_string());
        state.authored = authored;
        state.loading = false;
        state
    }

    fn simple_pr(number: u64, base: &str, head: &str, reviewers: Vec<ReviewerStatus>) -> Pr {
        Pr {
            number,
            title: format!("t{number}"),
            url: format!("https://github.com/o/r/pull/{number}"),
            is_draft: false,
            repo: "o/r".to_string(),
            base_ref: base.to_string(),
            head_ref: head.to_string(),
            author: "me".to_string(),
            reviewers,
            updated_at: chrono::Utc.timestamp_opt(100, 0).unwrap(),
            merged_at: None,
            unresolved_comments: vec![],
            checks: vec![],
            checks_rollup: crate::model::ChecksRollup::Unknown,
        }
    }

    fn requested_user(login: &str) -> ReviewerStatus {
        ReviewerStatus {
            login: login.to_string(),
            kind: ReviewerKind::User,
            state: ReviewState::NoReview,
            requested: true,
        }
    }

    #[test]
    fn toggle_l_on_collapsed_reviewers_expands_and_keeps_selection() {
        let mut state = me_state(vec![simple_pr(
            1,
            "main",
            "a",
            vec![requested_user("alice")],
        )]);
        // Selectable: Repo(0), Pr(1), Reviewers header(2). Select the header.
        state.authored_sel = 2;
        toggle_section(&mut state, true);

        assert!(report::is_expanded(
            &state.toggled,
            "o/r",
            1,
            SectionId::Reviewers,
            SectionId::Reviewers.default_expanded()
        ));
        // Selection stays on the header row.
        assert_eq!(state.authored_sel, 2);
        // The reviewer row is now present.
        let section = state.authored_section();
        assert!(
            section
                .rows
                .iter()
                .any(|r| matches!(r, Row::Reviewer { .. }))
        );
    }

    #[test]
    fn toggle_h_on_reviewer_collapses_and_reselects_header() {
        let mut state = me_state(vec![simple_pr(
            1,
            "main",
            "a",
            vec![requested_user("alice")],
        )]);
        // Start expanded so a reviewer row exists.
        report::set_expanded(&mut state.toggled, "o/r", 1, SectionId::Reviewers, true);
        // Selectable: Repo(0), Pr(1), Reviewers header(2), Reviewer(3). Select
        // the reviewer.
        state.authored_sel = 3;
        toggle_section(&mut state, false);

        assert!(!report::is_expanded(
            &state.toggled,
            "o/r",
            1,
            SectionId::Reviewers,
            SectionId::Reviewers.default_expanded()
        ));
        // Selection moved back up to the header.
        assert_eq!(state.authored_sel, 2);
        let section = state.authored_section();
        assert!(
            !section
                .rows
                .iter()
                .any(|r| matches!(r, Row::Reviewer { .. }))
        );
    }

    #[test]
    fn section_ctx_nested_child_resolves_to_parent_stacked_and_h_collapses_it() {
        // A(main←a) root; B(a←b) stacked on A. Stacked PRs expanded by default.
        let mut state = me_state(vec![
            simple_pr(1, "main", "a", vec![]),
            simple_pr(2, "a", "b", vec![]),
        ]);
        // Selectable: Repo(0), Pr#1(1), Stacked header(2), Pr#2(3).
        let section = state.authored_section();
        let ctx = section_ctx_at(&section.rows, 3).expect("nested child resolves");
        assert_eq!(ctx.repo, "o/r");
        assert_eq!(ctx.number, 1); // parent PR
        assert_eq!(ctx.section, SectionId::Stacked);
        assert!(!ctx.is_header);
        drop(section);

        // `h` on the nested child collapses the parent's Stacked node.
        state.authored_sel = 3;
        toggle_section(&mut state, false);
        assert!(!report::is_expanded(
            &state.toggled,
            "o/r",
            1,
            SectionId::Stacked,
            SectionId::Stacked.default_expanded()
        ));
        assert_eq!(state.authored_sel, 2); // back on the Stacked header
        let section = state.authored_section();
        assert!(
            !section
                .rows
                .iter()
                .any(|r| matches!(r, Row::Pr { pr, .. } if pr.number == 2)),
            "collapsing Stacked hides the nested child PR",
        );
    }

    fn pr_with_checks(number: u64, checks: Vec<CheckStatus>) -> Pr {
        Pr {
            checks,
            checks_rollup: crate::model::ChecksRollup::Green,
            ..simple_pr(number, "main", "a", vec![])
        }
    }

    fn check(name: &str, url: Option<&str>, required: bool) -> CheckStatus {
        CheckStatus {
            name: name.to_string(),
            state: crate::model::CheckState::Success,
            url: url.map(str::to_string),
            required,
        }
    }

    #[test]
    fn section_ctx_check_row_resolves_to_checks_section() {
        // An actionable check is a direct child of Checks.
        let mut failing = check("build", Some("https://ci/build"), true);
        failing.state = crate::model::CheckState::Failure;
        let mut state = me_state(vec![pr_with_checks(1, vec![failing])]);
        report::set_expanded(&mut state.toggled, "o/r", 1, SectionId::Checks, true);
        // Selectable: Repo(0), Pr#1(1), Checks header(2), Check(3).
        let section = state.authored_section();
        let ctx = section_ctx_at(&section.rows, 3).expect("check row resolves");
        assert_eq!(ctx.repo, "o/r");
        assert_eq!(ctx.number, 1);
        assert_eq!(ctx.section, SectionId::Checks);
        assert!(!ctx.is_header);

        match selected_row(&section.rows, 3) {
            Some(Selected::Check(pr, c)) => {
                assert_eq!(pr.number, 1);
                assert_eq!(c.name, "build");
            }
            _ => panic!("index 3 should be the check row"),
        }
    }

    #[test]
    fn toggle_h_on_check_collapses_checks_and_reselects_header() {
        let mut failing = check("build", Some("https://ci/build"), true);
        failing.state = crate::model::CheckState::Failure;
        let mut state = me_state(vec![pr_with_checks(1, vec![failing])]);
        report::set_expanded(&mut state.toggled, "o/r", 1, SectionId::Checks, true);
        // Selectable: Repo(0), Pr#1(1), Checks header(2), Check(3). Select the
        // check and collapse via `h`.
        state.authored_sel = 3;
        toggle_section(&mut state, false);
        assert!(!report::is_expanded(
            &state.toggled,
            "o/r",
            1,
            SectionId::Checks,
            false
        ));
        // Cursor moved back to the Checks header (index 2).
        assert_eq!(state.authored_sel, 2);
        let section = state.authored_section();
        assert!(!section.rows.iter().any(|r| matches!(r, Row::Check { .. })));
    }

    #[test]
    fn toggle_h_on_valid_check_collapses_valid_results_and_reselects_nested_header() {
        let mut state = me_state(vec![pr_with_checks(
            1,
            vec![check("build", Some("https://ci/build"), true)],
        )]);
        report::set_expanded(&mut state.toggled, "o/r", 1, SectionId::Checks, true);
        report::set_expanded(&mut state.toggled, "o/r", 1, SectionId::ValidResults, true);
        // Selectable: Repo(0), PR(1), Checks(2), Valid Results(3), valid check(4).
        state.authored_sel = 4;
        let section = state.authored_section();
        let ctx = section_ctx_at(&section.rows, 4).expect("valid check resolves");
        assert_eq!(ctx.section, SectionId::ValidResults);
        drop(section);

        toggle_section(&mut state, false);
        assert_eq!(state.authored_sel, 3);
        assert!(report::is_expanded(
            &state.toggled,
            "o/r",
            1,
            SectionId::Checks,
            false,
        ));
        assert!(!report::is_expanded(
            &state.toggled,
            "o/r",
            1,
            SectionId::ValidResults,
            false,
        ));
        let section = state.authored_section();
        assert!(section.rows.iter().any(|row| matches!(
            row,
            Row::SectionHeader {
                section: SectionId::ValidResults,
                expanded: false,
                ..
            }
        )));
        assert!(
            !section
                .rows
                .iter()
                .any(|row| matches!(row, Row::Check { .. }))
        );
    }

    fn state_for_radar(with_releases: bool) -> AppState {
        let mut state = AppState::new();
        state.mode = ViewMode::Radar;
        state.focus = Focus::Reviewing;
        if with_releases {
            state.releases = vec![RepoReleaseInfo {
                repo: "o/r".to_string(),
                recent_releases: Vec::new(),
                latest_tag: None,
            }];
        }
        state
    }

    #[test]
    fn radar_tab_cycles_reviewing_and_releases() {
        let mut state = state_for_radar(true);
        focus_next(&mut state);
        assert_eq!(state.focus, Focus::Releases);
        focus_next(&mut state);
        assert_eq!(state.focus, Focus::Reviewing);
        focus_prev(&mut state); // Shift+Tab reverses
        assert_eq!(state.focus, Focus::Releases);
    }

    #[test]
    fn radar_tab_skips_empty_releases() {
        let mut state = state_for_radar(false); // releases empty
        focus_next(&mut state);
        assert_eq!(state.focus, Focus::Reviewing); // Releases skipped, stays put
        focus_prev(&mut state);
        assert_eq!(state.focus, Focus::Reviewing);
    }

    #[test]
    fn should_redraw_predicate() {
        assert!(should_redraw(true, false, false)); // dirty alone
        assert!(should_redraw(false, true, false)); // changed alone
        assert!(should_redraw(false, false, true)); // loading alone
        assert!(!should_redraw(false, false, false)); // idle: skip the draw
    }

    #[test]
    fn half_page_jump_moves_by_half_pane_height_and_clamps() {
        // A list longer than the pane, so jumps have room to move.
        let prs: Vec<Pr> = (1..=20)
            .map(|n| simple_pr(n, "main", &format!("f{n}"), vec![]))
            .collect();
        let mut state = me_state(prs);
        let len = state.current_len();
        assert!(len >= 12, "expected a long list, got {len}");

        // Pane shows 10 rows → half-page jump is 5 (what PageDown/Ctrl-D use).
        state.authored_page = 10;
        assert_eq!(state.page_step(), 5);

        state.authored_sel = 0;
        let step = state.page_step();
        move_selection(&mut state, step); // PageDown
        assert_eq!(state.authored_sel, 5);
        let step = state.page_step();
        move_selection(&mut state, step);
        assert_eq!(state.authored_sel, 10);

        let step = state.page_step();
        move_selection(&mut state, -step); // PageUp
        assert_eq!(state.authored_sel, 5);

        // Overshooting either edge clamps instead of wrapping.
        state.authored_sel = len - 2;
        let step = state.page_step();
        move_selection(&mut state, step);
        assert_eq!(state.authored_sel, len - 1);

        state.authored_sel = 1;
        let step = state.page_step();
        move_selection(&mut state, -step);
        assert_eq!(state.authored_sel, 0);
    }

    #[test]
    fn page_step_floors_at_one() {
        let mut state = me_state(vec![simple_pr(1, "main", "f1", vec![])]);
        state.authored_page = 0; // before the first frame records a height
        assert_eq!(state.page_step(), 1);
        state.authored_page = 1; // tiny pane: 1/2 == 0, floored to 1
        assert_eq!(state.page_step(), 1);
    }

    #[test]
    fn drain_msgs_reports_change() {
        let mut state = AppState::new(); // loading: true seed → no fetch spawned
        let (tx, rx) = mpsc::channel::<Msg>();
        let snapshots = web::SnapshotStore::new();
        tx.send(Msg::Action {
            label: "test".into(),
            result: Ok(()),
        })
        .unwrap();

        assert!(drain_msgs(&rx, &mut state, &tx, &snapshots, &mut |_| {}).changed);
        assert_eq!(state.status, Some("test: ok".to_string()));

        // Channel is now empty → nothing drained → no change reported.
        assert!(!drain_msgs(&rx, &mut state, &tx, &snapshots, &mut |_| {}).changed);
    }

    #[test]
    fn shared_refresh_transition_publishes_loading_and_is_single_flight() {
        let mut state = AppState::new();
        state.loading = false;
        let snapshots = web::SnapshotStore::new();
        snapshots.publish(web::WebSnapshot::from_app(&state));
        let launches = Cell::new(0);

        assert!(request_refresh(&mut state, &snapshots, || {
            assert!(snapshots.load().is_loading());
            launches.set(launches.get() + 1);
        }));
        assert!(state.loading);
        assert_eq!(launches.get(), 1);

        assert!(!request_refresh(&mut state, &snapshots, || {
            launches.set(launches.get() + 1);
        }));
        assert_eq!(launches.get(), 1);
    }

    #[test]
    fn web_refresh_is_acknowledged_after_publication_and_duplicates_do_not_launch() {
        let mut state = AppState::new();
        state.loading = false;
        let snapshots = web::SnapshotStore::new();
        snapshots.publish(web::WebSnapshot::from_app(&state));
        let (tx, rx) = mpsc::channel::<Msg>();
        let (first_ack, first_acknowledgment) = mpsc::channel();
        let (second_ack, second_acknowledgment) = mpsc::channel();
        tx.send(Msg::WebRefresh {
            acknowledged: first_ack,
        })
        .unwrap();
        tx.send(Msg::WebRefresh {
            acknowledged: second_ack,
        })
        .unwrap();
        let launches = Cell::new(0);

        let drained = drain_msgs(&rx, &mut state, &tx, &snapshots, &mut |_| {
            assert!(snapshots.load().is_loading());
            launches.set(launches.get() + 1);
        });
        assert!(drained.changed);
        // The first web Refresh launched a fetch; `run_app` uses this to reset
        // the auto-refresh deadline.
        assert!(drained.launched_refresh);
        first_acknowledgment.try_recv().unwrap();
        second_acknowledgment.try_recv().unwrap();
        assert!(snapshots.load().is_loading());
        assert_eq!(launches.get(), 1);
    }

    fn press(state: &mut AppState, code: KeyCode) -> bool {
        handle_authored_search_key(state, KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn slash_starts_search_only_in_me_and_editing_consumes_global_key_characters() {
        let mut state = me_state(vec![simple_pr(1, "main", "a", vec![])]);
        assert!(press(&mut state, KeyCode::Char('/')));
        assert_eq!(
            state.authored_search,
            AuthoredSearch::Editing(String::new())
        );

        for character in ['q', 'r', 'p', 'e', 'j'] {
            assert!(press(&mut state, KeyCode::Char(character)));
        }
        assert_eq!(
            state.authored_search,
            AuthoredSearch::Editing("qrpej".into())
        );
        assert_eq!(state.mode, ViewMode::Me);
        assert!(!state.loading);
        assert_eq!(state.authored_sel, 0);

        let mut radar = state_for_radar(false);
        assert!(!press(&mut radar, KeyCode::Char('/')));
        assert_eq!(radar.authored_search, AuthoredSearch::Normal);
    }

    #[test]
    fn search_updates_incrementally_backspaces_unicode_and_resets_stale_layout() {
        let mut first = simple_pr(1, "main", "a", vec![]);
        first.title = "ordinary".into();
        let mut second = simple_pr(2, "main", "b", vec![]);
        second.title = "café".into();
        let mut state = me_state(vec![first, second]);
        state.authored_sel = 1;
        *state.authored_list_state.offset_mut() = 7;

        press(&mut state, KeyCode::Char('/'));
        for character in "café".chars() {
            press(&mut state, KeyCode::Char(character));
        }
        assert_eq!(
            state.authored_search,
            AuthoredSearch::Editing("café".into())
        );
        assert_eq!(state.authored_section().count, 1);
        // Filtered selectable rows: repo header(0) + the matching PR(1). The
        // reset clamps the stale selection to that range (still valid at 1) and
        // resets the stale scroll offset.
        assert_eq!(state.authored_sel, 1);
        assert_eq!(state.authored_list_state.offset(), 0);

        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.authored_search, AuthoredSearch::Editing("caf".into()));
        assert_eq!(state.authored_section().count, 1);
    }

    #[test]
    fn enter_commits_empty_enter_clears_and_escape_cancels_or_clears() {
        let mut state = me_state(vec![simple_pr(1, "main", "a", vec![])]);
        press(&mut state, KeyCode::Char('/'));
        press(&mut state, KeyCode::Char('t'));
        press(&mut state, KeyCode::Enter);
        assert_eq!(state.authored_search, AuthoredSearch::Filtered("t".into()));

        assert!(press(&mut state, KeyCode::Esc));
        assert_eq!(state.authored_search, AuthoredSearch::Normal);

        press(&mut state, KeyCode::Char('/'));
        press(&mut state, KeyCode::Char('x'));
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.authored_search, AuthoredSearch::Normal);

        press(&mut state, KeyCode::Char('/'));
        press(&mut state, KeyCode::Enter);
        assert_eq!(state.authored_search, AuthoredSearch::Normal);
    }

    #[test]
    fn slash_replaces_committed_query_and_filter_survives_refresh() {
        let mut state = me_state(vec![simple_pr(1, "main", "a", vec![])]);
        state.authored_search = AuthoredSearch::Filtered("old".into());
        press(&mut state, KeyCode::Char('/'));
        assert_eq!(
            state.authored_search,
            AuthoredSearch::Editing(String::new())
        );
        press(&mut state, KeyCode::Char('n'));
        press(&mut state, KeyCode::Enter);

        let mut refreshed_pr = simple_pr(2, "main", "new", vec![]);
        refreshed_pr.title = "new result".into();
        state.apply(Data {
            viewer: "me".into(),
            authored: vec![refreshed_pr],
            reviewing: vec![],
            merged: vec![],
            releases: vec![],
            config_error: None,
            warnings: vec![],
        });
        assert_eq!(state.authored_search, AuthoredSearch::Filtered("n".into()));
        assert_eq!(state.authored_section().count, 1);
    }

    #[test]
    fn committed_filter_collapse_is_temporary_and_prior_state_is_restored() {
        let mut state = me_state(vec![simple_pr(
            1,
            "main",
            "a",
            vec![requested_user("needle")],
        )]);
        // Persistently expand Reviewers before searching.
        report::set_expanded(&mut state.toggled, "o/r", 1, SectionId::Reviewers, true);
        state.authored_search = AuthoredSearch::Filtered("needle".into());
        // Filtered selectable order: Repo(0), Pr(1), Reviewers header(2),
        // reviewer(3). Select the matching reviewer under the exposed header.
        state.authored_sel = 3;
        toggle_section(&mut state, false);
        assert!(
            state
                .search_collapsed
                .contains_key(&("o/r".into(), 1, SectionId::Reviewers))
        );
        assert!(report::is_expanded(
            &state.toggled,
            "o/r",
            1,
            SectionId::Reviewers,
            SectionId::Reviewers.default_expanded()
        ));

        press(&mut state, KeyCode::Esc);
        assert!(state.search_collapsed.is_empty());
        let section = state.authored_section();
        assert!(
            section
                .rows
                .iter()
                .any(|row| matches!(row, Row::Reviewer { .. }))
        );
    }

    fn comment(url: &str) -> PrComment {
        PrComment {
            author: "carol".to_string(),
            body: "add a test".to_string(),
            url: url.to_string(),
            path: None,
            is_outdated: false,
        }
    }

    /// A failing check named `name` with details URL `url`.
    fn failing_check(name: &str, url: Option<&str>) -> CheckStatus {
        let mut c = check(name, url, true);
        c.state = crate::model::CheckState::Failure;
        c
    }

    /// The selectable index of the first row for which `want` holds. Panics if
    /// none match — keeps selection-resolution tests robust to row-order shifts.
    fn sel_where(section: &Section<'_>, want: impl Fn(&Selected<'_>) -> bool) -> usize {
        let mut idx = 0usize;
        while let Some(row) = selected_row(&section.rows, idx) {
            if want(&row) {
                return idx;
            }
            idx += 1;
        }
        panic!("no selectable row matched the predicate");
    }

    #[test]
    fn pr_actions_for_skips_reviewers_and_passing_checks() {
        // A PR with only a requested reviewer and a passing check has nothing
        // actionable.
        let mut inert = simple_pr(9, "main", "z", vec![requested_user("bob")]);
        inert.checks = vec![check("lint", Some("https://ci/lint"), true)];
        assert!(pr_actions_for(&inert).is_none());
    }

    #[test]
    fn gather_actionable_collects_pr_and_descendant_stack() {
        // Root #1 (comment + one failing + one passing check); child #2 (one
        // failing check). Reviewers and the passing check are excluded.
        let mut root = simple_pr(1, "main", "a", vec![requested_user("alice")]);
        root.unresolved_comments = vec![comment("https://x/#c1")];
        root.checks = vec![
            failing_check("build", Some("https://ci/build")),
            check("lint", Some("https://ci/lint"), true),
        ];
        let mut child = simple_pr(2, "a", "b", vec![]);
        child.checks = vec![failing_check("test", None)];
        let prs = vec![&root, &child];
        let tree = authored_tree(&prs);
        assert_eq!(tree.len(), 1, "child #2 nests under root #1");

        let items = gather_actionable(&tree[0]);
        assert_eq!(items.len(), 2);
        // Pre-order: the root first, its comment plus only the failing check.
        assert_eq!(items[0].pr.number, 1);
        assert_eq!(items[0].comments.len(), 1);
        assert_eq!(items[0].checks.len(), 1);
        assert_eq!(items[0].checks[0].name, "build");
        // Then the descendant: its failing check, no comments.
        assert_eq!(items[1].pr.number, 2);
        assert!(items[1].comments.is_empty());
        assert_eq!(items[1].checks.len(), 1);
    }

    #[test]
    fn combined_prompt_single_pr_comment_then_check_single_branch_trailer() {
        let mut pr = simple_pr(12, "main", "feature", vec![]);
        pr.title = "Fix the thing".into();
        pr.unresolved_comments = vec![comment("https://x/#c1")];
        let build = failing_check("build", Some("https://ci/build"));
        let items = vec![PrActions {
            pr: &pr,
            comments: pr.unresolved_comments.iter().collect(),
            checks: vec![&build],
        }];
        assert_eq!(
            combined_prompt(&items).unwrap(),
            "Please address the following:\n\
             \n\
             In feature (#12 Fix the thing):\n\
             - https://x/#c1\n\
             - check build (https://ci/build)\n\
             \n\
             Use a worktree if this branch is not already active in the current worktree."
        );
    }

    #[test]
    fn combined_prompt_empty_head_ref_drops_the_in_branch_prefix() {
        let mut pr = simple_pr(5, "main", "", vec![]);
        pr.title = "No branch".into();
        let c = comment("https://x/#c9");
        let items = vec![PrActions {
            pr: &pr,
            comments: vec![&c],
            checks: vec![],
        }];
        assert_eq!(
            combined_prompt(&items).unwrap(),
            "Please address the following:\n\
             \n\
             #5 No branch:\n\
             - https://x/#c9\n\
             \n\
             Use a worktree if this branch is not already active in the current worktree."
        );
    }

    #[test]
    fn combined_prompt_multiple_branches_use_multi_branch_trailer_and_pr_url_fallback() {
        let mut pr1 = simple_pr(1, "main", "a", vec![]);
        pr1.title = "first".into();
        let mut pr2 = simple_pr(2, "a", "b", vec![]);
        pr2.title = "second".into();
        let build = failing_check("build", Some("https://ci/build"));
        let nourl = failing_check("test", None); // → falls back to pr2.url
        let c = comment("https://x/#c1");
        let items = vec![
            PrActions {
                pr: &pr1,
                comments: vec![&c],
                checks: vec![&build],
            },
            PrActions {
                pr: &pr2,
                comments: vec![],
                checks: vec![&nourl],
            },
        ];
        assert_eq!(
            combined_prompt(&items).unwrap(),
            format!(
                "Please address the following:\n\
                 \n\
                 In a (#1 first):\n\
                 - https://x/#c1\n\
                 - check build (https://ci/build)\n\
                 \n\
                 In b (#2 second):\n\
                 - check test ({})\n\
                 \n\
                 Use a worktree for each branch if it is not already active in the current worktree, and consider spawning a separate sub-agent per branch.",
                pr2.url
            )
        );
    }

    #[test]
    fn combined_prompt_is_none_when_no_actionable_items() {
        assert_eq!(combined_prompt(&[]), None);
        // A PR group with neither comments nor checks contributes nothing.
        let pr = simple_pr(1, "main", "a", vec![]);
        let empty = vec![PrActions {
            pr: &pr,
            comments: vec![],
            checks: vec![],
        }];
        assert_eq!(combined_prompt(&empty), None);
    }

    /// Repo `o/r`: root #1 (branch `a`) with a comment and a failing check;
    /// child #2 (branch `b`) with a failing check; a second root #3 (branch
    /// `c`) with a comment. Distinguishes PR-subtree, Stacked-children, and
    /// whole-repo gather scopes.
    fn stacked_state() -> AppState {
        let mut pr1 = simple_pr(1, "main", "a", vec![]);
        pr1.title = "first".into();
        pr1.unresolved_comments = vec![comment("https://x/#c1")];
        pr1.checks = vec![failing_check("build", Some("https://ci/build"))];
        let mut pr2 = simple_pr(2, "a", "b", vec![]);
        pr2.title = "second".into();
        pr2.checks = vec![failing_check("test", None)];
        let mut pr3 = simple_pr(3, "main", "c", vec![]);
        pr3.title = "third".into();
        pr3.unresolved_comments = vec![comment("https://x/#c3")];
        me_state(vec![pr1, pr2, pr3])
    }

    #[test]
    fn copy_prompt_on_pr_row_aggregates_the_whole_stack() {
        let mut state = stacked_state();
        let section = state.authored_section();
        let idx = sel_where(
            &section,
            |s| matches!(s, Selected::Pr(pr) if pr.number == 1),
        );
        drop(section);
        state.authored_sel = idx;
        // The whole #1 stack (#1 and its child #2), but NOT the sibling root #3.
        assert_eq!(
            build_copy_prompt(&state).unwrap(),
            "Please address the following:\n\
             \n\
             In a (#1 first):\n\
             - https://x/#c1\n\
             - check build (https://ci/build)\n\
             \n\
             In b (#2 second):\n\
             - check test (https://github.com/o/r/pull/2)\n\
             \n\
             Use a worktree for each branch if it is not already active in the current worktree, and consider spawning a separate sub-agent per branch."
        );
    }

    #[test]
    fn copy_prompt_on_stacked_header_covers_children_only() {
        let mut state = stacked_state();
        let section = state.authored_section();
        let idx = sel_where(
            &section,
            |s| matches!(s, Selected::Section(pr, SectionId::Stacked) if pr.number == 1),
        );
        drop(section);
        state.authored_sel = idx;
        // Only the child #2, not the parent #1 → single branch.
        assert_eq!(
            build_copy_prompt(&state).unwrap(),
            "Please address the following:\n\
             \n\
             In b (#2 second):\n\
             - check test (https://github.com/o/r/pull/2)\n\
             \n\
             Use a worktree if this branch is not already active in the current worktree."
        );
    }

    #[test]
    fn copy_prompt_on_repo_header_covers_every_pr_in_the_repo() {
        let mut state = stacked_state();
        let section = state.authored_section();
        let idx = sel_where(&section, |s| matches!(s, Selected::Repo(_)));
        drop(section);
        state.authored_sel = idx;
        // Both roots and the child: #1, #2, #3 → three branches.
        assert_eq!(
            build_copy_prompt(&state).unwrap(),
            "Please address the following:\n\
             \n\
             In a (#1 first):\n\
             - https://x/#c1\n\
             - check build (https://ci/build)\n\
             \n\
             In b (#2 second):\n\
             - check test (https://github.com/o/r/pull/2)\n\
             \n\
             In c (#3 third):\n\
             - https://x/#c3\n\
             \n\
             Use a worktree for each branch if it is not already active in the current worktree, and consider spawning a separate sub-agent per branch."
        );
    }

    #[test]
    fn copy_prompt_single_comment_and_single_check_share_the_unified_format() {
        let mut state = stacked_state();
        let section = state.authored_section();
        let comment_idx = sel_where(
            &section,
            |s| matches!(s, Selected::Comment(_, c) if c.url == "https://x/#c1"),
        );
        let check_idx = sel_where(
            &section,
            |s| matches!(s, Selected::Check(_, c) if c.name == "build"),
        );
        drop(section);

        state.authored_sel = comment_idx;
        assert_eq!(
            build_copy_prompt(&state).unwrap(),
            "Please address the following:\n\
             \n\
             In a (#1 first):\n\
             - https://x/#c1\n\
             \n\
             Use a worktree if this branch is not already active in the current worktree."
        );

        state.authored_sel = check_idx;
        assert_eq!(
            build_copy_prompt(&state).unwrap(),
            "Please address the following:\n\
             \n\
             In a (#1 first):\n\
             - check build (https://ci/build)\n\
             \n\
             Use a worktree if this branch is not already active in the current worktree."
        );
    }

    /// Repo `o/r`, PR #1 (branch `a`) with only a requested reviewer and a
    /// passing check — every actionable-gather scope is empty, but the single
    /// passing check is still addressable. Checks/Valid Results/Reviewers are
    /// forced open so their nodes are reachable.
    fn inert_state() -> AppState {
        let mut pr = simple_pr(1, "main", "a", vec![requested_user("alice")]);
        pr.checks = vec![check("lint", Some("https://ci/lint"), true)];
        let mut state = me_state(vec![pr]);
        report::set_expanded(&mut state.toggled, "o/r", 1, SectionId::Checks, true);
        report::set_expanded(&mut state.toggled, "o/r", 1, SectionId::ValidResults, true);
        report::set_expanded(&mut state.toggled, "o/r", 1, SectionId::Reviewers, true);
        state
    }

    #[test]
    fn build_copy_prompt_is_none_for_prompt_free_and_all_green_scopes() {
        let mut state = inert_state();
        let section = state.authored_section();
        let repo = sel_where(&section, |s| matches!(s, Selected::Repo(_)));
        let pr = sel_where(&section, |s| matches!(s, Selected::Pr(p) if p.number == 1));
        let checks = sel_where(&section, |s| {
            matches!(s, Selected::Section(_, SectionId::Checks))
        });
        let valid = sel_where(&section, |s| {
            matches!(s, Selected::Section(_, SectionId::ValidResults))
        });
        let reviewers = sel_where(&section, |s| {
            matches!(s, Selected::Section(_, SectionId::Reviewers))
        });
        let reviewer = sel_where(&section, |s| matches!(s, Selected::Reviewer(_, _)));
        let lint = sel_where(
            &section,
            |s| matches!(s, Selected::Check(_, c) if c.name == "lint"),
        );
        drop(section);

        // Repo / PR subtree (all-green) and prompt-notion-free nodes → nothing.
        for idx in [repo, pr, checks, valid, reviewers, reviewer] {
            state.authored_sel = idx;
            assert_eq!(
                build_copy_prompt(&state),
                None,
                "index {idx} should be empty"
            );
        }

        // A single check is still addressable even when it is passing.
        state.authored_sel = lint;
        assert_eq!(
            build_copy_prompt(&state).unwrap(),
            "Please address the following:\n\
             \n\
             In a (#1 t1):\n\
             - check lint (https://ci/lint)\n\
             \n\
             Use a worktree if this branch is not already active in the current worktree."
        );
    }

    #[test]
    fn copy_prompt_status_empty_case_and_no_op_outside_me() {
        let mut state = inert_state();
        let section = state.authored_section();
        let pr_idx = sel_where(&section, |s| matches!(s, Selected::Pr(p) if p.number == 1));
        drop(section);

        // Empty gather → explicit footer status (no clipboard write happens).
        state.authored_sel = pr_idx;
        copy_prompt(&mut state);
        assert_eq!(state.status.as_deref(), Some("c: nothing to address here"));

        // Outside the Authored pane, `c` is a silent no-op.
        state.status = None;
        state.mode = ViewMode::People;
        copy_prompt(&mut state);
        assert_eq!(state.status, None);
    }

    #[test]
    fn repo_header_is_selectable_and_toggles_the_whole_repo() {
        let mut state = me_state(vec![simple_pr(1, "main", "a", vec![])]);
        // Both walkers agree: the repo header is the first selectable row.
        let section = state.authored_section();
        assert!(matches!(
            selected_row(&section.rows, 0),
            Some(Selected::Repo(r)) if r == "o/r"
        ));
        let ctx = section_ctx_at(&section.rows, 0).expect("repo header resolves");
        assert_eq!(ctx.repo, "o/r");
        assert_eq!(ctx.number, 0);
        assert_eq!(ctx.section, SectionId::Repo);
        assert!(ctx.is_header);
        drop(section);

        // `h` collapses the repo grouping, hiding its PR rows; cursor stays put.
        state.authored_sel = 0;
        toggle_section(&mut state, false);
        assert!(!report::is_expanded(
            &state.toggled,
            "o/r",
            0,
            SectionId::Repo,
            true
        ));
        assert_eq!(state.authored_sel, 0);
        let section = state.authored_section();
        assert!(!section.rows.iter().any(|r| matches!(r, Row::Pr { .. })));
        assert!(
            section
                .rows
                .iter()
                .any(|r| matches!(r, Row::RepoHeader { .. }))
        );
        drop(section);

        // `l` re-expands and the PR reappears.
        toggle_section(&mut state, true);
        assert!(report::is_expanded(
            &state.toggled,
            "o/r",
            0,
            SectionId::Repo,
            true
        ));
        let section = state.authored_section();
        assert!(
            section
                .rows
                .iter()
                .any(|r| matches!(r, Row::Pr { pr, .. } if pr.number == 1))
        );
    }
}
