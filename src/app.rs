use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use anyhow::Result;
use chrono::{DateTime, Local};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{DefaultTerminal, widgets::ListState};

use crate::{
    github::{self, Data},
    model::{Pr, ReviewerKind, ReviewerStatus},
    report::{self, Row, Section},
    ui,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Authored,
    Reviewing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Me,
    People,
}

pub enum Msg {
    Fetched(Result<Data>),
    Action { label: String, result: Result<()> },
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
    /// Persisted across frames so scroll offset is sticky — `ui` only moves
    /// the viewport when the selection crosses a scroll-margin boundary.
    pub authored_list_state: ListState,
    pub reviewing_list_state: ListState,
    pub people_list_state: ListState,
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
            authored_list_state: ListState::default(),
            reviewing_list_state: ListState::default(),
            people_list_state: ListState::default(),
        }
    }

    fn apply(&mut self, data: Data) {
        self.viewer = Some(data.viewer);
        self.authored = data.authored;
        self.reviewing = data.reviewing;
        self.merged = data.merged;
        self.loaded_at = Some(Local::now());
        self.error = None;
        self.status = None;
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
        report::build_section_authored(&self.authored, self.viewer_str())
    }

    pub fn people_section(&self) -> Section<'_> {
        report::build_section_people(&self.authored, &self.reviewing, self.viewer_str())
    }

    fn current_section(&self) -> Section<'_> {
        match self.mode {
            ViewMode::Me => match self.focus {
                Focus::Authored => self.authored_section(),
                Focus::Reviewing => self.reviewing_section(),
            },
            ViewMode::People => self.people_section(),
        }
    }

    fn current_len(&self) -> usize {
        count_selectable(&self.current_section())
    }

    fn current_sel(&self) -> usize {
        match self.mode {
            ViewMode::Me => match self.focus {
                Focus::Authored => self.authored_sel,
                Focus::Reviewing => self.reviewing_sel,
            },
            ViewMode::People => self.people_sel,
        }
    }

    fn current_sel_mut(&mut self) -> &mut usize {
        match self.mode {
            ViewMode::Me => match self.focus {
                Focus::Authored => &mut self.authored_sel,
                Focus::Reviewing => &mut self.reviewing_sel,
            },
            ViewMode::People => &mut self.people_sel,
        }
    }

    /// Walk the current section's selectable rows, tracking each PR's parent
    /// (so reviewer rows can locate their PR), and invoke `f` on the selected
    /// row. `f` receives the parent PR plus `Some(reviewer)` for reviewer
    /// rows and `None` for PR rows. Returns whatever `f` returned, or `None`
    /// if the selection is out of range.
    fn with_selected<R>(
        &self,
        mut f: impl FnMut(&Pr, Option<&ReviewerStatus>) -> Option<R>,
    ) -> Option<R> {
        let section = self.current_section();
        let sel = self.current_sel();
        let mut idx = 0usize;
        let mut current_pr: Option<&Pr> = None;
        for row in &section.rows {
            match row {
                Row::Pr { pr, .. } => {
                    current_pr = Some(pr);
                    if idx == sel {
                        return f(pr, None);
                    }
                    idx += 1;
                }
                Row::Reviewer(r) => {
                    if idx == sel {
                        return current_pr.and_then(|pr| f(pr, Some(r)));
                    }
                    idx += 1;
                }
                _ => {}
            }
        }
        None
    }
}

fn count_selectable(section: &Section<'_>) -> usize {
    section.rows.iter().filter(|r| r.is_selectable()).count()
}

pub fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal);
    ratatui::restore();
    result
}

fn run_app(terminal: &mut DefaultTerminal) -> Result<()> {
    let mut state = AppState::new();
    let (tx, rx) = mpsc::channel::<Msg>();
    spawn_fetch(&tx);

    loop {
        drain_msgs(&rx, &mut state, &tx);

        terminal.draw(|f| ui::draw(f, &mut state))?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Esc => {
                    if state.mode == ViewMode::People {
                        state.mode = ViewMode::Me;
                        state.people_sel = 0;
                    }
                }
                KeyCode::Char('p') => {
                    if state.mode == ViewMode::Me {
                        state.mode = ViewMode::People;
                        state.people_sel = 0;
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => move_selection(&mut state, 1),
                KeyCode::Char('k') | KeyCode::Up => move_selection(&mut state, -1),
                KeyCode::Char('g') => jump(&mut state, true),
                KeyCode::Char('G') => jump(&mut state, false),
                KeyCode::Tab | KeyCode::BackTab => {
                    if state.mode == ViewMode::Me {
                        toggle_focus(&mut state);
                    }
                }
                KeyCode::Enter => open_selected(&state),
                KeyCode::Char('r') => {
                    if !state.loading {
                        state.loading = true;
                        spawn_fetch(&tx);
                    }
                }
                KeyCode::Char('x') => remove_selected_reviewer(&mut state, &tx),
                _ => {}
            }
        }
    }
    Ok(())
}

fn drain_msgs(rx: &Receiver<Msg>, state: &mut AppState, tx: &Sender<Msg>) {
    while let Ok(msg) = rx.try_recv() {
        match msg {
            Msg::Fetched(Ok(data)) => state.apply(data),
            Msg::Fetched(Err(e)) => state.fail(format!("{e:#}")),
            Msg::Action { label, result } => match result {
                Ok(()) => {
                    state.status = Some(format!("{label}: ok"));
                    if !state.loading {
                        state.loading = true;
                        spawn_fetch(tx);
                    }
                }
                Err(e) => {
                    state.status = Some(format!("{label}: {e:#}"));
                }
            },
        }
    }
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

fn toggle_focus(state: &mut AppState) {
    state.focus = match state.focus {
        Focus::Authored => Focus::Reviewing,
        Focus::Reviewing => Focus::Authored,
    };
}

fn open_selected(state: &AppState) {
    let url = state.with_selected(|pr, _| Some(pr.url.clone()));
    if let Some(url) = url {
        let _ = open::that(url);
    }
}

fn remove_selected_reviewer(state: &mut AppState, tx: &Sender<Msg>) {
    let extracted = state.with_selected(|pr, rv| {
        let rv = rv?;
        Some((
            pr.repo.clone(),
            pr.number,
            rv.login.clone(),
            rv.kind,
            rv.requested,
        ))
    });
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
