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
    model::{Pr, ReviewerKind, ReviewerStatus, group_by_person, group_by_repo},
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

pub enum Row<'a> {
    Pr(&'a Pr),
    Reviewer {
        pr: &'a Pr,
        reviewer: &'a ReviewerStatus,
    },
}

pub enum Msg {
    Fetched(Result<Data>),
    Action { label: String, result: Result<()> },
}

pub struct AppState {
    pub viewer: Option<String>,
    pub authored: Vec<Pr>,
    pub reviewing: Vec<Pr>,
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
        let a_len = selectable_rows(&self.authored).len();
        let r_len = selectable_rows(&self.reviewing).len();
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

    fn focused_prs(&self) -> &[Pr] {
        match self.focus {
            Focus::Authored => &self.authored,
            Focus::Reviewing => &self.reviewing,
        }
    }

    fn focused_sel_mut(&mut self) -> &mut usize {
        match self.focus {
            Focus::Authored => &mut self.authored_sel,
            Focus::Reviewing => &mut self.reviewing_sel,
        }
    }

    fn focused_sel(&self) -> usize {
        match self.focus {
            Focus::Authored => self.authored_sel,
            Focus::Reviewing => self.reviewing_sel,
        }
    }

    pub fn selected_row(&self) -> Option<Row<'_>> {
        match self.mode {
            ViewMode::Me => selectable_rows(self.focused_prs())
                .into_iter()
                .nth(self.focused_sel()),
            ViewMode::People => {
                let viewer = self.viewer.as_deref().unwrap_or("");
                selectable_rows_people(&self.authored, &self.reviewing, viewer)
                    .into_iter()
                    .nth(self.people_sel)
            }
        }
    }

    fn current_len(&self) -> usize {
        match self.mode {
            ViewMode::Me => selectable_rows(self.focused_prs()).len(),
            ViewMode::People => {
                let viewer = self.viewer.as_deref().unwrap_or("");
                selectable_rows_people(&self.authored, &self.reviewing, viewer).len()
            }
        }
    }

    fn current_sel(&self) -> usize {
        match self.mode {
            ViewMode::Me => self.focused_sel(),
            ViewMode::People => self.people_sel,
        }
    }

    fn current_sel_mut(&mut self) -> &mut usize {
        match self.mode {
            ViewMode::Me => self.focused_sel_mut(),
            ViewMode::People => &mut self.people_sel,
        }
    }
}

/// Flat list of selectable rows in display order: each PR followed by its
/// reviewer rows, grouped by repo. Repo headers are not selectable.
pub fn selectable_rows(prs: &[Pr]) -> Vec<Row<'_>> {
    let mut rows = Vec::new();
    for (_, group_prs) in group_by_repo(prs) {
        for pr in group_prs {
            rows.push(Row::Pr(pr));
            for r in &pr.reviewers {
                rows.push(Row::Reviewer { pr, reviewer: r });
            }
        }
    }
    rows
}

/// People-mode analogue: each person's Authored/Reviewing PRs flattened with
/// their reviewer sub-rows. Person headers and sub-group labels are not
/// selectable — they're visual only, emitted by the UI layer.
pub fn selectable_rows_people<'a>(
    authored: &'a [Pr],
    reviewing: &'a [Pr],
    viewer: &str,
) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    for person in group_by_person(authored, reviewing, viewer) {
        for pr in person.authored.iter().chain(person.reviewing.iter()) {
            rows.push(Row::Pr(pr));
            for r in &pr.reviewers {
                rows.push(Row::Reviewer { pr, reviewer: r });
            }
        }
    }
    rows
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
    let url = match state.selected_row() {
        Some(Row::Pr(pr)) => Some(pr.url.clone()),
        Some(Row::Reviewer { pr, .. }) => Some(pr.url.clone()),
        None => None,
    };
    if let Some(url) = url {
        let _ = open::that(url);
    }
}

fn remove_selected_reviewer(state: &mut AppState, tx: &Sender<Msg>) {
    let Some(Row::Reviewer { pr, reviewer }) = state.selected_row() else {
        state.status = Some("x: select a reviewer row first".into());
        return;
    };
    if !reviewer.requested {
        // GitHub's DELETE requested_reviewers endpoint silently no-ops for
        // reviewers that aren't currently in reviewRequests. Don't pretend we
        // did anything — explain.
        state.status = Some(format!(
            "x: {} already reviewed; nothing to un-request (dismiss the review on GitHub to clear it)",
            reviewer.login
        ));
        return;
    }
    let Some((owner, repo)) = pr.repo.split_once('/') else {
        state.status = Some(format!("x: bad repo '{}'", pr.repo));
        return;
    };
    let owner = owner.to_string();
    let repo = repo.to_string();
    let number = pr.number;
    let login = reviewer.login.clone();
    let kind = reviewer.kind;
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
