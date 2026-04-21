use std::{
    collections::BTreeSet,
    hash::{Hash, Hasher},
    io::{self, IsTerminal, Write},
};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};

use crate::{
    github,
    model::{
        Pr,
        ReleaseInfo,
        RepoReleaseInfo,
        ReviewState,
        ReviewerStatus,
        TagInfo,
        authors_for_me,
        authors_for_people,
        group_by_person,
        group_by_repo,
        human_age,
        merged_fetch_authors,
    },
};

pub const MERGED_PANE_CAP: usize = 10;

pub struct Report<'a> {
    pub viewer: &'a str,
    pub loaded_at: Option<DateTime<Local>>,
    pub sections: Vec<Section<'a>>,
}

pub struct Section<'a> {
    pub title: String,
    pub subtitle: Option<String>,
    pub count: usize,
    #[allow(dead_code)]
    pub kind: SectionKind,
    pub rows: Vec<Row<'a>>,
    pub empty_message: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionKind {
    MeReviewing,
    MeAuthored,
    People,
    RecentlyMerged,
    Releases,
}

pub enum Row<'a> {
    RepoHeader(String),
    PersonHeader(String),
    SubGroupLabel(&'static str),
    Pr {
        pr: &'a Pr,
        hide_author_if: Option<String>,
    },
    Reviewer(&'a ReviewerStatus),
    MergedPr(&'a Pr),
    ReleaseEntry {
        release: &'a ReleaseInfo,
        now: DateTime<Utc>,
    },
    ReleaseTag {
        repo: &'a str,
        tag: &'a TagInfo,
        now: DateTime<Utc>,
    },
    ReleaseEmpty,
}

impl Row<'_> {
    pub fn is_selectable(&self) -> bool {
        matches!(
            self,
            Row::Pr { .. } | Row::Reviewer(_) | Row::ReleaseEntry { .. } | Row::ReleaseTag { .. }
        )
    }
}

pub fn build_section_reviewing<'a>(reviewing: &'a [Pr]) -> Section<'a> {
    let mut rows: Vec<Row<'a>> = Vec::new();
    for (repo, group_prs) in group_by_repo(reviewing) {
        rows.push(Row::RepoHeader(repo));
        for pr in group_prs {
            rows.push(Row::Pr {
                pr,
                hide_author_if: None,
            });
            for r in &pr.reviewers {
                rows.push(Row::Reviewer(r));
            }
        }
    }
    Section {
        title: "Review requested of me".to_string(),
        subtitle: None,
        count: reviewing.len(),
        kind: SectionKind::MeReviewing,
        rows,
        empty_message: Some("(none)"),
    }
}

pub fn build_section_authored<'a>(authored: &'a [Pr], viewer: &str) -> Section<'a> {
    let mut rows: Vec<Row<'a>> = Vec::new();
    for (repo, group_prs) in group_by_repo(authored) {
        rows.push(Row::RepoHeader(repo));
        for pr in group_prs {
            rows.push(Row::Pr {
                pr,
                hide_author_if: Some(viewer.to_string()),
            });
            for r in &pr.reviewers {
                rows.push(Row::Reviewer(r));
            }
        }
    }
    Section {
        title: "Authored by me".to_string(),
        subtitle: Some(format!("@{viewer}")),
        count: authored.len(),
        kind: SectionKind::MeAuthored,
        rows,
        empty_message: Some("(none)"),
    }
}

pub fn build_section_people<'a>(
    authored: &'a [Pr],
    reviewing: &'a [Pr],
    viewer: &str,
) -> Section<'a> {
    let groups = group_by_person(authored, reviewing, viewer);
    let count = groups.len();
    let mut rows: Vec<Row<'a>> = Vec::new();
    for person in groups {
        rows.push(Row::PersonHeader(person.login.clone()));
        if !person.authored.is_empty() {
            rows.push(Row::SubGroupLabel("Authored"));
            for pr in person.authored {
                rows.push(Row::Pr {
                    pr,
                    hide_author_if: Some(person.login.clone()),
                });
                for r in &pr.reviewers {
                    rows.push(Row::Reviewer(r));
                }
            }
        }
        if !person.reviewing.is_empty() {
            rows.push(Row::SubGroupLabel("Reviewing"));
            for pr in person.reviewing {
                rows.push(Row::Pr {
                    pr,
                    hide_author_if: None,
                });
                for r in &pr.reviewers {
                    rows.push(Row::Reviewer(r));
                }
            }
        }
    }
    Section {
        title: "People".to_string(),
        subtitle: None,
        count,
        kind: SectionKind::People,
        rows,
        empty_message: Some("(no other people)"),
    }
}

pub fn build_section_merged<'a>(
    merged: &'a [Pr],
    allowed_authors: &BTreeSet<String>,
    cap: usize,
) -> Section<'a> {
    let visible: Vec<&'a Pr> = merged
        .iter()
        .filter(|p| allowed_authors.contains(&p.author.to_ascii_lowercase()))
        .take(cap)
        .collect();
    let count = visible.len();
    let rows: Vec<Row<'a>> = visible.into_iter().map(Row::MergedPr).collect();
    Section {
        title: "Recently merged PRs".to_string(),
        subtitle: None,
        count,
        kind: SectionKind::RecentlyMerged,
        rows,
        empty_message: Some("No recently merged PRs."),
    }
}

pub fn build_section_releases<'a>(
    releases: &'a [RepoReleaseInfo],
    now: DateTime<Utc>,
) -> Section<'a> {
    let mut rows: Vec<Row<'a>> = Vec::new();
    for info in releases {
        rows.push(Row::RepoHeader(info.repo.clone()));
        if !info.recent_releases.is_empty() {
            for release in &info.recent_releases {
                rows.push(Row::ReleaseEntry { release, now });
            }
        } else if let Some(tag) = &info.latest_tag {
            rows.push(Row::ReleaseTag {
                repo: info.repo.as_str(),
                tag,
                now,
            });
        } else {
            rows.push(Row::ReleaseEmpty);
        }
    }
    Section {
        title: "Recent releases".to_string(),
        subtitle: None,
        count: releases.len(),
        kind: SectionKind::Releases,
        rows,
        empty_message: Some("(no configured repos)"),
    }
}

pub fn build_full_report<'a>(
    viewer: &'a str,
    authored: &'a [Pr],
    reviewing: &'a [Pr],
    merged: &'a [Pr],
    releases: &'a [RepoReleaseInfo],
    now: DateTime<Utc>,
    loaded_at: Option<DateTime<Local>>,
) -> Report<'a> {
    let allowed: BTreeSet<String> = merged_fetch_authors(viewer, authored, reviewing)
        .into_iter()
        .collect();
    Report {
        viewer,
        loaded_at,
        sections: vec![
            build_section_reviewing(reviewing),
            build_section_releases(releases, now),
            build_section_authored(authored, viewer),
            build_section_people(authored, reviewing, viewer),
            build_section_merged(merged, &allowed, MERGED_PANE_CAP),
        ],
    }
}

pub fn allowed_authors_me(viewer: &str, reviewing: &[Pr]) -> BTreeSet<String> {
    authors_for_me(viewer, reviewing).into_iter().collect()
}

pub fn allowed_authors_people(authored: &[Pr], reviewing: &[Pr], viewer: &str) -> BTreeSet<String> {
    authors_for_people(authored, reviewing, viewer)
        .into_iter()
        .collect()
}

// --- color helpers (migrated from ui.rs) ---

pub fn rgb_for_login(login: &str) -> (u8, u8, u8) {
    let hue = (raw_hue(login) + hue_offset()).rem_euclid(360.0);
    hsl_to_rgb(hue, 0.80, 0.6)
}

fn raw_hue(login: &str) -> f32 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write_u32(100);
    login
        .trim_start_matches('@')
        .to_ascii_lowercase()
        .hash(&mut h);
    (h.finish() % 360) as f32
}

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

// --- console renderer ---

pub fn render(report: &Report<'_>, out: &mut impl Write) -> io::Result<()> {
    let use_color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let width = term_width();
    render_with(report, out, use_color, width)
}

pub fn render_with(
    report: &Report<'_>,
    out: &mut impl Write,
    use_color: bool,
    width: usize,
) -> io::Result<()> {
    if let Some(at) = report.loaded_at {
        writeln!(
            out,
            "{}rollup report — @{} — {}{}",
            bold(use_color),
            report.viewer,
            at.format("%Y-%m-%d %H:%M:%S"),
            reset(use_color),
        )?;
    } else {
        writeln!(
            out,
            "{}rollup report — @{}{}",
            bold(use_color),
            report.viewer,
            reset(use_color),
        )?;
    }

    for (i, section) in report.sections.iter().enumerate() {
        if i > 0 {
            writeln!(out)?;
        }
        render_section(section, out, use_color, width)?;
    }
    Ok(())
}

fn render_section(
    section: &Section<'_>,
    out: &mut impl Write,
    use_color: bool,
    width: usize,
) -> io::Result<()> {
    let header = match &section.subtitle {
        Some(sub) => format!("━━ {} ({}, {}) ━━", section.title, section.count, sub),
        None => format!("━━ {} ({}) ━━", section.title, section.count),
    };
    writeln!(out, "{}{}{}", bold(use_color), header, reset(use_color))?;

    if section.rows.is_empty() {
        if let Some(msg) = section.empty_message {
            writeln!(out, "  {}{}{}", dim(use_color), msg, reset(use_color))?;
        }
        return Ok(());
    }

    for row in &section.rows {
        render_row(row, out, use_color, width)?;
    }
    Ok(())
}

fn render_row(
    row: &Row<'_>,
    out: &mut impl Write,
    use_color: bool,
    width: usize,
) -> io::Result<()> {
    match row {
        Row::RepoHeader(repo) => {
            writeln!(
                out,
                "  {}{}{}",
                fg_named(use_color, 35),
                repo,
                reset(use_color)
            )
        }
        Row::PersonHeader(login) => {
            let (r, g, b) = rgb_for_login(login);
            writeln!(
                out,
                "  {}{}@{}{}",
                fg_rgb(use_color, r, g, b),
                bold(use_color),
                login,
                reset(use_color),
            )
        }
        Row::SubGroupLabel(label) => {
            writeln!(out, "    {}{}:{}", dim(use_color), label, reset(use_color))
        }
        Row::Pr { pr, hide_author_if } => {
            render_pr_line(pr, hide_author_if.as_deref(), out, use_color, width)
        }
        Row::Reviewer(r) => render_reviewer_line(r, out, use_color),
        Row::MergedPr(pr) => render_merged_pr_line(pr, out, use_color, width),
        Row::ReleaseEntry { release, now } => {
            render_release_entry_line(release, *now, out, use_color)
        }
        Row::ReleaseTag { tag, now, .. } => render_release_tag_line(tag, *now, out, use_color),
        Row::ReleaseEmpty => writeln!(
            out,
            "    {}(no releases or tags){}",
            dim(use_color),
            reset(use_color),
        ),
    }
}

fn render_release_entry_line(
    release: &ReleaseInfo,
    now: DateTime<Utc>,
    out: &mut impl Write,
    use_color: bool,
) -> io::Result<()> {
    let label = release
        .name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| release.tag_name.clone());
    write!(
        out,
        "    {} ({})",
        label,
        human_age(release.created_at, now),
    )?;
    if release.is_prerelease {
        write!(out, " {}[pre]{}", dim(use_color), reset(use_color))?;
    }
    writeln!(out)
}

fn render_release_tag_line(
    tag: &TagInfo,
    now: DateTime<Utc>,
    out: &mut impl Write,
    use_color: bool,
) -> io::Result<()> {
    writeln!(
        out,
        "    {}tag: {} ({}){}",
        dim(use_color),
        tag.name,
        human_age(tag.committed_at, now),
        reset(use_color),
    )
}

fn render_pr_line(
    pr: &Pr,
    hide_author_if: Option<&str>,
    out: &mut impl Write,
    use_color: bool,
    width: usize,
) -> io::Result<()> {
    let indent = "    ";
    let mut prefix = String::new();
    let mut plain_prefix_cols = indent.chars().count();

    prefix.push_str(indent);
    let num = format!("#{} ", pr.number);
    plain_prefix_cols += num.chars().count();
    prefix.push_str(&fg_named(use_color, 34));
    prefix.push_str(&num);
    prefix.push_str(&reset(use_color));

    if pr.is_draft {
        let tag = "[draft] ";
        plain_prefix_cols += tag.chars().count();
        prefix.push_str(&dim(use_color));
        prefix.push_str(tag);
        prefix.push_str(&reset(use_color));
    }

    let show_author = hide_author_if.is_none_or(|v| v != pr.author);
    if show_author {
        let handle = format!("@{} ", pr.author);
        plain_prefix_cols += handle.chars().count();
        let (r, g, b) = rgb_for_login(&pr.author);
        prefix.push_str(&fg_rgb(use_color, r, g, b));
        prefix.push_str(&handle);
        prefix.push_str(&reset(use_color));
    }

    let title = truncate_title(&pr.title, width, plain_prefix_cols);
    writeln!(out, "{prefix}{title}")
}

fn render_reviewer_line(
    r: &ReviewerStatus,
    out: &mut impl Write,
    use_color: bool,
) -> io::Result<()> {
    let (glyph, glyph_color, crossed) = match r.state {
        ReviewState::Approved => ("✓", Some(32u8), false),
        ReviewState::ChangesRequested => ("✗", Some(31u8), false),
        ReviewState::Commented => ("◉", Some(33u8), false),
        ReviewState::NoReview => ("○", None, false),
        ReviewState::Dismissed => ("⊘", None, true),
    };

    write!(out, "      ")?;
    if let Some(code) = glyph_color {
        write!(
            out,
            "{}{}{} ",
            fg_named(use_color, code),
            glyph,
            reset(use_color),
        )?;
    } else if crossed {
        write!(
            out,
            "{}{}{}{} ",
            dim(use_color),
            crossed_out(use_color),
            glyph,
            reset(use_color),
        )?;
    } else {
        write!(out, "{}{}{} ", dim(use_color), glyph, reset(use_color))?;
    }

    let (rr, gg, bb) = rgb_for_login(&r.login);
    write!(
        out,
        "{}{}{}",
        fg_rgb(use_color, rr, gg, bb),
        r.login,
        reset(use_color),
    )?;

    if r.requested {
        writeln!(
            out,
            "  {}{}[req]{}",
            fg_named(use_color, 36),
            bold(use_color),
            reset(use_color),
        )
    } else {
        writeln!(out, "  {}(reviewed){}", dim(use_color), reset(use_color))
    }
}

fn render_merged_pr_line(
    pr: &Pr,
    out: &mut impl Write,
    use_color: bool,
    width: usize,
) -> io::Result<()> {
    let indent = "  ";
    let mut prefix = String::new();
    let mut plain_prefix_cols = indent.chars().count();

    prefix.push_str(indent);
    let num = format!("#{} ", pr.number);
    plain_prefix_cols += num.chars().count();
    prefix.push_str(&fg_named(use_color, 34));
    prefix.push_str(&num);
    prefix.push_str(&reset(use_color));

    let repo = format!("{} ", pr.repo);
    plain_prefix_cols += repo.chars().count();
    prefix.push_str(&fg_named(use_color, 35));
    prefix.push_str(&repo);
    prefix.push_str(&reset(use_color));

    let handle = format!("@{} ", pr.author);
    plain_prefix_cols += handle.chars().count();
    let (r, g, b) = rgb_for_login(&pr.author);
    prefix.push_str(&fg_rgb(use_color, r, g, b));
    prefix.push_str(&handle);
    prefix.push_str(&reset(use_color));

    let title = truncate_title(&pr.title, width, plain_prefix_cols);
    writeln!(out, "{prefix}{title}")
}

fn truncate_title(title: &str, width: usize, prefix_cols: usize) -> String {
    if prefix_cols + 1 >= width {
        return title.to_string();
    }
    let budget = width - prefix_cols;
    if title.chars().count() <= budget {
        return title.to_string();
    }
    if budget <= 1 {
        return "…".to_string();
    }
    let mut s: String = title.chars().take(budget - 1).collect();
    s.push('…');
    s
}

fn term_width() -> usize {
    crossterm::terminal::size()
        .ok()
        .map(|(c, _)| c as usize)
        .unwrap_or(120)
}

fn bold(on: bool) -> String {
    if on { "\x1b[1m".into() } else { String::new() }
}

fn dim(on: bool) -> String {
    if on { "\x1b[2m".into() } else { String::new() }
}

fn crossed_out(on: bool) -> String {
    if on { "\x1b[9m".into() } else { String::new() }
}

fn reset(on: bool) -> String {
    if on { "\x1b[0m".into() } else { String::new() }
}

fn fg_named(on: bool, code: u8) -> String {
    if on {
        format!("\x1b[{code}m")
    } else {
        String::new()
    }
}

fn fg_rgb(on: bool, r: u8, g: u8, b: u8) -> String {
    if on {
        format!("\x1b[38;2;{r};{g};{b}m")
    } else {
        String::new()
    }
}

pub fn run() -> Result<()> {
    let data = github::fetch()?;
    if let Some(err) = &data.config_error {
        eprintln!("config: {err}");
    }
    let now = Utc::now();
    let report = build_full_report(
        &data.viewer,
        &data.authored,
        &data.reviewing,
        &data.merged,
        &data.releases,
        now,
        Some(Local::now()),
    );
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    match render(&report, &mut lock) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::model::ReviewerKind;

    fn ts(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn pr(
        repo: &str,
        number: u64,
        author: &str,
        is_draft: bool,
        updated: i64,
        reviewers: Vec<ReviewerStatus>,
    ) -> Pr {
        Pr {
            number,
            title: format!("t{number}"),
            url: format!("https://github.com/{repo}/pull/{number}"),
            is_draft,
            repo: repo.to_string(),
            author: author.to_string(),
            reviewers,
            updated_at: ts(updated),
            merged_at: None,
        }
    }

    fn merged_pr(repo: &str, number: u64, author: &str, merged: i64) -> Pr {
        Pr {
            merged_at: Some(ts(merged)),
            ..pr(repo, number, author, false, merged, vec![])
        }
    }

    fn user(login: &str, requested: bool) -> ReviewerStatus {
        ReviewerStatus {
            login: login.to_string(),
            kind: ReviewerKind::User,
            state: ReviewState::NoReview,
            requested,
        }
    }

    fn team(name: &str, requested: bool) -> ReviewerStatus {
        ReviewerStatus {
            login: name.to_string(),
            kind: ReviewerKind::Team,
            state: ReviewState::NoReview,
            requested,
        }
    }

    #[test]
    fn build_section_reviewing_row_order() {
        let prs = vec![
            pr("z/r", 1, "a", false, 100, vec![user("r1", true)]),
            pr("a/r", 2, "b", true, 500, vec![]),
            pr("a/r", 3, "c", false, 200, vec![]),
            pr("a/r", 4, "d", false, 400, vec![]),
        ];
        let section = build_section_reviewing(&prs);
        let kinds: Vec<String> = section
            .rows
            .iter()
            .map(|r| match r {
                Row::RepoHeader(h) => format!("H:{h}"),
                Row::Pr { pr, .. } => format!("P:{}", pr.number),
                Row::Reviewer(r) => format!("R:{}", r.login),
                _ => "?".into(),
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["H:a/r", "P:4", "P:3", "P:2", "H:z/r", "P:1", "R:r1"],
        );
    }

    #[test]
    fn build_section_people_excludes_viewer_and_teams() {
        let viewer = "me";
        let authored = vec![pr(
            "o/r",
            1,
            viewer,
            false,
            100,
            vec![user("alice", true), team("@core", true)],
        )];
        let reviewing = vec![pr(
            "o/r",
            2,
            "bob",
            false,
            200,
            vec![user(viewer, true), team("@core", true)],
        )];
        let section = build_section_people(&authored, &reviewing, viewer);

        let person_headers: Vec<&str> = section
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::PersonHeader(login) => Some(login.as_str()),
                _ => None,
            })
            .collect();
        assert!(!person_headers.contains(&viewer));
        assert!(!person_headers.iter().any(|l| l.starts_with('@')));
        assert!(person_headers.contains(&"alice"));
        assert!(person_headers.contains(&"bob"));

        let alice_idx = section
            .rows
            .iter()
            .position(|r| matches!(r, Row::PersonHeader(h) if h == "alice"))
            .expect("alice present");
        match &section.rows[alice_idx + 1] {
            Row::SubGroupLabel(s) => assert_eq!(*s, "Reviewing"),
            _ => panic!("expected SubGroupLabel after PersonHeader"),
        }
        assert!(matches!(section.rows[alice_idx + 2], Row::Pr { .. }));
    }

    #[test]
    fn build_section_merged_caps_and_filters() {
        let m1 = merged_pr("o/r", 2, "bob", 400);
        let m2 = merged_pr("o/r", 3, "alice", 300);
        let m3 = merged_pr("o/r", 4, "carol", 200);
        let m4 = merged_pr("o/r", 1, "alice", 100);
        let desc_sorted = vec![m1, m2, m3, m4];
        let allowed: BTreeSet<String> = ["alice".to_string(), "carol".to_string()]
            .into_iter()
            .collect();
        let section = build_section_merged(&desc_sorted, &allowed, 2);
        let nums: Vec<u64> = section
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::MergedPr(pr) => Some(pr.number),
                _ => None,
            })
            .collect();
        assert_eq!(nums, vec![3, 4]);
        assert_eq!(section.count, 2);
    }

    fn ts_utc(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn release_info(
        repo: &str,
        releases: Vec<crate::model::ReleaseInfo>,
        latest_tag: Option<crate::model::TagInfo>,
    ) -> RepoReleaseInfo {
        RepoReleaseInfo {
            repo: repo.to_string(),
            recent_releases: releases,
            latest_tag,
        }
    }

    fn rel(tag: &str, created_secs: i64, is_pre: bool) -> crate::model::ReleaseInfo {
        crate::model::ReleaseInfo {
            tag_name: tag.to_string(),
            name: Some(tag.to_string()),
            url: format!("https://github.com/o/r/releases/tag/{tag}"),
            created_at: ts_utc(created_secs),
            is_prerelease: is_pre,
        }
    }

    fn tag(name: &str, at: i64) -> crate::model::TagInfo {
        crate::model::TagInfo {
            name: name.to_string(),
            committed_at: ts_utc(at),
        }
    }

    #[test]
    fn build_full_report_contains_all_five_sections() {
        let viewer = "me";
        let authored = vec![pr("o/r", 1, viewer, false, 100, vec![])];
        let reviewing = vec![pr("o/r", 2, "bob", false, 200, vec![])];
        let merged: Vec<Pr> = vec![];
        let releases: Vec<RepoReleaseInfo> = vec![];
        let now = ts_utc(1_000_000);
        let report =
            build_full_report(viewer, &authored, &reviewing, &merged, &releases, now, None);
        assert_eq!(report.sections.len(), 5);
        assert_eq!(report.sections[0].kind, SectionKind::MeReviewing);
        assert_eq!(report.sections[1].kind, SectionKind::Releases);
        assert_eq!(report.sections[2].kind, SectionKind::MeAuthored);
        assert_eq!(report.sections[3].kind, SectionKind::People);
        assert_eq!(report.sections[4].kind, SectionKind::RecentlyMerged);
    }

    #[test]
    fn build_section_releases_tree_shape() {
        let rs = vec![
            release_info(
                "a/b",
                vec![rel("v1.1", 200, false), rel("v1.0", 100, false)],
                None,
            ),
            release_info("c/d", vec![], Some(tag("v2", 200))),
            release_info("e/f", vec![], None),
        ];
        let section = build_section_releases(&rs, ts_utc(1000));
        assert_eq!(section.kind, SectionKind::Releases);
        assert_eq!(section.count, 3);

        let kinds: Vec<&'static str> = section
            .rows
            .iter()
            .map(|r| match r {
                Row::RepoHeader(_) => "header",
                Row::ReleaseEntry { .. } => "entry",
                Row::ReleaseTag { .. } => "tag",
                Row::ReleaseEmpty => "empty",
                _ => "?",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "header", "entry", "entry", "header", "tag", "header", "empty"
            ],
        );
    }

    #[test]
    fn console_render_no_color_has_no_escape_bytes() {
        let viewer = "me";
        let authored = vec![pr("o/r", 1, viewer, false, 100, vec![user("alice", true)])];
        let reviewing = vec![pr("o/r", 2, "bob", false, 200, vec![])];
        let merged: Vec<Pr> = vec![];
        let releases: Vec<RepoReleaseInfo> = vec![];
        let now = ts_utc(1_000_000);
        let report =
            build_full_report(viewer, &authored, &reviewing, &merged, &releases, now, None);
        let mut out: Vec<u8> = Vec::new();
        render_with(&report, &mut out, false, 120).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains('\x1b'), "expected no ANSI escapes, got: {s:?}");
    }

    #[test]
    fn console_render_includes_all_section_titles() {
        let viewer = "me";
        let authored = vec![pr("o/r", 1, viewer, false, 100, vec![])];
        let reviewing = vec![pr("o/r", 2, "bob", false, 200, vec![])];
        let merged: Vec<Pr> = vec![];
        let releases: Vec<RepoReleaseInfo> = vec![];
        let now = ts_utc(1_000_000);
        let report =
            build_full_report(viewer, &authored, &reviewing, &merged, &releases, now, None);
        let mut out: Vec<u8> = Vec::new();
        render_with(&report, &mut out, false, 120).unwrap();
        let s = String::from_utf8(out).unwrap();
        for title in [
            "Review requested of me",
            "Recent releases",
            "Authored by me",
            "People",
            "Recently merged",
        ] {
            assert!(s.contains(title), "missing {title:?} in:\n{s}");
        }
    }

    #[test]
    fn console_render_handles_empty_sections() {
        let viewer = "me";
        let authored: Vec<Pr> = vec![];
        let reviewing: Vec<Pr> = vec![];
        let merged: Vec<Pr> = vec![];
        let releases: Vec<RepoReleaseInfo> = vec![];
        let now = ts_utc(1_000_000);
        let report =
            build_full_report(viewer, &authored, &reviewing, &merged, &releases, now, None);
        let mut out: Vec<u8> = Vec::new();
        render_with(&report, &mut out, false, 120).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("(none)"));
        assert!(s.contains("(no other people)"));
        assert!(s.contains("No recently merged PRs."));
        assert!(s.contains("(no configured repos)"));
    }

    #[test]
    fn console_render_release_lines_format() {
        let now = ts_utc(1_000_000);
        let rs = vec![
            release_info(
                "o/pre",
                vec![
                    rel("v1.2.3", 1_000_000 - 3 * 86_400, true),
                    rel("v1.2.2", 1_000_000 - 10 * 86_400, false),
                ],
                None,
            ),
            release_info("o/tagged", vec![], Some(tag("v0.1.0", 1_000_000 - 86_400))),
            release_info("o/none", vec![], None),
        ];
        let section = build_section_releases(&rs, now);
        let mut out: Vec<u8> = Vec::new();
        render_section(&section, &mut out, false, 120).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("o/pre"), "{s}");
        assert!(s.contains("v1.2.3 (3d)"), "{s}");
        assert!(s.contains("[pre]"), "{s}");
        assert!(s.contains("v1.2.2 (1w)"), "{s}");
        assert!(s.contains("o/tagged"), "{s}");
        assert!(s.contains("tag: v0.1.0 (1d)"), "{s}");
        assert!(s.contains("o/none"), "{s}");
        assert!(s.contains("(no releases or tags)"), "{s}");
    }

    #[test]
    fn row_is_selectable_covers_releases_and_tags() {
        let p = pr("o/r", 1, "a", false, 100, vec![]);
        let r = user("alice", true);
        let release = rel("v1", 100, false);
        let tg = tag("v0", 50);
        let now = ts_utc(1);
        assert!(
            Row::Pr {
                pr: &p,
                hide_author_if: None,
            }
            .is_selectable()
        );
        assert!(Row::Reviewer(&r).is_selectable());
        assert!(
            Row::ReleaseEntry {
                release: &release,
                now,
            }
            .is_selectable()
        );
        assert!(
            Row::ReleaseTag {
                repo: "o/r",
                tag: &tg,
                now,
            }
            .is_selectable()
        );
        assert!(!Row::ReleaseEmpty.is_selectable());
        assert!(!Row::RepoHeader("o/r".to_string()).is_selectable());
        assert!(!Row::PersonHeader("alice".to_string()).is_selectable());
        assert!(!Row::SubGroupLabel("Authored").is_selectable());
        assert!(!Row::MergedPr(&p).is_selectable());
    }
}
