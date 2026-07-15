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
        CheckState, CheckStatus, ChecksRollup, Pr, PrComment, PrTreeNode, ReleaseInfo,
        RepoReleaseInfo, ReviewState, ReviewerStatus, TagInfo, authored_tree, authors_for_me,
        authors_for_people, checks_by_display_priority, group_by_person, group_by_repo, human_age,
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

/// Explicit collapse-state choices for the Authored pane's section nodes,
/// keyed by stable PR identity `(repo, number)` plus the section kind. Storing
/// the chosen value (rather than a deviation from the default) keeps a user's
/// choice stable when a data-driven default changes after a refresh.
pub type ToggledSet = std::collections::HashMap<(String, u64, SectionId), bool>;

/// Identifies one of a PR's collapsible child sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SectionId {
    Checks,
    ValidResults,
    Reviewers,
    Comments,
    Stacked,
}

impl SectionId {
    pub fn label(self) -> &'static str {
        match self {
            SectionId::Checks => "Checks",
            SectionId::ValidResults => "Valid Results",
            SectionId::Reviewers => "Reviewers",
            SectionId::Comments => "Open comments",
            SectionId::Stacked => "Stacked PRs",
        }
    }

    /// Static section default. Checks supplies a data-driven default at its
    /// call site; Valid Results and Reviewers start collapsed, while Open
    /// comments and Stacked PRs start expanded.
    pub fn default_expanded(self) -> bool {
        !matches!(
            self,
            SectionId::Checks | SectionId::ValidResults | SectionId::Reviewers
        )
    }
}

/// The at-a-glance merge-readiness summary carried on a Checks `SectionHeader`
/// row: the rollup signal plus the required-check ratio shown while collapsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChecksSummary {
    pub rollup: ChecksRollup,
    /// Required checks that have passed (Success/Neutral/Skipped).
    pub passed_required: usize,
    /// Total required checks.
    pub total_required: usize,
}

impl ChecksSummary {
    pub fn of(pr: &Pr) -> Self {
        let total_required = pr.checks.iter().filter(|c| c.required).count();
        let passed_required = pr
            .checks
            .iter()
            .filter(|c| {
                c.required
                    && matches!(
                        c.state,
                        CheckState::Success | CheckState::Neutral | CheckState::Skipped
                    )
            })
            .count();
        ChecksSummary {
            rollup: pr.checks_rollup,
            passed_required,
            total_required,
        }
    }

    /// The collapsed-header ratio text, e.g. `4/4 required`, `no required
    /// checks`, or `unknown`. The state glyph is rendered separately.
    pub fn ratio_text(&self) -> String {
        match self.rollup {
            ChecksRollup::Unknown => "unknown".to_string(),
            _ if self.total_required == 0 => "no required checks".to_string(),
            _ => format!("{}/{} required", self.passed_required, self.total_required),
        }
    }
}

/// One distinct reviewer response-state token, shown as a set on the Reviewers
/// header so a `✗ changes` is visible while the section is collapsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewerSummaryToken {
    Requested,
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
}

impl ReviewerSummaryToken {
    pub fn tui_label(self) -> &'static str {
        match self {
            ReviewerSummaryToken::Requested => "req",
            ReviewerSummaryToken::Approved => "✓ approved",
            ReviewerSummaryToken::ChangesRequested => "✗ changes",
            ReviewerSummaryToken::Commented => "◉ commented",
            ReviewerSummaryToken::Dismissed => "⊘ dismissed",
        }
    }

    pub fn console_label(self) -> &'static str {
        match self {
            ReviewerSummaryToken::Requested => "req",
            ReviewerSummaryToken::Approved => "approved",
            ReviewerSummaryToken::ChangesRequested => "changes-requested",
            ReviewerSummaryToken::Commented => "commented",
            ReviewerSummaryToken::Dismissed => "dismissed",
        }
    }
}

/// Whether a PR's section is currently expanded. An explicit user choice wins
/// over `default`, which may be derived from the PR's current data.
pub fn is_expanded(t: &ToggledSet, repo: &str, number: u64, id: SectionId, default: bool) -> bool {
    t.get(&(repo.to_string(), number, id))
        .copied()
        .unwrap_or(default)
}

/// Store a PR section's explicit expanded/collapsed choice.
pub fn set_expanded(t: &mut ToggledSet, repo: &str, number: u64, id: SectionId, want: bool) {
    t.insert((repo.to_string(), number, id), want);
}

fn check_is_valid(check: &CheckStatus) -> bool {
    matches!(
        check.state,
        CheckState::Success | CheckState::Skipped | CheckState::Neutral
    )
}

fn checks_default_expanded(pr: &Pr) -> bool {
    pr.checks
        .iter()
        .any(|check| matches!(check.state, CheckState::Failure | CheckState::Error))
}

/// The distinct reviewer response states present on a PR, stably ordered: `req`
/// first (any reviewer still requested), then, for reviewers who have reviewed,
/// each distinct verdict in the order approved, changes, commented, dismissed.
/// `NoReview` contributes nothing on its own (only via `requested`).
pub fn reviewer_summary(reviewers: &[ReviewerStatus]) -> Vec<ReviewerSummaryToken> {
    let mut out: Vec<ReviewerSummaryToken> = Vec::new();
    if reviewers.iter().any(|r| r.requested) {
        out.push(ReviewerSummaryToken::Requested);
    }
    for (state, token) in [
        (ReviewState::Approved, ReviewerSummaryToken::Approved),
        (
            ReviewState::ChangesRequested,
            ReviewerSummaryToken::ChangesRequested,
        ),
        (ReviewState::Commented, ReviewerSummaryToken::Commented),
        (ReviewState::Dismissed, ReviewerSummaryToken::Dismissed),
    ] {
        if reviewers.iter().any(|r| !r.requested && r.state == state) {
            out.push(token);
        }
    }
    out
}

pub enum Row<'a> {
    RepoHeader(String),
    PersonHeader(String),
    SubGroupLabel(&'static str),
    Pr {
        pr: &'a Pr,
        hide_author_if: Option<String>,
        /// Whether to append the PR's non-empty head branch as a secondary
        /// label. True only for rows built for the Authored pane/report.
        show_head_ref: bool,
        /// Full leading indent string for tree rendering (with `├`/`└`/`│`
        /// connectors). `None` in flat panes, which fall back to a fixed indent.
        tree_prefix: Option<String>,
        /// For a stacked child PR in the Authored pane, the parent PR's number
        /// (same repo), so `h` on the child collapses the parent's Stacked PRs
        /// node. `None` for roots and in every other pane's builder.
        stacked_under: Option<u64>,
    },
    Reviewer {
        r: &'a ReviewerStatus,
        /// Leading indent for tree rendering; `None` uses the fixed indent.
        tree_prefix: Option<String>,
    },
    /// A selectable, collapsible section node (e.g. "Reviewers") sitting at a
    /// PR's child indent. `l`/Right expands, `h`/Left collapses; its child rows
    /// are only emitted when `expanded`. The display label comes from
    /// `section.label()`. `summary` is non-empty only for Reviewers; `checks` is
    /// `Some` only for the Checks header.
    SectionHeader {
        section: SectionId,
        expanded: bool,
        summary: Vec<ReviewerSummaryToken>,
        checks: Option<ChecksSummary>,
        tree_prefix: String,
    },
    /// An unresolved review-thread comment under a PR. Selectable; Enter opens
    /// the comment's permalink.
    Comment {
        c: &'a PrComment,
        tree_prefix: String,
    },
    /// A single check under a PR's Checks section. Selectable; Enter opens the
    /// check's details URL (falling back to the PR).
    Check {
        c: &'a CheckStatus,
        tree_prefix: String,
    },
    MergedPr {
        pr: &'a Pr,
        now: DateTime<Utc>,
    },
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
            Row::Pr { .. }
                | Row::Reviewer { .. }
                | Row::SectionHeader { .. }
                | Row::Comment { .. }
                | Row::Check { .. }
                | Row::ReleaseEntry { .. }
                | Row::ReleaseTag { .. }
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
                show_head_ref: false,
                tree_prefix: None,
                stacked_under: None,
            });
            for r in &pr.reviewers {
                rows.push(Row::Reviewer {
                    r,
                    tree_prefix: None,
                });
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

pub fn build_section_authored<'a>(
    authored: &'a [Pr],
    viewer: &str,
    toggled: &ToggledSet,
) -> Section<'a> {
    let mut rows: Vec<Row<'a>> = Vec::new();
    for (repo, group_prs) in group_by_repo(authored) {
        rows.push(Row::RepoHeader(repo));
        // Within each repo, nest PRs by merge target (stacked-PR tree). Roots
        // start at the two-space base under the repo header.
        let tree = authored_tree(&group_prs);
        let n = tree.len();
        for (i, node) in tree.iter().enumerate() {
            push_pr(&mut rows, node, viewer, "  ", i + 1 == n, None, toggled);
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

/// Build the TUI's filtered Authored tree. Unlike [`build_section_authored`],
/// this walks every logical descendant regardless of collapse state and emits
/// only matching rows plus the repo/PR/section path needed to reach them.
/// Persistent `ToggledSet` state is deliberately absent: `search_collapsed`
/// contains only temporary folds made while a committed filter is active.
pub fn build_section_authored_filtered<'a>(
    authored: &'a [Pr],
    viewer: &str,
    query: &str,
    search_collapsed: &ToggledSet,
) -> Section<'a> {
    let query = query.to_lowercase();
    let mut rows: Vec<Row<'a>> = Vec::new();
    let mut count = 0;

    for (repo, group_prs) in group_by_repo(authored) {
        let tree = authored_tree(&group_prs);
        let matching_roots: Vec<&PrTreeNode<'_>> = tree
            .iter()
            .filter(|node| filtered_pr_matches(node, viewer, &query))
            .collect();
        if visible_text_matches(&repo, &query) || !matching_roots.is_empty() {
            rows.push(Row::RepoHeader(repo));
            count += matching_roots
                .iter()
                .map(|node| count_filtered_prs(node, viewer, &query))
                .sum::<usize>();
            let root_count = matching_roots.len();
            for (index, node) in matching_roots.into_iter().enumerate() {
                push_filtered_pr(
                    &mut rows,
                    node,
                    viewer,
                    &query,
                    "  ",
                    index + 1 == root_count,
                    None,
                    search_collapsed,
                );
            }
        }
    }

    Section {
        title: "Authored by me".to_string(),
        subtitle: Some(format!("@{viewer}")),
        count,
        kind: SectionKind::MeAuthored,
        rows,
        empty_message: Some("(no matches)"),
    }
}

fn count_filtered_prs(node: &PrTreeNode<'_>, viewer: &str, query: &str) -> usize {
    1 + node
        .children
        .iter()
        .filter(|child| filtered_pr_matches(child, viewer, query))
        .map(|child| count_filtered_prs(child, viewer, query))
        .sum::<usize>()
}

fn visible_text_matches(text: &str, query: &str) -> bool {
    text.to_lowercase().contains(query)
}

fn pr_visible_text(pr: &Pr, viewer: &str) -> String {
    let draft = if pr.is_draft { " [draft]" } else { "" };
    let author = if pr.author == viewer {
        String::new()
    } else {
        format!(" @{}", pr.author)
    };
    let head_ref = if pr.head_ref.is_empty() {
        String::new()
    } else {
        format!(" [{}]", pr.head_ref)
    };
    format!("#{}{}{} {}{head_ref}", pr.number, draft, author, pr.title)
}

fn reviewer_visible_text(reviewer: &ReviewerStatus) -> String {
    let glyph = match reviewer.state {
        ReviewState::Approved => "✓",
        ReviewState::ChangesRequested => "✗",
        ReviewState::Commented => "◉",
        ReviewState::NoReview => "○",
        ReviewState::Dismissed => "⊘",
    };
    let badge = if reviewer.requested {
        "[req]"
    } else {
        "(reviewed)"
    };
    format!("{glyph} {} {badge}", reviewer.login)
}

fn comment_visible_text(comment: &PrComment) -> String {
    format!(
        "@{} {} {} {}",
        comment.author,
        comment.body,
        comment.path.as_deref().unwrap_or(""),
        if comment.is_outdated {
            "[outdated]"
        } else {
            ""
        }
    )
}

fn check_visible_text(check: &CheckStatus) -> String {
    let glyph = match check.state {
        CheckState::Success => "✓",
        CheckState::Failure | CheckState::Error => "✗",
        CheckState::Pending => "◉",
        CheckState::Skipped => "⊘",
        CheckState::Neutral => "○",
    };
    format!(
        "{glyph} {} {}",
        check.name,
        if check.required { "" } else { "(not required)" }
    )
}

fn section_visible_text(section: SectionId, pr: &Pr) -> String {
    match section {
        SectionId::Checks => {
            let summary = ChecksSummary::of(pr);
            let glyph = match summary.rollup {
                ChecksRollup::Green => "✓",
                ChecksRollup::Red => "✗",
                ChecksRollup::Pending => "◉",
                ChecksRollup::Unknown => "○",
            };
            format!("{} {glyph} {}", section.label(), summary.ratio_text())
        }
        SectionId::ValidResults => section.label().to_string(),
        SectionId::Reviewers => {
            let summary = reviewer_summary(&pr.reviewers)
                .into_iter()
                .map(ReviewerSummaryToken::tui_label)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{} {summary}", section.label())
        }
        SectionId::Comments | SectionId::Stacked => section.label().to_string(),
    }
}

fn filtered_pr_matches(node: &PrTreeNode<'_>, viewer: &str, query: &str) -> bool {
    let pr = node.pr;
    visible_text_matches(&pr_visible_text(pr, viewer), query)
        || (!pr.checks.is_empty()
            && (visible_text_matches(&section_visible_text(SectionId::Checks, pr), query)
                || (pr.checks.iter().any(check_is_valid)
                    && visible_text_matches(
                        &section_visible_text(SectionId::ValidResults, pr),
                        query,
                    ))
                || pr
                    .checks
                    .iter()
                    .any(|check| visible_text_matches(&check_visible_text(check), query))))
        || (!pr.reviewers.is_empty()
            && (visible_text_matches(&section_visible_text(SectionId::Reviewers, pr), query)
                || pr
                    .reviewers
                    .iter()
                    .any(|reviewer| visible_text_matches(&reviewer_visible_text(reviewer), query))))
        || (!pr.unresolved_comments.is_empty()
            && (visible_text_matches(&section_visible_text(SectionId::Comments, pr), query)
                || pr
                    .unresolved_comments
                    .iter()
                    .any(|comment| visible_text_matches(&comment_visible_text(comment), query))))
        || (!node.children.is_empty()
            && (visible_text_matches(&section_visible_text(SectionId::Stacked, pr), query)
                || node
                    .children
                    .iter()
                    .any(|child| filtered_pr_matches(child, viewer, query))))
}

#[allow(clippy::too_many_arguments)]
fn push_filtered_pr<'a>(
    rows: &mut Vec<Row<'a>>,
    node: &PrTreeNode<'a>,
    viewer: &str,
    query: &str,
    prefix: &str,
    is_last: bool,
    stacked_under: Option<u64>,
    search_collapsed: &ToggledSet,
) {
    let connector = if is_last { "└─ " } else { "├─ " };
    rows.push(Row::Pr {
        pr: node.pr,
        hide_author_if: Some(viewer.to_string()),
        show_head_ref: true,
        tree_prefix: Some(format!("{prefix}{connector}")),
        stacked_under,
    });
    let child_base = format!("{prefix}{}", if is_last { "   " } else { "│  " });
    let pr = node.pr;

    if !pr.checks.is_empty() {
        let checks_section_matches =
            visible_text_matches(&section_visible_text(SectionId::Checks, pr), query);
        let valid_section_matches = pr.checks.iter().any(check_is_valid)
            && visible_text_matches(&section_visible_text(SectionId::ValidResults, pr), query);
        let matching: Vec<&CheckStatus> = checks_by_display_priority(&pr.checks)
            .into_iter()
            .filter(|check| visible_text_matches(&check_visible_text(check), query))
            .collect();
        let matching_actionable: Vec<&CheckStatus> = matching
            .iter()
            .copied()
            .filter(|check| !check_is_valid(check))
            .collect();
        let matching_valid: Vec<&CheckStatus> = matching
            .into_iter()
            .filter(|check| check_is_valid(check))
            .collect();
        let checks_has_descendants =
            !matching_actionable.is_empty() || valid_section_matches || !matching_valid.is_empty();
        if checks_section_matches || checks_has_descendants {
            let checks_expanded = search_collapsed
                .get(&(pr.repo.clone(), pr.number, SectionId::Checks))
                .copied()
                .unwrap_or(true)
                && checks_has_descendants;
            rows.push(Row::SectionHeader {
                section: SectionId::Checks,
                expanded: checks_expanded,
                summary: Vec::new(),
                checks: Some(ChecksSummary::of(pr)),
                tree_prefix: child_base.clone(),
            });
            if checks_expanded {
                let show_valid_results = valid_section_matches || !matching_valid.is_empty();
                let direct_count = matching_actionable.len() + usize::from(show_valid_results);
                for (index, check) in matching_actionable.into_iter().enumerate() {
                    rows.push(Row::Check {
                        c: check,
                        tree_prefix: format!(
                            "{child_base}{}",
                            if index + 1 == direct_count {
                                "└─ "
                            } else {
                                "├─ "
                            }
                        ),
                    });
                }
                if show_valid_results {
                    let valid_expanded = search_collapsed
                        .get(&(pr.repo.clone(), pr.number, SectionId::ValidResults))
                        .copied()
                        .unwrap_or(true)
                        && !matching_valid.is_empty();
                    rows.push(Row::SectionHeader {
                        section: SectionId::ValidResults,
                        expanded: valid_expanded,
                        summary: Vec::new(),
                        checks: None,
                        tree_prefix: format!("{child_base}└─ "),
                    });
                    if valid_expanded {
                        let valid_base = format!("{child_base}   ");
                        let count = matching_valid.len();
                        for (index, check) in matching_valid.into_iter().enumerate() {
                            rows.push(Row::Check {
                                c: check,
                                tree_prefix: format!(
                                    "{valid_base}{}",
                                    if index + 1 == count {
                                        "└─ "
                                    } else {
                                        "├─ "
                                    }
                                ),
                            });
                        }
                    }
                }
            }
        }
    }

    if !pr.reviewers.is_empty() {
        let section_matches =
            visible_text_matches(&section_visible_text(SectionId::Reviewers, pr), query);
        let matching: Vec<&ReviewerStatus> = pr
            .reviewers
            .iter()
            .filter(|reviewer| visible_text_matches(&reviewer_visible_text(reviewer), query))
            .collect();
        if section_matches || !matching.is_empty() {
            let collapsed = search_collapsed
                .get(&(pr.repo.clone(), pr.number, SectionId::Reviewers))
                .is_some_and(|expanded| !expanded);
            rows.push(Row::SectionHeader {
                section: SectionId::Reviewers,
                expanded: !collapsed && !matching.is_empty(),
                summary: reviewer_summary(&pr.reviewers),
                checks: None,
                tree_prefix: child_base.clone(),
            });
            if !collapsed {
                let count = matching.len();
                for (index, reviewer) in matching.into_iter().enumerate() {
                    rows.push(Row::Reviewer {
                        r: reviewer,
                        tree_prefix: Some(format!(
                            "{child_base}{}",
                            if index + 1 == count {
                                "└─ "
                            } else {
                                "├─ "
                            }
                        )),
                    });
                }
            }
        }
    }

    if !pr.unresolved_comments.is_empty() {
        let section_matches =
            visible_text_matches(&section_visible_text(SectionId::Comments, pr), query);
        let matching: Vec<&PrComment> = pr
            .unresolved_comments
            .iter()
            .filter(|comment| visible_text_matches(&comment_visible_text(comment), query))
            .collect();
        if section_matches || !matching.is_empty() {
            let collapsed = search_collapsed
                .get(&(pr.repo.clone(), pr.number, SectionId::Comments))
                .is_some_and(|expanded| !expanded);
            rows.push(Row::SectionHeader {
                section: SectionId::Comments,
                expanded: !collapsed && !matching.is_empty(),
                summary: Vec::new(),
                checks: None,
                tree_prefix: child_base.clone(),
            });
            if !collapsed {
                let count = matching.len();
                for (index, comment) in matching.into_iter().enumerate() {
                    rows.push(Row::Comment {
                        c: comment,
                        tree_prefix: format!(
                            "{child_base}{}",
                            if index + 1 == count {
                                "└─ "
                            } else {
                                "├─ "
                            }
                        ),
                    });
                }
            }
        }
    }

    if !node.children.is_empty() {
        let section_matches =
            visible_text_matches(&section_visible_text(SectionId::Stacked, pr), query);
        let matching: Vec<&PrTreeNode<'_>> = node
            .children
            .iter()
            .filter(|child| filtered_pr_matches(child, viewer, query))
            .collect();
        if section_matches || !matching.is_empty() {
            let collapsed = search_collapsed
                .get(&(pr.repo.clone(), pr.number, SectionId::Stacked))
                .is_some_and(|expanded| !expanded);
            rows.push(Row::SectionHeader {
                section: SectionId::Stacked,
                expanded: !collapsed && !matching.is_empty(),
                summary: Vec::new(),
                checks: None,
                tree_prefix: child_base.clone(),
            });
            if !collapsed {
                let count = matching.len();
                for (index, child) in matching.into_iter().enumerate() {
                    push_filtered_pr(
                        rows,
                        child,
                        viewer,
                        query,
                        &child_base,
                        index + 1 == count,
                        Some(pr.number),
                        search_collapsed,
                    );
                }
            }
        }
    }
}

/// Flatten one PR tree node (and its stacked children) into rows. Each PR's
/// children are grouped into up to four ordered sections — Checks, Reviewers,
/// Open comments, Stacked PRs — drawn with classic `├─`/`└─`/`│` connectors.
/// Every non-empty section always emits a selectable, collapsible `SectionHeader`;
/// its child rows are emitted only when that `(repo, number, section)` is
/// expanded per `toggled` (Reviewers collapsed by default, the others
/// expanded).
///
/// `stacked_under` is the parent PR's number when this node is a stacked child
/// (so `h` on the child can collapse the parent's Stacked PRs node); `None` for
/// roots.
///
/// `node: &PrTreeNode<'a>` yields `node.pr: &'a Pr`; iterating
/// `node.pr.reviewers` / `node.pr.unresolved_comments` produces `&'a _`, so the
/// rows carry `'a` references into `authored` and are independent of the local
/// `tree` that may drop between loop iterations.
#[allow(clippy::too_many_arguments)]
fn push_pr<'a>(
    rows: &mut Vec<Row<'a>>,
    node: &PrTreeNode<'a>,
    viewer: &str,
    prefix: &str,
    is_last: bool,
    stacked_under: Option<u64>,
    toggled: &ToggledSet,
) {
    let connector = if is_last { "└─ " } else { "├─ " };
    rows.push(Row::Pr {
        pr: node.pr,
        hide_author_if: Some(viewer.to_string()),
        show_head_ref: true,
        tree_prefix: Some(format!("{prefix}{connector}")),
        stacked_under,
    });
    let child_base = format!("{prefix}{}", if is_last { "   " } else { "│  " });
    let repo = node.pr.repo.as_str();
    let number = node.pr.number;

    // Checks section — emitted first and opened by default when any check is
    // failing/errored. Actionable checks remain direct children; completed
    // outcomes live under a nested, default-collapsed Valid Results node.
    if !node.pr.checks.is_empty() {
        let expanded = is_expanded(
            toggled,
            repo,
            number,
            SectionId::Checks,
            checks_default_expanded(node.pr),
        );
        rows.push(Row::SectionHeader {
            section: SectionId::Checks,
            expanded,
            summary: Vec::new(),
            checks: Some(ChecksSummary::of(node.pr)),
            tree_prefix: child_base.clone(),
        });
        if expanded {
            let checks = checks_by_display_priority(&node.pr.checks);
            let actionable: Vec<&CheckStatus> = checks
                .iter()
                .copied()
                .filter(|check| !check_is_valid(check))
                .collect();
            let valid: Vec<&CheckStatus> = checks
                .into_iter()
                .filter(|check| check_is_valid(check))
                .collect();
            let direct_count = actionable.len() + usize::from(!valid.is_empty());
            for (i, c) in actionable.into_iter().enumerate() {
                let conn = if i + 1 == direct_count {
                    "└─ "
                } else {
                    "├─ "
                };
                rows.push(Row::Check {
                    c,
                    tree_prefix: format!("{child_base}{conn}"),
                });
            }
            if !valid.is_empty() {
                let valid_expanded = is_expanded(
                    toggled,
                    repo,
                    number,
                    SectionId::ValidResults,
                    SectionId::ValidResults.default_expanded(),
                );
                rows.push(Row::SectionHeader {
                    section: SectionId::ValidResults,
                    expanded: valid_expanded,
                    summary: Vec::new(),
                    checks: None,
                    tree_prefix: format!("{child_base}└─ "),
                });
                if valid_expanded {
                    let valid_base = format!("{child_base}   ");
                    let count = valid.len();
                    for (index, check) in valid.into_iter().enumerate() {
                        rows.push(Row::Check {
                            c: check,
                            tree_prefix: format!(
                                "{valid_base}{}",
                                if index + 1 == count {
                                    "└─ "
                                } else {
                                    "├─ "
                                }
                            ),
                        });
                    }
                }
            }
        }
    }

    // Reviewers section.
    if !node.pr.reviewers.is_empty() {
        let expanded = is_expanded(
            toggled,
            repo,
            number,
            SectionId::Reviewers,
            SectionId::Reviewers.default_expanded(),
        );
        rows.push(Row::SectionHeader {
            section: SectionId::Reviewers,
            expanded,
            summary: reviewer_summary(&node.pr.reviewers),
            checks: None,
            tree_prefix: child_base.clone(),
        });
        if expanded {
            let m = node.pr.reviewers.len();
            for (i, r) in node.pr.reviewers.iter().enumerate() {
                let c = if i + 1 == m { "└─ " } else { "├─ " };
                rows.push(Row::Reviewer {
                    r,
                    tree_prefix: Some(format!("{child_base}{c}")),
                });
            }
        }
    }

    // Open comments section.
    if !node.pr.unresolved_comments.is_empty() {
        let expanded = is_expanded(
            toggled,
            repo,
            number,
            SectionId::Comments,
            SectionId::Comments.default_expanded(),
        );
        rows.push(Row::SectionHeader {
            section: SectionId::Comments,
            expanded,
            summary: Vec::new(),
            checks: None,
            tree_prefix: child_base.clone(),
        });
        if expanded {
            let m = node.pr.unresolved_comments.len();
            for (i, c) in node.pr.unresolved_comments.iter().enumerate() {
                let conn = if i + 1 == m { "└─ " } else { "├─ " };
                rows.push(Row::Comment {
                    c,
                    tree_prefix: format!("{child_base}{conn}"),
                });
            }
        }
    }

    // Stacked PRs section.
    if !node.children.is_empty() {
        let expanded = is_expanded(
            toggled,
            repo,
            number,
            SectionId::Stacked,
            SectionId::Stacked.default_expanded(),
        );
        rows.push(Row::SectionHeader {
            section: SectionId::Stacked,
            expanded,
            summary: Vec::new(),
            checks: None,
            tree_prefix: child_base.clone(),
        });
        if expanded {
            let m = node.children.len();
            for (i, kid) in node.children.iter().enumerate() {
                push_pr(
                    rows,
                    kid,
                    viewer,
                    &child_base,
                    i + 1 == m,
                    Some(number),
                    toggled,
                );
            }
        }
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
                    show_head_ref: false,
                    tree_prefix: None,
                    stacked_under: None,
                });
                for r in &pr.reviewers {
                    rows.push(Row::Reviewer {
                        r,
                        tree_prefix: None,
                    });
                }
            }
        }
        if !person.reviewing.is_empty() {
            rows.push(Row::SubGroupLabel("Reviewing"));
            for pr in person.reviewing {
                rows.push(Row::Pr {
                    pr,
                    hide_author_if: None,
                    show_head_ref: false,
                    tree_prefix: None,
                    stacked_under: None,
                });
                for r in &pr.reviewers {
                    rows.push(Row::Reviewer {
                        r,
                        tree_prefix: None,
                    });
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
    now: DateTime<Utc>,
) -> Section<'a> {
    let visible: Vec<&'a Pr> = merged
        .iter()
        .filter(|p| allowed_authors.contains(&p.author.to_ascii_lowercase()))
        .take(cap)
        .collect();
    let count = visible.len();
    let rows: Vec<Row<'a>> = visible
        .into_iter()
        .map(|pr| Row::MergedPr { pr, now })
        .collect();
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
    // The console has no interactive controls, so make every section visible.
    // Explicitly expand every interactive level for complete console output.
    let mut toggled = ToggledSet::new();
    for pr in authored {
        set_expanded(&mut toggled, &pr.repo, pr.number, SectionId::Checks, true);
        set_expanded(
            &mut toggled,
            &pr.repo,
            pr.number,
            SectionId::ValidResults,
            true,
        );
        set_expanded(
            &mut toggled,
            &pr.repo,
            pr.number,
            SectionId::Reviewers,
            true,
        );
    }
    Report {
        viewer,
        loaded_at,
        sections: vec![
            build_section_reviewing(reviewing),
            build_section_releases(releases, now),
            build_section_authored(authored, viewer, &toggled),
            build_section_people(authored, reviewing, viewer),
            build_section_merged(merged, &allowed, MERGED_PANE_CAP, now),
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
        Row::Pr {
            pr,
            hide_author_if,
            show_head_ref,
            tree_prefix,
            ..
        } => render_pr_line(
            pr,
            hide_author_if.as_deref(),
            *show_head_ref,
            tree_prefix.as_deref(),
            out,
            use_color,
            width,
        ),
        Row::Reviewer { r, tree_prefix } => {
            render_reviewer_line(r, tree_prefix.as_deref(), out, use_color)
        }
        Row::SectionHeader {
            section,
            expanded,
            summary,
            checks,
            tree_prefix,
        } => render_section_header_line(
            *section,
            *expanded,
            summary,
            checks.as_ref(),
            tree_prefix,
            out,
            use_color,
        ),
        Row::Comment { c, tree_prefix } => render_comment_line(c, tree_prefix, out, use_color),
        Row::Check { c, tree_prefix } => render_check_line(c, tree_prefix, out, use_color),
        Row::MergedPr { pr, now } => render_merged_pr_line(pr, *now, out, use_color, width),
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

#[allow(clippy::too_many_arguments)]
fn render_section_header_line(
    section: SectionId,
    expanded: bool,
    summary: &[ReviewerSummaryToken],
    checks: Option<&ChecksSummary>,
    tree_prefix: &str,
    out: &mut impl Write,
    use_color: bool,
) -> io::Result<()> {
    let glyph = if expanded { "▾" } else { "▸" };
    write!(
        out,
        "{}{}{} {}{}",
        dim(use_color),
        tree_prefix,
        glyph,
        section.label(),
        reset(use_color),
    )?;
    if let Some(cs) = checks {
        write!(
            out,
            " {}[{}]{}",
            dim(use_color),
            checks_summary_text(cs),
            reset(use_color),
        )?;
    } else if !summary.is_empty() {
        let tokens: Vec<&str> = summary.iter().map(|t| t.console_label()).collect();
        write!(
            out,
            " {}[{}]{}",
            dim(use_color),
            tokens.join(", "),
            reset(use_color),
        )?;
    }
    writeln!(out)
}

/// Glyph-free checks summary for `rollup report`, e.g. `4/4 required`,
/// `3/4 required failing`, `2/4 required pending`, `no required checks`,
/// `unknown`.
pub fn checks_summary_text(cs: &ChecksSummary) -> String {
    let ratio = cs.ratio_text();
    match cs.rollup {
        ChecksRollup::Red => format!("{ratio} failing"),
        ChecksRollup::Pending => format!("{ratio} pending"),
        ChecksRollup::Green | ChecksRollup::Unknown => ratio,
    }
}

fn render_check_line(
    c: &CheckStatus,
    tree_prefix: &str,
    out: &mut impl Write,
    use_color: bool,
) -> io::Result<()> {
    write!(out, "{}{}{}", dim(use_color), tree_prefix, reset(use_color))?;
    let (glyph, glyph_color) = check_glyph(c.state);
    if let Some(code) = glyph_color {
        write!(
            out,
            "{}{}{} ",
            fg_named(use_color, code),
            glyph,
            reset(use_color),
        )?;
    } else {
        write!(out, "{}{}{} ", dim(use_color), glyph, reset(use_color))?;
    }
    // Non-required checks are de-emphasized (dim name + suffix) since they
    // never affect the merge-readiness signal.
    if c.required {
        write!(out, "{}", c.name)?;
        writeln!(out)
    } else {
        writeln!(
            out,
            "{}{} (not required){}",
            dim(use_color),
            c.name,
            reset(use_color),
        )
    }
}

/// Console glyph + optional ANSI color code for a check state, reusing the
/// reviewer palette (`✓` green, `✗` red, `◉` yellow, `○`/`⊘` dim).
fn check_glyph(state: CheckState) -> (&'static str, Option<u8>) {
    match state {
        CheckState::Success => ("✓", Some(32)),
        CheckState::Failure | CheckState::Error => ("✗", Some(31)),
        CheckState::Pending => ("◉", Some(33)),
        CheckState::Skipped => ("⊘", None),
        CheckState::Neutral => ("○", None),
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
    show_head_ref: bool,
    tree_prefix: Option<&str>,
    out: &mut impl Write,
    use_color: bool,
    width: usize,
) -> io::Result<()> {
    let mut prefix = String::new();
    let mut plain_prefix_cols;

    match tree_prefix {
        Some(tp) => {
            plain_prefix_cols = tp.chars().count();
            prefix.push_str(&dim(use_color));
            prefix.push_str(tp);
            prefix.push_str(&reset(use_color));
        }
        None => {
            let indent = "    ";
            plain_prefix_cols = indent.chars().count();
            prefix.push_str(indent);
        }
    }

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

    let suffix = show_head_ref
        .then_some(pr.head_ref.as_str())
        .filter(|head_ref| !head_ref.is_empty())
        .map(|head_ref| format!(" [{head_ref}]"));
    let budget = width.saturating_sub(plain_prefix_cols);
    let (title, suffix) = match suffix {
        Some(suffix) if suffix.chars().count() < budget => {
            let title_budget = budget - suffix.chars().count();
            (truncate_text(&pr.title, title_budget), Some(suffix))
        }
        _ => (truncate_text(&pr.title, budget), None),
    };
    write!(out, "{prefix}{title}")?;
    if let Some(suffix) = suffix {
        write!(out, "{}{}{}", dim(use_color), suffix, reset(use_color))?;
    }
    writeln!(out)
}

fn render_reviewer_line(
    r: &ReviewerStatus,
    tree_prefix: Option<&str>,
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

    match tree_prefix {
        Some(tp) => write!(out, "{}{}{}", dim(use_color), tp, reset(use_color))?,
        None => write!(out, "      ")?,
    }
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

fn render_comment_line(
    c: &PrComment,
    tree_prefix: &str,
    out: &mut impl Write,
    use_color: bool,
) -> io::Result<()> {
    write!(out, "{}{}{}", dim(use_color), tree_prefix, reset(use_color))?;
    let (r, g, b) = rgb_for_login(&c.author);
    write!(
        out,
        "{}@{}{}",
        fg_rgb(use_color, r, g, b),
        c.author,
        reset(use_color),
    )?;
    write!(out, " {}", c.body)?;
    if let Some(path) = &c.path {
        write!(out, " {}({}){}", dim(use_color), path, reset(use_color))?;
    }
    if c.is_outdated {
        write!(out, " {}[outdated]{}", dim(use_color), reset(use_color))?;
    }
    writeln!(out)
}

fn render_merged_pr_line(
    pr: &Pr,
    now: DateTime<Utc>,
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

    if let Some(merged_at) = pr.merged_at {
        let age = format!("({}) ", human_age(merged_at, now));
        plain_prefix_cols += age.chars().count();
        prefix.push_str(&age);
    }

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
    truncate_text(title, width.saturating_sub(prefix_cols))
}

fn truncate_text(text: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if text.chars().count() <= budget {
        return text.to_string();
    }
    if budget <= 1 {
        return "…".to_string();
    }
    let mut s: String = text.chars().take(budget - 1).collect();
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
    for w in &data.warnings {
        eprintln!("warning: {w}");
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
            base_ref: "main".to_string(),
            head_ref: format!("branch-{number}"),
            author: author.to_string(),
            reviewers,
            updated_at: ts(updated),
            merged_at: None,
            unresolved_comments: vec![],
            checks: vec![],
            checks_rollup: ChecksRollup::Unknown,
        }
    }

    fn check(name: &str, state: CheckState, required: bool) -> CheckStatus {
        CheckStatus {
            name: name.to_string(),
            state,
            url: Some(format!("https://ci/{name}")),
            required,
        }
    }

    fn comment(author: &str, body: &str, path: Option<&str>, is_outdated: bool) -> PrComment {
        PrComment {
            author: author.to_string(),
            body: body.to_string(),
            url: format!("https://github.com/o/r/pull/1#discussion_r_{author}"),
            path: path.map(str::to_string),
            is_outdated,
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

    /// A reviewer who has already reviewed (`requested == false`) with a given
    /// verdict — used to exercise the response-state summary.
    fn reviewed(login: &str, state: ReviewState) -> ReviewerStatus {
        ReviewerStatus {
            login: login.to_string(),
            kind: ReviewerKind::User,
            state,
            requested: false,
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
                Row::Reviewer { r, .. } => format!("R:{}", r.login),
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
        let now = ts_utc(1_000_000);
        let section = build_section_merged(&desc_sorted, &allowed, 2, now);
        let nums: Vec<u64> = section
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::MergedPr { pr, .. } => Some(pr.number),
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

    fn pr_refs(repo: &str, number: u64, base: &str, head: &str) -> Pr {
        Pr {
            base_ref: base.to_string(),
            head_ref: head.to_string(),
            ..pr(repo, number, "me", false, 100, vec![])
        }
    }

    #[test]
    fn build_section_authored_tree_shape() {
        // A(main←a) is the root; B(a←b) stacks on A; C(main←c) is a second root.
        let a = pr_refs("o/r", 1, "main", "a");
        let b = pr_refs("o/r", 2, "a", "b");
        let c = pr_refs("o/r", 3, "main", "c");
        let authored = vec![a, b, c];
        let section = build_section_authored(&authored, "me", &ToggledSet::new());
        assert_eq!(section.kind, SectionKind::MeAuthored);
        assert_eq!(section.count, 3);

        // Pre-order: repo header, A, A's (default-expanded) Stacked PRs header,
        // its child B, then sibling root C.
        let seq: Vec<String> = section
            .rows
            .iter()
            .map(|r| match r {
                Row::RepoHeader(h) => format!("H:{h}"),
                Row::Pr { pr, .. } => format!("P:{}", pr.number),
                Row::Reviewer { r, .. } => format!("R:{}", r.login),
                Row::SectionHeader { section, .. } => format!("S:{}", section.label()),
                _ => "?".into(),
            })
            .collect();
        assert_eq!(seq, vec!["H:o/r", "P:1", "S:Stacked PRs", "P:2", "P:3"]);

        // Grab each PR's tree prefix by number.
        let prefix = |num: u64| -> String {
            section
                .rows
                .iter()
                .find_map(|r| match r {
                    Row::Pr {
                        pr,
                        tree_prefix: Some(tp),
                        ..
                    } if pr.number == num => Some(tp.clone()),
                    _ => None,
                })
                .expect("pr present with prefix")
        };
        // Roots: A has a following sibling (its child doesn't count as a
        // sibling), so it uses `├─`; C is the last root, so `└─`.
        assert_eq!(prefix(1), "  ├─ ");
        assert_eq!(prefix(3), "  └─ ");
        // B is A's only child: one continuation bar under A (A is not last),
        // then a `└─` connector.
        assert_eq!(prefix(2), "  │  └─ ");

        assert!(
            section
                .rows
                .iter()
                .filter_map(|row| match row {
                    Row::Pr { show_head_ref, .. } => Some(show_head_ref),
                    _ => None,
                })
                .all(|show_head_ref| *show_head_ref)
        );
    }

    #[test]
    fn non_authored_pr_rows_do_not_enable_head_ref_labels() {
        let authored = vec![pr("o/r", 1, "alice", false, 100, vec![])];
        let reviewing = vec![pr("o/r", 2, "bob", false, 200, vec![])];
        let sections = [
            build_section_reviewing(&reviewing),
            build_section_people(&authored, &reviewing, "me"),
        ];

        for section in sections {
            assert!(
                section
                    .rows
                    .iter()
                    .filter_map(|row| match row {
                        Row::Pr { show_head_ref, .. } => Some(show_head_ref),
                        _ => None,
                    })
                    .all(|show_head_ref| !*show_head_ref)
            );
        }
    }

    #[test]
    fn authored_console_head_ref_is_dimmed_width_safe_and_omits_empty_ref() {
        let mut labeled = pr("o/r", 1, "me", false, 100, vec![]);
        labeled.title = "A deliberately long title".to_string();
        labeled.head_ref = "feature/foo".to_string();
        let mut empty = pr("o/r", 2, "me", false, 90, vec![]);
        empty.head_ref.clear();
        let authored = vec![labeled, empty];
        let section = build_section_authored(&authored, "me", &ToggledSet::new());

        let mut plain = Vec::new();
        render_section(&section, &mut plain, false, 30).unwrap();
        let plain = String::from_utf8(plain).unwrap();
        let labeled_line = plain.lines().find(|line| line.contains("#1 ")).unwrap();
        let empty_line = plain.lines().find(|line| line.contains("#2 ")).unwrap();
        assert!(labeled_line.ends_with(" [feature/foo]"), "{labeled_line}");
        assert!(labeled_line.chars().count() <= 30, "{labeled_line}");
        assert!(!empty_line.contains(" []"), "{empty_line}");

        let mut colored = Vec::new();
        render_section(&section, &mut colored, true, 120).unwrap();
        let colored = String::from_utf8(colored).unwrap();
        assert!(colored.contains("\x1b[2m [feature/foo]\x1b[0m"));
    }

    #[test]
    fn build_section_authored_three_deep_bars() {
        // A(main←a) sole root with two children B1(a←b1) and B2(a←b2); B1 has
        // its own child C(b1←c). Verifies a mid-level `│` bar three deep: C
        // sits under B1, and B1 is *not* the last child of A, so B1's column
        // must keep drawing `│` down to C.
        let a = pr_refs("o/r", 1, "main", "a");
        let b1 = pr_refs("o/r", 2, "a", "b1");
        let c = pr_refs("o/r", 3, "b1", "c");
        let b2 = pr_refs("o/r", 4, "a", "b2");
        // Sort within the repo is stable at equal update times, so input order
        // (a, b1, c, b2) is preserved and B1 precedes B2 as A's children.
        let authored = vec![a, b1, c, b2];
        let section = build_section_authored(&authored, "me", &ToggledSet::new());

        let prefix = |num: u64| -> String {
            section
                .rows
                .iter()
                .find_map(|r| match r {
                    Row::Pr {
                        pr,
                        tree_prefix: Some(tp),
                        ..
                    } if pr.number == num => Some(tp.clone()),
                    _ => None,
                })
                .expect("pr present with prefix")
        };
        // A: sole root → `└─`; child base is five spaces.
        assert_eq!(prefix(1), "  └─ ");
        // B1: first of A's two children → `├─`, no ancestor bar (A is last).
        assert_eq!(prefix(2), "     ├─ ");
        // B2: last child → `└─`.
        assert_eq!(prefix(4), "     └─ ");
        // C: only child of B1. B1 is *not* last, so its column keeps a `│`
        // bar, then C's own `└─`.
        assert_eq!(prefix(3), "     │  └─ ");
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
    fn full_console_report_expands_every_authored_section() {
        let viewer = "me";
        let mut p = pr("o/r", 1, viewer, false, 100, vec![user("alice", true)]);
        p.checks = vec![check("build", CheckState::Success, true)];
        p.checks_rollup = ChecksRollup::Green;
        p.unresolved_comments = vec![comment("bob", "please fix", None, false)];
        let authored = vec![p];
        let report = build_full_report(viewer, &authored, &[], &[], &[], ts_utc(1_000_000), None);
        let section = report
            .sections
            .iter()
            .find(|section| section.kind == SectionKind::MeAuthored)
            .unwrap();

        assert!(section.rows.iter().all(|row| !matches!(
            row,
            Row::SectionHeader {
                expanded: false,
                ..
            }
        )));
        assert!(
            section
                .rows
                .iter()
                .any(|row| matches!(row, Row::Check { .. }))
        );
        assert!(
            section
                .rows
                .iter()
                .any(|row| matches!(row, Row::Reviewer { .. }))
        );
        assert!(
            section
                .rows
                .iter()
                .any(|row| matches!(row, Row::Comment { .. }))
        );
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
    fn console_render_merged_pr_includes_age() {
        let now = ts_utc(1_000_000);
        let merged = vec![merged_pr("o/r", 42, "alice", 1_000_000 - 3 * 86_400)];
        let allowed: BTreeSet<String> = ["alice".to_string()].into_iter().collect();
        let section = build_section_merged(&merged, &allowed, 10, now);
        let mut out: Vec<u8> = Vec::new();
        render_section(&section, &mut out, false, 120).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("#42"), "{s}");
        assert!(s.contains("(3d)"), "{s}");
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
                show_head_ref: false,
                tree_prefix: None,
                stacked_under: None,
            }
            .is_selectable()
        );
        assert!(
            Row::Reviewer {
                r: &r,
                tree_prefix: None,
            }
            .is_selectable()
        );
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
        let cm = comment("carol", "add a test", Some("src/foo.rs"), false);
        assert!(
            Row::Comment {
                c: &cm,
                tree_prefix: "  ".to_string(),
            }
            .is_selectable()
        );
        assert!(!Row::ReleaseEmpty.is_selectable());
        assert!(!Row::RepoHeader("o/r".to_string()).is_selectable());
        assert!(!Row::PersonHeader("alice".to_string()).is_selectable());
        assert!(!Row::SubGroupLabel("Authored").is_selectable());
        assert!(!Row::MergedPr { pr: &p, now }.is_selectable());
        // Section headers are now selectable collapse controls.
        assert!(
            Row::SectionHeader {
                section: SectionId::Reviewers,
                expanded: false,
                summary: vec![ReviewerSummaryToken::Requested],
                checks: None,
                tree_prefix: "  ".to_string(),
            }
            .is_selectable()
        );
        let ck = check("build", CheckState::Success, true);
        assert!(
            Row::Check {
                c: &ck,
                tree_prefix: "  ".to_string(),
            }
            .is_selectable()
        );
    }

    #[test]
    fn build_section_authored_always_emits_reviewers_header_collapsed_by_default() {
        // A single PR with only reviewers now ALWAYS emits a Reviewers header,
        // collapsed by default: the header shows (with a non-empty summary) but
        // no Reviewer rows are emitted.
        let p = pr("o/r", 1, "me", false, 100, vec![user("alice", true)]);
        let authored = vec![p];
        let section = build_section_authored(&authored, "me", &ToggledSet::new());

        let header = section.rows.iter().find_map(|r| match r {
            Row::SectionHeader {
                section: SectionId::Reviewers,
                expanded,
                summary,
                ..
            } => Some((*expanded, summary.clone())),
            _ => None,
        });
        let (expanded, summary) = header.expect("reviewers-only PR must emit a Reviewers header");
        assert!(!expanded, "Reviewers is collapsed by default");
        assert!(
            !summary.is_empty(),
            "summary carries the response-state set"
        );
        assert!(
            !section
                .rows
                .iter()
                .any(|r| matches!(r, Row::Reviewer { .. })),
            "collapsed Reviewers emits no reviewer rows",
        );
    }

    #[test]
    fn build_section_authored_emits_headers_for_multi_section() {
        // A PR with reviewers AND open comments → both headers appear at the
        // child base. Reviewers is collapsed by default (no reviewer rows); Open
        // comments is expanded by default (comment rows present with connectors).
        let mut p = pr("o/r", 1, "me", false, 100, vec![user("alice", true)]);
        p.unresolved_comments = vec![
            comment("carol", "add a test here", Some("src/foo.rs"), false),
            comment("dave", "nit: rename", None, true),
        ];
        let authored = vec![p];
        let section = build_section_authored(&authored, "me", &ToggledSet::new());

        // Root PR at the two-space base → child base is "     " (2 + 3).
        let child_base = "     ";

        let headers: Vec<(SectionId, bool, String)> = section
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::SectionHeader {
                    section,
                    expanded,
                    tree_prefix,
                    ..
                } => Some((*section, *expanded, tree_prefix.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            headers,
            vec![
                (SectionId::Reviewers, false, child_base.to_string()),
                (SectionId::Comments, true, child_base.to_string()),
            ],
        );

        // Reviewers collapsed → no reviewer rows.
        assert!(
            !section
                .rows
                .iter()
                .any(|r| matches!(r, Row::Reviewer { .. })),
            "collapsed Reviewers emits no reviewer rows",
        );

        // Open comments expanded → two comments: first `├─`, second `└─`.
        let comment_prefixes: Vec<String> = section
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Comment { tree_prefix, .. } => Some(tree_prefix.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            comment_prefixes,
            vec!["     ├─ ".to_string(), "     └─ ".to_string()],
        );
    }

    #[test]
    fn build_section_authored_default_collapse_reveals_comments_and_stacked() {
        // A root PR with reviewers + comments, plus a stacked child PR. At
        // defaults: Reviewers collapsed (no reviewer rows), Open comments and
        // Stacked PRs expanded (comment rows and the nested PR present).
        let mut a = pr_refs("o/r", 1, "main", "a");
        a.reviewers = vec![user("alice", true)];
        a.unresolved_comments = vec![comment("carol", "please fix", Some("src/x.rs"), false)];
        let b = pr_refs("o/r", 2, "a", "b");
        let authored = vec![a, b];
        let section = build_section_authored(&authored, "me", &ToggledSet::new());

        assert!(
            !section
                .rows
                .iter()
                .any(|r| matches!(r, Row::Reviewer { .. })),
            "Reviewers collapsed by default",
        );
        assert!(
            section
                .rows
                .iter()
                .any(|r| matches!(r, Row::Comment { .. })),
            "Open comments expanded by default",
        );
        // The nested child PR #2 is present (Stacked PRs expanded by default).
        assert!(
            section
                .rows
                .iter()
                .any(|r| matches!(r, Row::Pr { pr, .. } if pr.number == 2)),
            "Stacked PRs expanded by default",
        );
    }

    #[test]
    fn build_section_authored_expand_reviewers_reveals_rows() {
        let p = pr("o/r", 1, "me", false, 100, vec![user("alice", true)]);
        let authored = vec![p];
        let mut toggled = ToggledSet::new();
        set_expanded(&mut toggled, "o/r", 1, SectionId::Reviewers, true);
        let section = build_section_authored(&authored, "me", &toggled);

        assert!(
            section
                .rows
                .iter()
                .any(|r| matches!(r, Row::Reviewer { .. })),
            "expanding Reviewers reveals reviewer rows",
        );
        let expanded = section.rows.iter().any(|r| {
            matches!(
                r,
                Row::SectionHeader {
                    section: SectionId::Reviewers,
                    expanded: true,
                    ..
                }
            )
        });
        assert!(expanded, "header reflects expanded state");
    }

    #[test]
    fn build_section_authored_collapse_comments_hides_rows_keeps_header() {
        let mut p = pr("o/r", 1, "me", false, 100, vec![]);
        p.unresolved_comments = vec![comment("carol", "fix", None, false)];
        let authored = vec![p];
        let mut toggled = ToggledSet::new();
        set_expanded(&mut toggled, "o/r", 1, SectionId::Comments, false);
        let section = build_section_authored(&authored, "me", &toggled);

        assert!(
            !section
                .rows
                .iter()
                .any(|r| matches!(r, Row::Comment { .. })),
            "collapsing Open comments hides comment rows",
        );
        assert!(
            section.rows.iter().any(|r| matches!(
                r,
                Row::SectionHeader {
                    section: SectionId::Comments,
                    expanded: false,
                    ..
                }
            )),
            "the Open comments header remains",
        );
    }

    #[test]
    fn build_section_authored_emits_checks_first_collapsed_by_default() {
        // A PR with checks AND reviewers → the Checks header appears before the
        // Reviewers header, collapsed by default (no Check rows), carrying the
        // merge-readiness summary.
        let mut p = pr("o/r", 1, "me", false, 100, vec![user("alice", true)]);
        p.checks = vec![
            check("build", CheckState::Success, true),
            check("test", CheckState::Success, true),
            check("lint", CheckState::Pending, false),
        ];
        p.checks_rollup = ChecksRollup::Green;
        let authored = vec![p];
        let section = build_section_authored(&authored, "me", &ToggledSet::new());

        // Header order: Checks before Reviewers.
        let header_ids: Vec<SectionId> = section
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::SectionHeader { section, .. } => Some(*section),
                _ => None,
            })
            .collect();
        assert_eq!(header_ids, vec![SectionId::Checks, SectionId::Reviewers]);

        // Checks header: collapsed, with a Green 2/2-required summary.
        let cs = section
            .rows
            .iter()
            .find_map(|r| match r {
                Row::SectionHeader {
                    section: SectionId::Checks,
                    expanded,
                    checks: Some(cs),
                    ..
                } => Some((*expanded, *cs)),
                _ => None,
            })
            .expect("Checks header present");
        assert!(!cs.0, "Checks collapsed by default");
        assert_eq!(cs.1.rollup, ChecksRollup::Green);
        assert_eq!(cs.1.passed_required, 2);
        assert_eq!(cs.1.total_required, 2);
        assert_eq!(cs.1.ratio_text(), "2/2 required");

        // Collapsed → no Check rows.
        assert!(!section.rows.iter().any(|r| matches!(r, Row::Check { .. })));
    }

    #[test]
    fn build_section_authored_omits_checks_when_none() {
        let p = pr("o/r", 1, "me", false, 100, vec![]);
        let authored = vec![p];
        let section = build_section_authored(&authored, "me", &ToggledSet::new());
        assert!(
            !section.rows.iter().any(|r| matches!(
                r,
                Row::SectionHeader {
                    section: SectionId::Checks,
                    ..
                }
            )),
            "a PR with no checks emits no Checks header",
        );
    }

    #[test]
    fn build_section_authored_expand_checks_reveals_rows() {
        let mut p = pr("o/r", 1, "me", false, 100, vec![]);
        p.checks = vec![
            check("build", CheckState::Success, true),
            check("lint", CheckState::Failure, false),
        ];
        p.checks_rollup = ChecksRollup::Green;
        let authored = vec![p];
        let mut toggled = ToggledSet::new();
        set_expanded(&mut toggled, "o/r", 1, SectionId::Checks, true);
        set_expanded(&mut toggled, "o/r", 1, SectionId::ValidResults, true);
        let section = build_section_authored(&authored, "me", &toggled);

        let check_names: Vec<&str> = section
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Check { c, .. } => Some(c.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(check_names, vec!["lint", "build"]);
    }

    #[test]
    fn failing_checks_open_with_actionable_rows_above_collapsed_valid_results() {
        let mut p = pr("o/r", 1, "me", false, 100, vec![]);
        p.checks = vec![
            check("success", CheckState::Success, true),
            check("pending", CheckState::Pending, true),
            check("optional failure", CheckState::Failure, false),
            check("neutral", CheckState::Neutral, false),
            check("skipped", CheckState::Skipped, false),
        ];
        let authored = vec![p];
        let section = build_section_authored(&authored, "me", &ToggledSet::new());

        let shape: Vec<String> = section
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::SectionHeader {
                    section,
                    expanded,
                    tree_prefix,
                    ..
                } => Some(format!("H:{}:{expanded}:{tree_prefix}", section.label())),
                Row::Check { c, tree_prefix } => Some(format!("C:{}:{tree_prefix}", c.name)),
                _ => None,
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                "H:Checks:true:     ",
                "C:optional failure:     ├─ ",
                "C:pending:     ├─ ",
                "H:Valid Results:false:     └─ ",
            ]
        );
    }

    #[test]
    fn only_valid_checks_keep_checks_closed_and_valid_results_is_optional() {
        let mut only_valid = pr("o/r", 1, "me", false, 100, vec![]);
        only_valid.checks = vec![
            check("success", CheckState::Success, true),
            check("skipped", CheckState::Skipped, false),
        ];
        let authored = vec![only_valid];
        let collapsed = build_section_authored(&authored, "me", &ToggledSet::new());
        assert!(collapsed.rows.iter().any(|row| matches!(
            row,
            Row::SectionHeader {
                section: SectionId::Checks,
                expanded: false,
                ..
            }
        )));
        assert!(!collapsed.rows.iter().any(|row| matches!(
            row,
            Row::SectionHeader {
                section: SectionId::ValidResults,
                ..
            }
        )));

        let mut expanded = ToggledSet::new();
        set_expanded(&mut expanded, "o/r", 1, SectionId::Checks, true);
        let section = build_section_authored(&authored, "me", &expanded);
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

        let mut no_valid = pr("o/r", 2, "me", false, 100, vec![]);
        no_valid.checks = vec![
            check("error", CheckState::Error, false),
            check("pending", CheckState::Pending, true),
        ];
        let no_valid_authored = vec![no_valid];
        let section = build_section_authored(&no_valid_authored, "me", &ToggledSet::new());
        assert!(!section.rows.iter().any(|row| matches!(
            row,
            Row::SectionHeader {
                section: SectionId::ValidResults,
                ..
            }
        )));
    }

    #[test]
    fn explicit_checks_choice_survives_failure_state_changes() {
        let mut p = pr("o/r", 1, "me", false, 100, vec![]);
        p.checks = vec![check("build", CheckState::Success, true)];
        let mut choices = ToggledSet::new();
        set_expanded(&mut choices, "o/r", 1, SectionId::Checks, true);
        let healthy = vec![p.clone()];
        assert!(matches!(
            checks_header_expanded(&healthy, &choices),
            Some(true)
        ));

        p.checks[0].state = CheckState::Failure;
        let failing = vec![p.clone()];
        assert!(matches!(
            checks_header_expanded(&failing, &choices),
            Some(true)
        ));

        set_expanded(&mut choices, "o/r", 1, SectionId::Checks, false);
        p.checks[0].state = CheckState::Success;
        let healthy_again = vec![p];
        assert!(matches!(
            checks_header_expanded(&healthy_again, &choices),
            Some(false)
        ));
    }

    fn checks_header_expanded(authored: &[Pr], choices: &ToggledSet) -> Option<bool> {
        build_section_authored(authored, "me", choices)
            .rows
            .into_iter()
            .find_map(|row| match row {
                Row::SectionHeader {
                    section: SectionId::Checks,
                    expanded,
                    ..
                } => Some(expanded),
                _ => None,
            })
    }

    #[test]
    fn checks_summary_ratio_text_variants() {
        let pending = ChecksSummary {
            rollup: ChecksRollup::Pending,
            passed_required: 2,
            total_required: 4,
        };
        assert_eq!(pending.ratio_text(), "2/4 required");
        assert_eq!(checks_summary_text(&pending), "2/4 required pending");

        let red = ChecksSummary {
            rollup: ChecksRollup::Red,
            passed_required: 3,
            total_required: 4,
        };
        assert_eq!(checks_summary_text(&red), "3/4 required failing");

        let zero = ChecksSummary {
            rollup: ChecksRollup::Green,
            passed_required: 0,
            total_required: 0,
        };
        assert_eq!(zero.ratio_text(), "no required checks");

        let unknown = ChecksSummary {
            rollup: ChecksRollup::Unknown,
            passed_required: 0,
            total_required: 2,
        };
        assert_eq!(unknown.ratio_text(), "unknown");
    }

    #[test]
    fn console_render_checks_header_and_rows() {
        let mut p = pr("o/r", 1, "me", false, 100, vec![]);
        p.checks = vec![
            check("build", CheckState::Success, true),
            check("optional", CheckState::Failure, false),
        ];
        p.checks_rollup = ChecksRollup::Green;
        let authored = vec![p];
        // Expand so the Check rows render too.
        let mut toggled = ToggledSet::new();
        set_expanded(&mut toggled, "o/r", 1, SectionId::Checks, true);
        set_expanded(&mut toggled, "o/r", 1, SectionId::ValidResults, true);
        let section = build_section_authored(&authored, "me", &toggled);
        let mut out: Vec<u8> = Vec::new();
        render_section(&section, &mut out, false, 120).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("Checks"), "{s}");
        assert!(s.contains("[1/1 required]"), "{s}");
        assert!(s.contains("build"), "{s}");
        assert!(s.contains("optional (not required)"), "{s}");
    }

    #[test]
    fn console_render_marks_only_outdated_comments() {
        let current = comment("alice", "still applies", None, false);
        let outdated = comment("bob", "the diff moved", None, true);
        let mut out = Vec::new();

        render_comment_line(&current, "", &mut out, false).unwrap();
        render_comment_line(&outdated, "", &mut out, false).unwrap();

        let lines: Vec<&str> = std::str::from_utf8(&out).unwrap().lines().collect();
        assert_eq!(
            lines,
            ["@alice still applies", "@bob the diff moved [outdated]"]
        );
    }

    #[test]
    fn reviewer_summary_dedups_and_orders() {
        let reviewers = vec![
            user("alice", true),                              // still requested
            reviewed("bob", ReviewState::Approved),           // approved
            reviewed("carol", ReviewState::ChangesRequested), // rejection
            reviewed("dave", ReviewState::Approved),          // duplicate approved
        ];
        let summary = reviewer_summary(&reviewers);
        assert_eq!(
            summary,
            vec![
                ReviewerSummaryToken::Requested,
                ReviewerSummaryToken::Approved,
                ReviewerSummaryToken::ChangesRequested,
            ],
        );
        // The rejection signal is present.
        assert!(summary.contains(&ReviewerSummaryToken::ChangesRequested));
    }

    #[test]
    fn filtered_authored_matches_pr_text_case_insensitively_and_prunes_siblings() {
        let mut matching = pr("o/keep", 1, "me", false, 300, vec![]);
        matching.title = "Repair Unicode Search".into();
        let mut sibling = pr("o/keep", 2, "me", false, 200, vec![]);
        sibling.title = "unrelated work".into();
        let other_repo = pr("o/drop", 3, "me", false, 100, vec![]);

        let authored = vec![matching, sibling, other_repo];
        let section =
            build_section_authored_filtered(&authored, "me", "UNICODE", &ToggledSet::new());

        assert_eq!(section.count, 1);
        assert_eq!(section.empty_message, Some("(no matches)"));
        assert!(
            section
                .rows
                .iter()
                .any(|row| matches!(row, Row::RepoHeader(repo) if repo == "o/keep"))
        );
        assert!(
            !section
                .rows
                .iter()
                .any(|row| matches!(row, Row::RepoHeader(repo) if repo == "o/drop"))
        );
        let numbers: Vec<u64> = section
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Pr { pr, .. } => Some(pr.number),
                _ => None,
            })
            .collect();
        assert_eq!(numbers, vec![1]);
    }

    #[test]
    fn filtered_authored_matches_head_ref_and_retains_stacked_ancestor() {
        let mut root = pr_refs("o/r", 1, "main", "stack-base");
        root.title = "root".to_string();
        let mut child = pr_refs("o/r", 2, "stack-base", "Feature/Search-Needle");
        child.title = "child".to_string();
        let authored = vec![root, child];

        let section =
            build_section_authored_filtered(&authored, "me", "search-needle", &ToggledSet::new());
        let numbers: Vec<u64> = section
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Pr {
                    pr,
                    show_head_ref: true,
                    ..
                } => Some(pr.number),
                _ => None,
            })
            .collect();
        assert_eq!(section.count, 2);
        assert_eq!(numbers, vec![1, 2]);
    }

    #[test]
    fn filtered_authored_descendant_matches_retain_ancestors_and_ignore_normal_collapse() {
        let mut authored_pr = pr(
            "o/r",
            1,
            "me",
            false,
            100,
            vec![user("NeedleReviewer", true)],
        );
        authored_pr.checks = vec![check("NeedleCheck", CheckState::Pending, true)];
        authored_pr.unresolved_comments = vec![comment(
            "alice",
            "NeedleComment",
            Some("src/search.rs"),
            false,
        )];
        // Checks and Reviewers are normally collapsed, but search must expose
        // their matching descendants without consulting persistent toggles.
        let normal_authored = vec![authored_pr.clone()];
        let normal = build_section_authored(&normal_authored, "me", &ToggledSet::new());
        assert!(
            !normal
                .rows
                .iter()
                .any(|row| matches!(row, Row::Check { .. }))
        );
        assert!(
            !normal
                .rows
                .iter()
                .any(|row| matches!(row, Row::Reviewer { .. }))
        );

        for (query, expected_section) in [
            ("needlereviewer", SectionId::Reviewers),
            ("needlecheck", SectionId::Checks),
            ("needlecomment", SectionId::Comments),
        ] {
            let section = build_section_authored_filtered(
                std::slice::from_ref(&authored_pr),
                "me",
                query,
                &ToggledSet::new(),
            );
            assert_eq!(section.count, 1);
            assert!(matches!(section.rows[0], Row::RepoHeader(_)));
            assert!(matches!(section.rows[1], Row::Pr { .. }));
            assert!(matches!(
                section.rows[2],
                Row::SectionHeader {
                    section,
                    expanded: true,
                    ..
                } if section == expected_section
            ));
            assert!(section.rows[3].is_selectable());
        }
    }

    #[test]
    fn filtered_authored_nested_match_recomputes_connectors_and_count() {
        let mut root = pr("o/r", 1, "me", false, 300, vec![]);
        root.head_ref = "stack-base".into();
        let mut unmatched_child = pr("o/r", 2, "me", false, 200, vec![]);
        unmatched_child.base_ref = "stack-base".into();
        unmatched_child.title = "skip me".into();
        let mut matching_child = pr("o/r", 3, "me", false, 100, vec![]);
        matching_child.base_ref = "stack-base".into();
        matching_child.title = "deep needle".into();

        let authored = vec![root, unmatched_child, matching_child];
        let section =
            build_section_authored_filtered(&authored, "me", "needle", &ToggledSet::new());
        assert_eq!(section.count, 2, "matching PR plus its PR ancestor");
        let prs: Vec<(u64, String)> = section
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Pr {
                    pr, tree_prefix, ..
                } => Some((pr.number, tree_prefix.clone().unwrap())),
                _ => None,
            })
            .collect();
        assert_eq!(prs, vec![(1, "  └─ ".into()), (3, "     └─ ".into())]);
        assert!(section.rows.iter().any(|row| matches!(
            row,
            Row::SectionHeader {
                section: SectionId::Stacked,
                expanded: true,
                ..
            }
        )));
    }

    #[test]
    fn filtered_valid_check_retains_checks_and_valid_results_ancestors() {
        let mut p = pr("o/r", 1, "me", false, 100, vec![]);
        p.checks = vec![
            check("needle success", CheckState::Success, true),
            check("unrelated failure", CheckState::Failure, false),
        ];
        let authored = vec![p];
        let section =
            build_section_authored_filtered(&authored, "me", "needle", &ToggledSet::new());
        let shape: Vec<String> = section
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::SectionHeader {
                    section, expanded, ..
                } => Some(format!("{}:{expanded}", section.label())),
                Row::Check { c, .. } => Some(c.name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            shape,
            vec!["Checks:true", "Valid Results:true", "needle success"]
        );
    }

    #[test]
    fn filtered_valid_results_label_retains_checks_without_exposing_checks() {
        let mut p = pr("o/r", 1, "me", false, 100, vec![]);
        p.checks = vec![check("build", CheckState::Success, true)];
        let authored = vec![p];
        let section =
            build_section_authored_filtered(&authored, "me", "valid results", &ToggledSet::new());
        let headers: Vec<(SectionId, bool)> = section
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::SectionHeader {
                    section, expanded, ..
                } => Some((*section, *expanded)),
                _ => None,
            })
            .collect();
        assert_eq!(
            headers,
            vec![(SectionId::Checks, true), (SectionId::ValidResults, false),]
        );
        assert!(
            !section
                .rows
                .iter()
                .any(|row| matches!(row, Row::Check { .. }))
        );
    }

    #[test]
    fn filtered_authored_uses_temporary_collapse_without_changing_match_count() {
        let authored = vec![pr("o/r", 1, "me", false, 100, vec![user("needle", true)])];
        let mut search_collapsed = ToggledSet::new();
        search_collapsed.insert(("o/r".into(), 1, SectionId::Reviewers), false);
        let section = build_section_authored_filtered(&authored, "me", "needle", &search_collapsed);

        assert_eq!(section.count, 1);
        assert!(section.rows.iter().any(|row| matches!(
            row,
            Row::SectionHeader {
                section: SectionId::Reviewers,
                expanded: false,
                ..
            }
        )));
        assert!(
            !section
                .rows
                .iter()
                .any(|row| matches!(row, Row::Reviewer { .. }))
        );
    }
}
