use std::hash::{Hash, Hasher};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::{
    app::{AppState, Focus},
    model::{Pr, ReviewState, ReviewerKind, ReviewerStatus, group_by_repo},
};

pub fn draw(f: &mut Frame, state: &mut AppState) {
    let outer = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(f.area());
    let sections = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(outer[0]);

    let focus = state.focus;
    let viewer = state.viewer.clone();
    draw_section(
        f,
        sections[0],
        "Review requested of me",
        &state.reviewing,
        state.reviewing_sel,
        &mut state.reviewing_list_state,
        focus == Focus::Reviewing,
        None,
    );
    draw_section(
        f,
        sections[1],
        "Authored by me",
        &state.authored,
        state.authored_sel,
        &mut state.authored_list_state,
        focus == Focus::Authored,
        viewer.as_deref(),
    );
    draw_footer(f, outer[1], state);
}

const SCROLL_MARGIN: usize = 4;

#[allow(clippy::too_many_arguments)]
fn draw_section(
    f: &mut Frame,
    area: Rect,
    title: &str,
    prs: &[Pr],
    selection: usize,
    list_state: &mut ListState,
    focused: bool,
    // When `Some`, PR rows authored by this login skip the inline author tag
    // (the viewer's login is shown in the pane title instead).
    viewer: Option<&str>,
) {
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title_line = match viewer {
        Some(v) => format!(" {title} ({}, @{v}) ", prs.len()),
        None => format!(" {title} ({}) ", prs.len()),
    };
    let block = Block::default()
        .title(title_line)
        .borders(Borders::ALL)
        .border_style(border_style);

    if prs.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let msg = Paragraph::new(Span::styled(
            "(none)",
            Style::default().add_modifier(Modifier::DIM),
        ));
        f.render_widget(msg, inner);
        return;
    }

    let groups = group_by_repo(prs);
    let mut items: Vec<ListItem> = Vec::new();
    let mut row_of_sel: Vec<usize> = Vec::new();

    for (repo, group_prs) in &groups {
        items.push(ListItem::new(Line::from(Span::styled(
            repo.clone(),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ))));
        for pr in group_prs {
            row_of_sel.push(items.len());
            items.push(ListItem::new(pr_line(pr, viewer)));
            for r in &pr.reviewers {
                row_of_sel.push(items.len());
                items.push(ListItem::new(reviewer_line(r)));
            }
        }
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
        ReviewState::Approved => ("+", Style::default().fg(Color::Green)),
        ReviewState::ChangesRequested => ("x", Style::default().fg(Color::Red)),
        ReviewState::Commented => (".", Style::default().fg(Color::Yellow)),
        ReviewState::NoReview => ("?", Style::default().add_modifier(Modifier::DIM)),
        ReviewState::Dismissed => (
            "-",
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

/// Deterministic per-login color: hash the normalized login into a hue, then
/// map through HSL at the same L/S (~0.56/0.56) as the original muted orange
/// so every name sits on the same perceptual "shelf" of lightness/saturation
/// — only the hue varies. Good for scanning for a particular name.
fn color_for_login(login: &str) -> Color {
    let hue = (raw_hue(login) + hue_offset()).rem_euclid(360.0);
    // Base shelf was (S=0.56, L=0.56). Cut saturation 30%, bump lightness 20%.
    let (r, g, b) = hsl_to_rgb(hue, 0.80, 0.6);
    Color::Rgb(r, g, b)
}

fn raw_hue(login: &str) -> f32 {
    // DefaultHasher is SipHash with a fixed (0, 0) key, so the mapping is
    // stable across runs and machines.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // Tweak the hash to get better results.
    h.write_u32(100);
    login
        .trim_start_matches('@')
        .to_ascii_lowercase()
        .hash(&mut h);
    (h.finish() % 360) as f32
}

/// Global hue shift picked so that `wbbradley` always lands on the original
/// muted orange (~29°). Every other login is rotated by the same amount.
fn hue_offset() -> f32 {
    use std::sync::OnceLock;
    const ANCHOR_LOGIN: &str = "wbbradley";
    const ANCHOR_HUE: f32 = 27.0;
    static OFFSET: OnceLock<f32> = OnceLock::new();
    *OFFSET.get_or_init(|| (ANCHOR_HUE - raw_hue(ANCHOR_LOGIN)).rem_euclid(360.0))
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r1, g1, b1) = if hp < 1.0 {
        (c, x, 0.0)
    } else if hp < 2.0 {
        (x, c, 0.0)
    } else if hp < 3.0 {
        (0.0, c, x)
    } else if hp < 4.0 {
        (0.0, x, c)
    } else if hp < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = l - c / 2.0;
    let to_byte = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (to_byte(r1), to_byte(g1), to_byte(b1))
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
            "Loading…",
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

    let line = Line::from(vec![
        Span::styled(
            "↑↓ move · Tab switch · Enter open · x remove reviewer · r refresh · q quit   ",
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::raw("["),
        status,
        Span::raw("]"),
    ]);
    f.render_widget(Paragraph::new(line), area);
}
