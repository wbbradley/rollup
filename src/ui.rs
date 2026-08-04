use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph},
};

use crate::{
    app::{AppState, AuthoredSearch, Focus, ViewMode},
    model::{
        CheckState, CheckStatus, ChecksRollup, Pr, PrComment, PrCommentKind, ReleaseInfo,
        ReviewState, ReviewerStatus, TagInfo, human_age,
    },
    report::{self, ChecksSummary, ReviewerSummaryToken, Row, Section, SectionId},
};

pub fn draw(f: &mut Frame, state: &mut AppState, web_address: &str) {
    let outer = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(f.area());
    let top_and_merged =
        Layout::vertical([Constraint::Percentage(75), Constraint::Percentage(25)]).split(outer[0]);
    let top = top_and_merged[0];
    let merged_area = top_and_merged[1];

    let viewer_str: String = state.viewer.clone().unwrap_or_default();

    match state.mode {
        ViewMode::Me => {
            let authored_section = match state
                .authored_search
                .query()
                .filter(|query| !query.is_empty())
            {
                Some(query) => report::build_section_authored_filtered(
                    &state.authored,
                    &viewer_str,
                    query,
                    &state.search_collapsed,
                ),
                None => {
                    report::build_section_authored(&state.authored, &viewer_str, &state.toggled)
                }
            };
            state.authored_page = draw_section(
                f,
                top,
                &authored_section,
                state.authored_sel,
                &mut state.authored_list_state,
                true,
            );
        }
        ViewMode::Radar => {
            let focus = state.focus;
            let parts = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(top);
            let reviewing_area = parts[0];
            let releases_area = parts[1];

            let reviewing_section = report::build_section_reviewing(&state.reviewing);
            state.reviewing_page = draw_section(
                f,
                reviewing_area,
                &reviewing_section,
                state.reviewing_sel,
                &mut state.reviewing_list_state,
                focus == Focus::Reviewing,
            );
            let mut releases_section =
                report::build_section_releases(&state.releases, chrono::Utc::now());
            if state.releases.is_empty() && state.loaded_at.is_none() {
                releases_section.empty_message = Some("Loading…");
            }
            state.releases_page = draw_section(
                f,
                releases_area,
                &releases_section,
                state.releases_sel,
                &mut state.releases_list_state,
                focus == Focus::Releases,
            );
        }
    }

    // Me and Radar both scope the merged pane to the viewer + reviewing authors.
    let allowed: std::collections::BTreeSet<String> =
        report::allowed_authors_me(&viewer_str, &state.reviewing);
    let mut merged_section = report::build_section_merged(
        &state.merged,
        &allowed,
        report::MERGED_PANE_CAP,
        chrono::Utc::now(),
    );
    if state.merged.is_empty() && state.loaded_at.is_none() {
        merged_section.empty_message = Some("Loading…");
    }
    draw_merged_pane(f, merged_area, &merged_section);

    draw_footer(f, outer[1], state, web_address);
}

const SCROLL_MARGIN: usize = 4;

/// Draws an interactive section into `area` and returns the pane's visible
/// inner height (rows, excluding the border). Callers persist that height so
/// half-page keys (`PageUp`/`PageDown`, `Ctrl-U`/`Ctrl-D`) can size their jump.
fn draw_section(
    f: &mut Frame,
    area: Rect,
    section: &Section<'_>,
    selection: usize,
    list_state: &mut ListState,
    focused: bool,
) -> usize {
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

    let inner = block.inner(area);
    let inner_h = inner.height as usize;

    if section.rows.is_empty() {
        f.render_widget(block, area);
        let msg = Paragraph::new(Span::styled(
            section.empty_message.unwrap_or(""),
            Style::default().add_modifier(Modifier::DIM),
        ));
        f.render_widget(msg, inner);
        return inner_h;
    }

    let mut items: Vec<ListItem> = Vec::new();
    let mut row_of_sel: Vec<usize> = Vec::new();
    for row in &section.rows {
        if row.is_selectable() {
            row_of_sel.push(items.len());
        }
        // Reserve a stable two-column selection gutter for every row. Comment
        // truncation can then use the rest of the pane exactly, whether or not
        // the row is currently selected.
        items.push(render_list_item(
            row,
            inner.width.saturating_sub(2) as usize,
        ));
    }

    let highlighted_row = row_of_sel.get(selection).copied();
    list_state.select(highlighted_row);

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
        .highlight_symbol(if focused { "▶ " } else { "  " })
        .highlight_spacing(HighlightSpacing::Always);

    f.render_stateful_widget(list, area, list_state);
    inner_h
}

fn render_list_item(row: &Row<'_>, width: usize) -> ListItem<'static> {
    match row {
        Row::RepoHeader { repo, expanded } => {
            let mut spans: Vec<Span<'static>> = Vec::new();
            // A disclosure glyph appears only where the header is collapsible
            // (the Authored pane sets `expanded: Some(_)`).
            if let Some(exp) = expanded {
                spans.push(Span::styled(
                    if *exp { "▾ " } else { "▸ " }.to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            spans.push(Span::styled(
                repo.clone(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
            ListItem::new(Line::from(spans))
        }
        Row::Pr {
            pr,
            hide_author_if,
            show_head_ref,
            tree_prefix,
            expanded,
            ..
        } => ListItem::new(pr_line(
            pr,
            hide_author_if.as_deref(),
            *show_head_ref,
            tree_prefix.as_deref(),
            *expanded,
        )),
        Row::Reviewer { r, tree_prefix } => ListItem::new(reviewer_line(r, tree_prefix.as_deref())),
        Row::SectionHeader {
            section,
            expanded,
            summary,
            checks,
            tree_prefix,
        } => ListItem::new(section_header_line(
            *section,
            *expanded,
            summary,
            checks.as_ref(),
            tree_prefix,
        )),
        Row::Comment { c, tree_prefix } => ListItem::new(comment_line(c, tree_prefix, width)),
        Row::Check { c, tree_prefix } => ListItem::new(check_line(c, tree_prefix)),
        Row::MergedPr { pr, now } => ListItem::new(merged_pr_line(pr, *now)),
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
    let inner = block.inner(area);

    if section.rows.is_empty() {
        f.render_widget(block, area);
        let msg = Paragraph::new(Span::styled(
            section.empty_message.unwrap_or(""),
            Style::default().add_modifier(Modifier::DIM),
        ));
        f.render_widget(msg, inner);
        return;
    }

    let items: Vec<ListItem> = section
        .rows
        .iter()
        .map(|row| render_list_item(row, inner.width as usize))
        .collect();
    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn merged_pr_line(pr: &Pr, now: chrono::DateTime<chrono::Utc>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        format!("  #{} ", pr.number),
        Style::default().fg(Color::Blue),
    ));
    if let Some(merged_at) = pr.merged_at {
        spans.push(Span::raw(format!("({}) ", human_age(merged_at, now))));
    }
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

fn pr_line(
    pr: &Pr,
    hide_author_if: Option<&str>,
    show_head_ref: bool,
    tree_prefix: Option<&str>,
    expanded: Option<bool>,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    match tree_prefix {
        Some(tp) => spans.push(Span::styled(
            tp.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        )),
        None => spans.push(Span::raw("  ")),
    }
    if let Some(expanded) = expanded {
        spans.push(Span::styled(
            if expanded { "▾ " } else { "▸ " },
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    spans.push(Span::styled(
        format!("#{} ", pr.number),
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
    if show_head_ref && !pr.head_ref.is_empty() {
        spans.push(Span::styled(
            format!(" [{}]", pr.head_ref),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

fn reviewer_line(r: &ReviewerStatus, tree_prefix: Option<&str>) -> Line<'static> {
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
    let prefix_span = match tree_prefix {
        Some(tp) => Span::styled(tp.to_string(), Style::default().add_modifier(Modifier::DIM)),
        None => Span::raw("      "),
    };
    let mut spans = vec![
        prefix_span,
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

fn section_header_line(
    section: SectionId,
    expanded: bool,
    summary: &[ReviewerSummaryToken],
    checks: Option<&ChecksSummary>,
    tree_prefix: &str,
) -> Line<'static> {
    let glyph = if expanded { "▾" } else { "▸" };
    let label_style = if section == SectionId::Comments {
        Style::default()
            .fg(Color::Rgb(255, 140, 0))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(
            tree_prefix.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(
            format!("{glyph} "),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(section.label().to_string(), label_style),
    ];
    if let Some(cs) = checks {
        // Merge-readiness signal: a colored state glyph + the required ratio.
        let (glyph, style) = checks_glyph_style(cs.rollup);
        spans.push(Span::raw("  "));
        spans.push(Span::styled(glyph.to_string(), style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            cs.ratio_text(),
            Style::default().add_modifier(Modifier::DIM),
        ));
    } else if !summary.is_empty() {
        let bracket = Style::default().add_modifier(Modifier::DIM);
        spans.push(Span::styled(" [", bracket));
        for (i, token) in summary.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(", ", bracket));
            }
            spans.push(Span::styled(
                token.tui_label().to_string(),
                summary_token_style(*token),
            ));
        }
        spans.push(Span::styled("]", bracket));
    }
    Line::from(spans)
}

/// Glyph + style for a checks rollup, reusing the reviewer palette: `✓` green,
/// `✗` red (bold), `◉` yellow, `○` dim.
fn checks_glyph_style(rollup: ChecksRollup) -> (&'static str, Style) {
    match rollup {
        ChecksRollup::Green => ("✓", Style::default().fg(Color::Green)),
        ChecksRollup::Red => (
            "✗",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        ChecksRollup::Pending => ("◉", Style::default().fg(Color::Yellow)),
        ChecksRollup::Unknown => ("○", Style::default().add_modifier(Modifier::DIM)),
    }
}

fn check_line(c: &CheckStatus, tree_prefix: &str) -> Line<'static> {
    let (glyph, glyph_style) = match c.state {
        CheckState::Success => ("✓", Style::default().fg(Color::Green)),
        CheckState::Failure | CheckState::Error => ("✗", Style::default().fg(Color::Red)),
        CheckState::Pending => ("◉", Style::default().fg(Color::Yellow)),
        CheckState::Skipped => (
            "⊘",
            Style::default()
                .add_modifier(Modifier::DIM)
                .add_modifier(Modifier::CROSSED_OUT),
        ),
        CheckState::Neutral => ("○", Style::default().add_modifier(Modifier::DIM)),
    };
    // Non-required checks are dimmed and tagged, since they never move the
    // merge-readiness signal.
    let name_style = if c.required {
        Style::default()
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let mut spans = vec![
        Span::styled(
            tree_prefix.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(glyph.to_string(), glyph_style),
        Span::raw(" "),
        Span::styled(c.name.clone(), name_style),
    ];
    if !c.required {
        spans.push(Span::styled(
            " (not required)",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

fn summary_token_style(token: ReviewerSummaryToken) -> Style {
    match token {
        ReviewerSummaryToken::Requested => Style::default().fg(Color::Cyan),
        ReviewerSummaryToken::Approved => Style::default().fg(Color::Green),
        ReviewerSummaryToken::ChangesRequested => {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
        ReviewerSummaryToken::Commented => Style::default().fg(Color::Yellow),
        ReviewerSummaryToken::Dismissed => Style::default().add_modifier(Modifier::DIM),
    }
}

fn comment_line(c: &PrComment, tree_prefix: &str, width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        tree_prefix.to_string(),
        Style::default().add_modifier(Modifier::DIM),
    )];
    if c.kind == PrCommentKind::Thread {
        spans.push(Span::styled(
            format!("@{} ", c.author),
            Style::default().fg(color_for_login(&c.author)),
        ));
    }
    spans.push(Span::raw(c.body.clone()));
    if let Some(path) = &c.path {
        spans.push(Span::styled(
            format!(" ({path})"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    if c.is_outdated {
        spans.push(Span::styled(
            " [outdated]",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    truncate_line(Line::from(spans), width)
}

/// Truncate a styled line to exactly `width` display columns, adding an
/// ellipsis only when content actually overflows. Styling is retained for the
/// visible portion of every span.
fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    if width == 0 {
        return Line::default();
    }

    let content_budget = width - 1;
    let mut used = 0;
    let mut visible = Vec::new();
    'spans: for span in line.spans {
        let mut text = String::new();
        for ch in span.content.chars() {
            let char_width = Span::raw(ch.to_string()).width();
            if used + char_width > content_budget {
                if !text.is_empty() {
                    visible.push(Span::styled(text, span.style));
                }
                break 'spans;
            }
            text.push(ch);
            used += char_width;
        }
        if !text.is_empty() {
            visible.push(Span::styled(text, span.style));
        }
    }
    visible.push(Span::raw("…"));
    Line::from(visible)
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

fn draw_footer(f: &mut Frame, area: Rect, state: &AppState, web_address: &str) {
    f.render_widget(Paragraph::new(footer_line(state, web_address)), area);
}

fn footer_line(state: &AppState, web_address: &str) -> Line<'static> {
    let web_label = format!("web {web_address} · ");
    if let AuthoredSearch::Editing(query) = &state.authored_search {
        return Line::from(format!("{web_label}inc search: {query}"));
    }

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

    let hint = match (&state.mode, &state.authored_search) {
        (ViewMode::Me, AuthoredSearch::Filtered(query)) => format!(
            "filter: {query} · Esc clear · / replace · ↑↓ move · h/l collapse/expand · Enter open · c copy   "
        ),
        (ViewMode::Me, _) => "↑↓ move · / search · h/l collapse/expand · Enter open · c copy · p review · e radar · r refresh · q quit   ".to_string(),
        (ViewMode::Radar, _) => "↑↓ move · Tab switch · Esc back · Enter open · x remove reviewer · r refresh · q quit   ".to_string(),
    };
    Line::from(vec![
        Span::styled(web_label, Style::default().add_modifier(Modifier::DIM)),
        Span::styled(hint, Style::default().add_modifier(Modifier::DIM)),
        Span::raw("["),
        status,
        Span::raw("]"),
    ])
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone, Utc};

    use super::*;
    use crate::model::ChecksRollup;

    fn test_pr(head_ref: &str) -> Pr {
        Pr {
            number: 7,
            title: "Improve rendering".to_string(),
            url: "https://github.com/o/r/pull/7".to_string(),
            is_draft: false,
            repo: "o/r".to_string(),
            base_ref: "main".to_string(),
            head_ref: head_ref.to_string(),
            author: "me".to_string(),
            reviewers: Vec::new(),
            updated_at: Utc.timestamp_opt(1, 0).unwrap(),
            merged_at: None,
            unresolved_comments: Vec::new(),
            checks: Vec::new(),
            checks_rollup: ChecksRollup::Unknown,
        }
    }

    #[test]
    fn pr_line_dims_head_ref_only_when_enabled_and_non_empty() {
        let line = pr_line(&test_pr("feature/render"), Some("me"), true, None, None);
        let suffix = line.spans.last().unwrap();
        assert_eq!(suffix.content.as_ref(), " [feature/render]");
        assert!(suffix.style.add_modifier.contains(Modifier::DIM));

        let disabled = pr_line(&test_pr("feature/render"), Some("me"), false, None, None);
        assert_eq!(
            disabled.spans.last().unwrap().content.as_ref(),
            "Improve rendering"
        );

        let empty = pr_line(&test_pr(""), Some("me"), true, None, None);
        assert_eq!(
            empty.spans.last().unwrap().content.as_ref(),
            "Improve rendering"
        );
    }

    #[test]
    fn pr_line_renders_disclosure_only_for_collapsible_prs() {
        let expanded = pr_line(
            &test_pr("branch"),
            Some("me"),
            true,
            Some("└─ "),
            Some(true),
        );
        assert_eq!(expanded.spans[1].content.as_ref(), "▾ ");

        let collapsed = pr_line(
            &test_pr("branch"),
            Some("me"),
            true,
            Some("└─ "),
            Some(false),
        );
        assert_eq!(collapsed.spans[1].content.as_ref(), "▸ ");

        let leaf = pr_line(&test_pr("branch"), Some("me"), true, Some("└─ "), None);
        assert_eq!(leaf.spans[1].content.as_ref(), "#7 ");
    }

    #[test]
    fn comment_line_uses_available_width_before_ellipsizing() {
        let body = "x".repeat(100);
        let comment = PrComment {
            kind: crate::model::PrCommentKind::Thread,
            author: "alice".to_string(),
            body: body.clone(),
            url: "https://example.test/review".to_string(),
            path: None,
            is_outdated: false,
        };

        let narrow = comment_line(&comment, "└─ ", 80);
        let narrow_text: String = narrow
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(narrow.width(), 80);
        assert!(narrow_text.ends_with('…'));

        let wide = comment_line(&comment, "└─ ", 140);
        let wide_text: String = wide
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(wide_text.ends_with(&body));
        assert!(!wide_text.ends_with('…'));
    }

    #[test]
    fn review_summary_line_omits_redundant_author() {
        let comment = PrComment {
            kind: PrCommentKind::ReviewSummary,
            author: "alice".to_string(),
            body: "Please skip the legacy flow".to_string(),
            url: "https://example.test/review".to_string(),
            path: None,
            is_outdated: false,
        };
        let line = comment_line(&comment, "   └─ ", 80);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "   └─ Please skip the legacy flow");
    }

    fn line_text(line: Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn footer_keeps_web_address_across_status_and_search_states() {
        let mut state = AppState::new();
        let address = "localhost:7011";
        let assert_address = |state: &AppState| {
            assert!(line_text(footer_line(state, address)).starts_with("web localhost:7011 · "));
        };

        assert_address(&state); // loading
        state.loading = false;
        assert_address(&state); // normal
        state.status = Some("refreshed".into());
        assert_address(&state);
        state.status = None;
        state.error = Some("boom".into());
        assert_address(&state);
        state.error = None;
        state.loaded_at = Some(Local.timestamp_opt(100, 0).unwrap());
        assert_address(&state);
        state.authored_search = AuthoredSearch::Filtered("needle".into());
        assert_address(&state);
        state.authored_search = AuthoredSearch::Editing("need".into());
        assert_eq!(
            line_text(footer_line(&state, address)),
            "web localhost:7011 · inc search: need"
        );
    }
}
