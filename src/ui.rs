use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::{
    app::{AppState, Focus, ViewMode},
    model::{Pr, ReleaseInfo, ReviewState, ReviewerKind, ReviewerStatus, TagInfo, human_age},
    report::{self, Row, Section},
};

pub fn draw(f: &mut Frame, state: &mut AppState) {
    let outer = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(f.area());
    let top_and_merged =
        Layout::vertical([Constraint::Percentage(75), Constraint::Percentage(25)]).split(outer[0]);
    let top = top_and_merged[0];
    let merged_area = top_and_merged[1];

    let viewer_str: String = state.viewer.clone().unwrap_or_default();

    match state.mode {
        ViewMode::Me => {
            let sections =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(top);
            let focus = state.focus;

            let reviewing_col = sections[0];
            let parts = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(reviewing_col);
            let reviewing_area = parts[0];
            let releases_area = parts[1];

            let reviewing_section = report::build_section_reviewing(&state.reviewing);
            draw_section(
                f,
                reviewing_area,
                &reviewing_section,
                state.reviewing_sel,
                &mut state.reviewing_list_state,
                focus == Focus::Reviewing,
            );
            let authored_section = report::build_section_authored(&state.authored, &viewer_str);
            draw_section(
                f,
                sections[1],
                &authored_section,
                state.authored_sel,
                &mut state.authored_list_state,
                focus == Focus::Authored,
            );
            let mut releases_section =
                report::build_section_releases(&state.releases, chrono::Utc::now());
            if state.releases.is_empty() && state.loaded_at.is_none() {
                releases_section.empty_message = Some("Loading…");
            }
            draw_section(
                f,
                releases_area,
                &releases_section,
                state.releases_sel,
                &mut state.releases_list_state,
                focus == Focus::Releases,
            );
        }
        ViewMode::People => {
            let people_section =
                report::build_section_people(&state.authored, &state.reviewing, &viewer_str);
            draw_section(
                f,
                top,
                &people_section,
                state.people_sel,
                &mut state.people_list_state,
                true,
            );
        }
    }

    let allowed: std::collections::BTreeSet<String> = match state.mode {
        ViewMode::Me => report::allowed_authors_me(&viewer_str, &state.reviewing),
        ViewMode::People => {
            report::allowed_authors_people(&state.authored, &state.reviewing, &viewer_str)
        }
    };
    let mut merged_section =
        report::build_section_merged(&state.merged, &allowed, report::MERGED_PANE_CAP);
    if state.merged.is_empty() && state.loaded_at.is_none() {
        merged_section.empty_message = Some("Loading…");
    }
    draw_merged_pane(f, merged_area, &merged_section);

    draw_footer(f, outer[1], state);
}

const SCROLL_MARGIN: usize = 4;

fn draw_section(
    f: &mut Frame,
    area: Rect,
    section: &Section<'_>,
    selection: usize,
    list_state: &mut ListState,
    focused: bool,
) {
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title_line = match &section.subtitle {
        Some(sub) => format!(" {} ({}, {}) ", section.title, section.count, sub),
        None => format!(" {} ({}) ", section.title, section.count),
    };
    let block = Block::default()
        .title(title_line)
        .borders(Borders::ALL)
        .border_style(border_style);

    if section.rows.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let msg = Paragraph::new(Span::styled(
            section.empty_message.unwrap_or(""),
            Style::default().add_modifier(Modifier::DIM),
        ));
        f.render_widget(msg, inner);
        return;
    }

    let mut items: Vec<ListItem> = Vec::new();
    let mut row_of_sel: Vec<usize> = Vec::new();
    for row in &section.rows {
        if row.is_selectable() {
            row_of_sel.push(items.len());
        }
        items.push(render_list_item(row));
    }

    let highlighted_row = row_of_sel.get(selection).copied();
    list_state.select(highlighted_row);

    let inner_h = block.inner(area).height as usize;
    apply_scroll_margin(list_state, highlighted_row, items.len(), inner_h);

    let highlight_style = if focused {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style)
        .highlight_symbol(if focused { "▶ " } else { "  " });

    f.render_stateful_widget(list, area, list_state);
}

fn render_list_item(row: &Row<'_>) -> ListItem<'static> {
    match row {
        Row::RepoHeader(repo) => ListItem::new(Line::from(Span::styled(
            repo.clone(),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ))),
        Row::PersonHeader(login) => ListItem::new(Line::from(Span::styled(
            format!("@{login}"),
            Style::default()
                .fg(color_for_login(login))
                .add_modifier(Modifier::BOLD),
        ))),
        Row::SubGroupLabel(label) => ListItem::new(Line::from(Span::styled(
            format!("  {label}"),
            Style::default().add_modifier(Modifier::DIM),
        ))),
        Row::Pr { pr, hide_author_if } => ListItem::new(pr_line(pr, hide_author_if.as_deref())),
        Row::Reviewer(r) => ListItem::new(reviewer_line(r)),
        Row::MergedPr(pr) => ListItem::new(merged_pr_line(pr)),
        Row::ReleaseEntry { release, now } => ListItem::new(release_entry_line(release, *now)),
        Row::ReleaseTag { tag, now, .. } => ListItem::new(release_tag_line(tag, *now)),
        Row::ReleaseEmpty => ListItem::new(Line::from(Span::styled(
            "  (no releases or tags)",
            Style::default().add_modifier(Modifier::DIM),
        ))),
    }
}

fn release_entry_line(release: &ReleaseInfo, now: chrono::DateTime<chrono::Utc>) -> Line<'static> {
    let label = release
        .name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| release.tag_name.clone());
    let mut spans: Vec<Span<'static>> = vec![Span::raw(format!(
        "  {} ({})",
        label,
        human_age(release.created_at, now)
    ))];
    if release.is_prerelease {
        spans.push(Span::styled(
            " [pre]",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

fn release_tag_line(tag: &TagInfo, now: chrono::DateTime<chrono::Utc>) -> Line<'static> {
    Line::from(Span::styled(
        format!("  tag: {} ({})", tag.name, human_age(tag.committed_at, now)),
        Style::default().add_modifier(Modifier::DIM),
    ))
}

/// Vim-style `scrolloff`: only move the viewport once the selection is within
/// `SCROLL_MARGIN` rows of the top or bottom edge, moving toward that edge.
/// Otherwise the offset is preserved — pressing `k` after jumping to the
/// bottom leaves the viewport alone until the cursor approaches the top.
fn apply_scroll_margin(
    list_state: &mut ListState,
    highlighted: Option<usize>,
    item_count: usize,
    inner_h: usize,
) {
    if item_count == 0 || inner_h == 0 {
        *list_state.offset_mut() = 0;
        return;
    }
    let Some(sel) = highlighted else { return };

    // If everything fits on screen, don't scroll.
    if item_count <= inner_h {
        *list_state.offset_mut() = 0;
        return;
    }

    // Cap margin at half the viewport so small panes don't fight themselves.
    let margin = SCROLL_MARGIN.min(inner_h.saturating_sub(1) / 2);
    let max_offset = item_count - inner_h;
    let mut offset = list_state.offset().min(max_offset);

    if sel < offset + margin {
        offset = sel.saturating_sub(margin);
    } else if sel + margin + 1 > offset + inner_h {
        offset = sel + margin + 1 - inner_h;
    }

    *list_state.offset_mut() = offset.min(max_offset);
}

fn draw_merged_pane(f: &mut Frame, area: Rect, section: &Section<'_>) {
    let border_style = Style::default().fg(Color::DarkGray);
    let title_line = format!(" {} ({}) ", section.title, section.count);
    let block = Block::default()
        .title(title_line)
        .borders(Borders::ALL)
        .border_style(border_style);

    if section.rows.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let msg = Paragraph::new(Span::styled(
            section.empty_message.unwrap_or(""),
            Style::default().add_modifier(Modifier::DIM),
        ));
        f.render_widget(msg, inner);
        return;
    }

    let items: Vec<ListItem> = section.rows.iter().map(render_list_item).collect();
    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn merged_pr_line(pr: &Pr) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        format!("  #{} ", pr.number),
        Style::default().fg(Color::Blue),
    ));
    spans.push(Span::styled(
        format!("{} ", pr.repo),
        Style::default().fg(Color::Magenta),
    ));
    spans.push(Span::styled(
        format!("@{} ", pr.author),
        Style::default().fg(color_for_login(&pr.author)),
    ));
    spans.push(Span::raw(pr.title.clone()));
    Line::from(spans)
}

fn pr_line(pr: &Pr, hide_author_if: Option<&str>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        format!("  #{} ", pr.number),
        Style::default().fg(Color::Blue),
    ));
    if pr.is_draft {
        spans.push(Span::styled(
            "[draft] ",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    let show_author = hide_author_if.is_none_or(|v| v != pr.author);
    if show_author {
        spans.push(Span::styled(
            format!("@{} ", pr.author),
            Style::default().fg(color_for_login(&pr.author)),
        ));
    }
    spans.push(Span::raw(pr.title.clone()));
    Line::from(spans)
}

fn reviewer_line(r: &ReviewerStatus) -> Line<'static> {
    let (glyph, glyph_style) = match r.state {
        ReviewState::Approved => ("✓", Style::default().fg(Color::Green)),
        ReviewState::ChangesRequested => ("✗", Style::default().fg(Color::Red)),
        ReviewState::Commented => ("◉", Style::default().fg(Color::Yellow)),
        ReviewState::NoReview => ("○", Style::default().add_modifier(Modifier::DIM)),
        ReviewState::Dismissed => (
            "⊘",
            Style::default()
                .add_modifier(Modifier::DIM)
                .add_modifier(Modifier::CROSSED_OUT),
        ),
    };
    let name_style = Style::default().fg(color_for_login(&r.login));
    let prefix = match r.kind {
        ReviewerKind::User => "      ",
        ReviewerKind::Team => "      ",
    };
    let mut spans = vec![
        Span::raw(prefix),
        Span::styled(glyph.to_string(), glyph_style),
        Span::raw(" "),
        Span::styled(r.login.clone(), name_style),
    ];
    // `[req]` — GitHub is actively waiting on this person (they're in
    // `reviewRequests`). Only these rows can be removed with `x`. Reviewers
    // without the badge are in the list because they already reviewed.
    if r.requested {
        spans.push(Span::styled(
            "  [req]",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(
            "  (reviewed)",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

fn color_for_login(login: &str) -> Color {
    let (r, g, b) = report::rgb_for_login(login);
    Color::Rgb(r, g, b)
}

/// Braille-dots spinner. Wall-clock driven so it animates without any per-
/// frame state — the event loop already redraws every ~100 ms.
fn spinner_frame() -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    FRAMES[(ms / 100) as usize % FRAMES.len()]
}

fn draw_footer(f: &mut Frame, area: Rect, state: &AppState) {
    let status = if let Some(err) = &state.error {
        Span::styled(
            format!("error: {err}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if let Some(msg) = &state.status {
        Span::styled(
            msg.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else if state.loading {
        Span::styled(
            format!("{} Loading…", spinner_frame()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if let Some(at) = state.loaded_at {
        Span::styled(
            format!(
                "loaded {} authored, {} reviewing · {}",
                state.authored.len(),
                state.reviewing.len(),
                at.format("%H:%M:%S"),
            ),
            Style::default().add_modifier(Modifier::DIM),
        )
    } else {
        Span::raw("")
    };

    let hint = match state.mode {
        ViewMode::Me => {
            "↑↓ move · Tab switch (incl. releases) · Enter open · x remove reviewer · p people · r refresh · q quit   "
        }
        ViewMode::People => {
            "↑↓ move · Esc back · Enter open · x remove reviewer · r refresh · q quit   "
        }
    };
    let line = Line::from(vec![
        Span::styled(hint, Style::default().add_modifier(Modifier::DIM)),
        Span::raw("["),
        status,
        Span::raw("]"),
    ]);
    f.render_widget(Paragraph::new(line), area);
}
