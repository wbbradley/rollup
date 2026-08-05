use std::{
    collections::{HashMap, HashSet},
    process::Command,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, de::DeserializeOwned};

use crate::{
    config,
    model::{
        self, CheckState, CheckStatus, ChecksRollup, Pr, PrComment, PrCommentKind, ReleaseInfo,
        RepoReleaseInfo, ReviewState, ReviewerKind, ReviewerStatus, TagInfo,
    },
};

const QUERY: &str = r#"
query {
  viewer { login }
  authored: search(query: "is:pr is:open author:@me archived:false", type: ISSUE, first: 100) {
    nodes { ...AuthoredPrFields }
  }
  reviewing: search(query: "is:pr is:open review-requested:@me archived:false", type: ISSUE, first: 100) {
    nodes { ...PrFields }
  }
}

fragment PrFields on PullRequest {
  number
  title
  url
  isDraft
  updatedAt
  mergedAt
  baseRefName
  headRefName
  repository { nameWithOwner }
  author { login }
  reviewRequests(first: 20) {
    nodes {
      requestedReviewer {
        __typename
        ... on User { login }
        ... on Team { name }
      }
    }
  }
  latestReviews(first: 20) {
    nodes { author { login } state }
  }
}

fragment AuthoredPrFields on PullRequest {
  ...PrFields
  mergeable
  reviews(first: 50, states: COMMENTED) {
    nodes { author { __typename login } bodyText url }
  }
  commits(last: 1) {
    nodes {
      commit {
        statusCheckRollup {
          state
          contexts(first: 100) {
            nodes {
              __typename
              ... on CheckRun { name status conclusion startedAt detailsUrl }
              ... on StatusContext { context state targetUrl }
            }
          }
        }
      }
    }
  }
  reviewThreads(first: 50) {
    nodes {
      id
      isResolved
      isOutdated
      comments(first: 1) {
        nodes { author { login } bodyText url path }
      }
    }
  }
}
"#;

const MERGED_QUERY: &str = r#"
query($q: String!) {
  merged: search(query: $q, type: ISSUE, first: 50) {
    nodes { ...PrFields }
  }
}

fragment PrFields on PullRequest {
  number
  title
  url
  isDraft
  updatedAt
  mergedAt
  baseRefName
  headRefName
  repository { nameWithOwner }
  author { login }
  reviewRequests(first: 20) {
    nodes {
      requestedReviewer {
        __typename
        ... on User { login }
        ... on Team { name }
      }
    }
  }
  latestReviews(first: 20) {
    nodes { author { login } state }
  }
}
"#;

/// Cap on `author:` qualifiers per merged-PR search. GitHub's search API has
/// an undocumented limit on operators/qualifiers; 10 is well under any known
/// ceiling and keeps the query string short.
// TODO: paginate or batch if we need unbounded author coverage.
const MERGED_AUTHOR_CAP: usize = 10;

#[derive(Debug)]
pub struct Data {
    pub viewer: String,
    pub authored: Vec<Pr>,
    pub reviewing: Vec<Pr>,
    /// Already sorted by `merged_at` desc and capped to the recent N.
    pub merged: Vec<Pr>,
    /// One entry per configured repo, in config order. Empty if the user has
    /// no `~/.config/rollup/config.yaml` or the file parsed with no repos.
    pub releases: Vec<RepoReleaseInfo>,
    /// If the config file failed to load/parse, the error message is surfaced
    /// here so the UI can report it without crashing the app.
    pub config_error: Option<String>,
    /// Non-fatal fetch-time warnings (e.g. SAML-blocked orgs). Deduped.
    pub warnings: Vec<String>,
}

/// A GraphQL response envelope. Both fields are optional so a partial-success
/// payload (accessible `data` plus a top-level `errors` array) and an
/// errors-only payload both still deserialize.
#[derive(Deserialize)]
struct GraphQlEnvelope<T> {
    data: Option<T>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

/// Parse a finished `gh api graphql` invocation into its `data` payload plus any
/// non-fatal warning messages from the top-level `errors` array.
///
/// GitHub returns HTTP 200 with a partial `data` object on a SAML block, and
/// `gh` exits non-zero whenever the response carries any `errors` — but the full
/// partial JSON is still on stdout. So warnings are surfaced regardless of exit
/// status, and the only fatal conditions are unparseable stdout or an absent
/// `data`.
fn parse_graphql<T: DeserializeOwned>(
    output: &std::process::Output,
    label: &str,
) -> Result<(T, Vec<String>)> {
    let envelope: GraphQlEnvelope<T> = serde_json::from_slice(&output.stdout).map_err(|e| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if !output.status.success() && !stderr.is_empty() {
            anyhow!("gh api graphql ({label}) failed: {stderr}")
        } else {
            anyhow!("gh api graphql ({label}): parsing response JSON: {e}")
        }
    })?;
    let warnings: Vec<String> = envelope
        .errors
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.message)
        .collect();
    match envelope.data {
        Some(data) => Ok((data, warnings)),
        None => {
            // errors-only / no `data` → genuinely fatal.
            let detail = if warnings.is_empty() {
                String::from_utf8_lossy(&output.stderr).trim().to_string()
            } else {
                warnings.join("; ")
            };
            Err(anyhow!("gh api graphql ({label}) failed: {detail}"))
        }
    }
}

pub fn remove_user_reviewer(owner: &str, repo: &str, pr_number: u64, login: &str) -> Result<()> {
    remove_reviewer_impl(owner, repo, pr_number, "reviewers[]", login)
}

pub fn remove_team_reviewer(owner: &str, repo: &str, pr_number: u64, team: &str) -> Result<()> {
    remove_reviewer_impl(owner, repo, pr_number, "team_reviewers[]", team)
}

const RESOLVE_REVIEW_THREAD_MUTATION: &str = r#"
mutation($threadId: ID!) {
  resolveReviewThread(input: {threadId: $threadId}) {
    thread { id isResolved }
  }
}
"#;

pub fn resolve_review_thread(thread_id: &str) -> Result<()> {
    let field = format!("threadId={thread_id}");
    let output = Command::new("gh")
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={RESOLVE_REVIEW_THREAD_MUTATION}"),
            "-f",
            &field,
        ])
        .output()
        .context("failed to invoke gh")?;
    let (data, warnings) = parse_graphql::<ResolveReviewThreadData>(&output, "resolve-thread")?;
    confirm_resolved_review_thread(thread_id, data, warnings)
}

fn confirm_resolved_review_thread(
    thread_id: &str,
    data: ResolveReviewThreadData,
    warnings: Vec<String>,
) -> Result<()> {
    let resolved = data
        .resolve_review_thread
        .and_then(|payload| payload.thread)
        .is_some_and(|thread| thread.id == thread_id && thread.is_resolved);
    if resolved {
        Ok(())
    } else {
        let detail = if warnings.is_empty() {
            "GitHub did not confirm the thread was resolved".to_string()
        } else {
            warnings.join("; ")
        };
        Err(anyhow!("gh resolve review thread failed: {detail}"))
    }
}

#[derive(Deserialize)]
struct ResolveReviewThreadData {
    #[serde(rename = "resolveReviewThread")]
    resolve_review_thread: Option<ResolveReviewThreadPayload>,
}

#[derive(Deserialize)]
struct ResolveReviewThreadPayload {
    thread: Option<ResolvedReviewThread>,
}

#[derive(Deserialize)]
struct ResolvedReviewThread {
    id: String,
    #[serde(rename = "isResolved")]
    is_resolved: bool,
}

fn remove_reviewer_impl(
    owner: &str,
    repo: &str,
    pr_number: u64,
    field: &str,
    value: &str,
) -> Result<()> {
    let endpoint = format!("repos/{owner}/{repo}/pulls/{pr_number}/requested_reviewers");
    let body = format!("{field}={value}");
    let output = Command::new("gh")
        .args(["api", "-X", "DELETE", &endpoint, "-f", &body])
        .output()
        .context("failed to invoke gh")?;
    if !output.status.success() {
        return Err(anyhow!(
            "gh remove reviewer failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

pub fn fetch() -> Result<Data> {
    let (config, config_error) = match config::load() {
        Ok(c) => (c, None),
        Err(e) => (config::Config::default(), Some(format!("{e:#}"))),
    };
    let mut warnings: Vec<String> = Vec::new();
    let (viewer, mut authored, reviewing, w) = fetch_open()?;
    warnings.extend(w);
    // Second round-trip: learn each authored PR's branch-protection-required
    // check flags (the bulk `search` query can't compute `isRequired` per PR)
    // and finalize its checks rollup. A hard failure is non-fatal — surface a
    // warning and mark the affected PRs Unknown rather than claim a false green.
    match fetch_required_checks(&authored) {
        Ok((authoritative, w)) => {
            warnings.extend(w);
            finalize_checks(&mut authored, &authoritative);
        }
        Err(e) => {
            warnings.push(format!("required checks: {e:#}"));
            for pr in &mut authored {
                if !pr.checks.is_empty() {
                    pr.checks_rollup = ChecksRollup::Unknown;
                }
            }
        }
    }
    // The merged-PR fetch is scoped to the authors visible in every view: the
    // viewer plus the authors of the PRs awaiting the viewer's review. The
    // render layer filters this set per view.
    let authors = model::authors_for_me(&viewer, &reviewing);
    let merged = if authors.is_empty() {
        Vec::new()
    } else {
        let (merged, w) = fetch_merged(&authors, MERGED_AUTHOR_CAP)?;
        warnings.extend(w);
        merged
    };
    let releases = if config.repos.is_empty() {
        Vec::new()
    } else {
        let (releases, w) = fetch_releases(&config.repos)?;
        warnings.extend(w);
        releases
    };
    // Dedup identical messages (one SAML line, not N) preserving first-seen order.
    let mut seen = std::collections::HashSet::new();
    warnings.retain(|w| seen.insert(w.clone()));
    Ok(Data {
        viewer,
        authored,
        reviewing,
        merged,
        releases,
        config_error,
        warnings,
    })
}

#[allow(clippy::type_complexity)]
fn fetch_open() -> Result<(String, Vec<Pr>, Vec<Pr>, Vec<String>)> {
    let output = Command::new("gh")
        .args(["api", "graphql", "-f"])
        .arg(format!("query={QUERY}"))
        .output()
        .context("failed to invoke gh; is it installed and on PATH?")?;
    let (data, warnings) = parse_graphql::<OpenDataResp>(&output, "open")?;
    let authored = data
        .authored
        .nodes
        .into_iter()
        .flatten()
        .filter_map(node_to_pr)
        .collect();
    let reviewing = data
        .reviewing
        .nodes
        .into_iter()
        .flatten()
        .filter_map(node_to_pr)
        .collect();
    Ok((data.viewer.login, authored, reviewing, warnings))
}

fn fetch_merged(authors: &[String], cap: usize) -> Result<(Vec<Pr>, Vec<String>)> {
    let clauses: Vec<String> = authors
        .iter()
        .take(MERGED_AUTHOR_CAP)
        .map(|a| format!("author:{a}"))
        .collect();
    if clauses.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let q = format!(
        "is:pr is:merged {} archived:false sort:updated-desc",
        clauses.join(" ")
    );

    let output = Command::new("gh")
        .args(["api", "graphql", "-f"])
        .arg(format!("query={MERGED_QUERY}"))
        .args(["-f", &format!("q={q}")])
        .output()
        .context("failed to invoke gh; is it installed and on PATH?")?;
    let (data, warnings) = parse_graphql::<MergedDataResp>(&output, "merged")?;
    let all: Vec<Pr> = data
        .merged
        .nodes
        .into_iter()
        .flatten()
        .filter_map(node_to_pr)
        .filter(|p| p.merged_at.is_some())
        .collect();
    Ok((
        model::recent_merged(&all, cap)
            .into_iter()
            .cloned()
            .collect(),
        warnings,
    ))
}

fn node_to_pr(node: PrNode) -> Option<Pr> {
    // search(type: ISSUE) returns Issue | PullRequest. With `is:pr` every match
    // hits the PullRequest fragment, but skip any stragglers defensively.
    let number = node.number?;
    let repo = node.repository?.name_with_owner;

    let mut reviewers: Vec<ReviewerStatus> = Vec::new();

    // Pass 1: fold in everyone who has actually submitted a review.
    // `requested` stays false here; pass 2 promotes anyone GitHub is still
    // asking to review (the "re-requested" case).
    if let Some(latest) = node.latest_reviews {
        for review in latest.nodes {
            let Some(author) = review.author else {
                continue;
            };
            let state = match review.state.as_str() {
                "APPROVED" => ReviewState::Approved,
                "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
                "COMMENTED" => ReviewState::Commented,
                "DISMISSED" => ReviewState::Dismissed,
                // PENDING here means the reviewer has a draft review saved but
                // hasn't submitted it — not observable to others, so ignore.
                _ => continue,
            };
            reviewers.push(ReviewerStatus {
                login: author.login,
                kind: ReviewerKind::User,
                state,
                requested: false,
            });
        }
    }

    // Pass 2: mark/insert everyone currently in `reviewRequests`. These are
    // the only reviewers the DELETE requested_reviewers endpoint can remove.
    if let Some(requests) = node.review_requests {
        for req in requests.nodes {
            let Some(rr) = req.requested_reviewer else {
                continue;
            };
            match rr {
                RequestedReviewer::User { login } => {
                    if let Some(existing) = reviewers
                        .iter_mut()
                        .find(|r| r.kind == ReviewerKind::User && r.login == login)
                    {
                        existing.requested = true;
                    } else {
                        reviewers.push(ReviewerStatus {
                            login,
                            kind: ReviewerKind::User,
                            state: ReviewState::NoReview,
                            requested: true,
                        });
                    }
                }
                RequestedReviewer::Team { name } => {
                    let login = format!("@{name}");
                    if let Some(existing) = reviewers
                        .iter_mut()
                        .find(|r| r.kind == ReviewerKind::Team && r.login == login)
                    {
                        existing.requested = true;
                    } else {
                        reviewers.push(ReviewerStatus {
                            login,
                            kind: ReviewerKind::Team,
                            state: ReviewState::NoReview,
                            requested: true,
                        });
                    }
                }
                RequestedReviewer::Other => {}
            }
        }
    }

    let updated_at = node
        .updated_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch"));

    let merged_at = node
        .merged_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));

    // Only the `authored:` query fetches review bodies and `reviewThreads`;
    // reviewing/merged nodes leave both `None`, so their `unresolved_comments`
    // come out empty. GitHub review-level comments do not belong to a thread and
    // therefore have no resolve action. Surface non-empty human COMMENTED review
    // bodies as open comments, then the first comment of each unresolved inline
    // thread (including outdated ones).
    let mut unresolved_comments: Vec<PrComment> = Vec::new();
    if let Some(reviews) = node.reviews {
        for review in reviews.nodes {
            let Some(author) = review.author else {
                continue;
            };
            if author.kind.as_deref() == Some("Bot") {
                continue;
            }
            let body = review.body_text.unwrap_or_default();
            if body.trim().is_empty() {
                continue;
            }
            let url = review.url.unwrap_or_default();
            if url.is_empty() {
                continue;
            }
            if !reviewers.iter().any(|reviewer| {
                reviewer.kind == ReviewerKind::User && reviewer.login == author.login
            }) {
                reviewers.push(ReviewerStatus {
                    login: author.login.clone(),
                    kind: ReviewerKind::User,
                    state: ReviewState::Commented,
                    requested: false,
                });
            }
            unresolved_comments.push(PrComment {
                kind: PrCommentKind::ReviewSummary,
                thread_id: None,
                author: author.login,
                body: normalized_comment_body(&body),
                url,
                path: None,
                is_outdated: false,
            });
        }
    }
    if let Some(threads) = node.review_threads {
        for thread in threads.nodes {
            if thread.is_resolved {
                continue;
            }
            let Some(comment) = thread.comments.and_then(|c| c.nodes.into_iter().next()) else {
                continue;
            };
            let url = comment.url.unwrap_or_default();
            if url.is_empty() {
                continue;
            }
            let path = comment.path.filter(|p| !p.is_empty());
            unresolved_comments.push(PrComment {
                kind: PrCommentKind::Thread,
                thread_id: thread.id,
                author: comment
                    .author
                    .map(|a| a.login)
                    .unwrap_or_else(|| "ghost".into()),
                body: normalized_comment_body(&comment.body_text.unwrap_or_default()),
                url,
                path,
                is_outdated: thread.is_outdated,
            });
        }
    }

    reviewers.sort_by_key(|r| r.login.to_lowercase());

    // Checks: only the authored fragment fetches `commits`/`mergeable`, so
    // reviewing/merged nodes yield no checks (`rollup_present == false`) and a
    // provisional Unknown rollup that's never rendered for them. For authored
    // PRs these are a provisional first-100-context preview with `required` still
    // false — `finalize_checks` replaces them with the second call's complete,
    // paginated set and recomputes the rollup. `mergeable == UNKNOWN` (or a
    // missing field) means GitHub is still computing → Unknown.
    let (checks, rollup_present) = checks_from_commits(&node.commits);
    let mergeable_unknown = !matches!(
        node.mergeable.as_deref(),
        Some("MERGEABLE") | Some("CONFLICTING")
    );
    let checks_rollup = model::compute_checks_rollup(&checks, mergeable_unknown, rollup_present);

    Some(Pr {
        number,
        title: node.title.unwrap_or_default(),
        url: node.url.unwrap_or_default(),
        is_draft: node.is_draft.unwrap_or(false),
        repo,
        base_ref: node.base_ref_name.unwrap_or_default(),
        head_ref: node.head_ref_name.unwrap_or_default(),
        author: node
            .author
            .map(|a| a.login)
            .unwrap_or_else(|| "ghost".into()),
        reviewers,
        updated_at,
        merged_at,
        unresolved_comments,
        checks,
        checks_rollup,
    })
}

fn normalized_comment_body(body: &str) -> String {
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Deserialize)]
struct OpenDataResp {
    viewer: Viewer,
    authored: SearchResp,
    reviewing: SearchResp,
}

#[derive(Deserialize)]
struct MergedDataResp {
    merged: SearchResp,
}

#[derive(Deserialize)]
struct Viewer {
    login: String,
}

#[derive(Deserialize)]
struct SearchResp {
    /// SAML-blocked search hits arrive as `null` inside `nodes`, so each element
    /// is optional; callers `.flatten()` before mapping through `node_to_pr`.
    nodes: Vec<Option<PrNode>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PrNode {
    number: Option<u64>,
    title: Option<String>,
    url: Option<String>,
    #[serde(rename = "isDraft")]
    is_draft: Option<bool>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
    #[serde(rename = "mergedAt")]
    merged_at: Option<String>,
    #[serde(rename = "baseRefName")]
    base_ref_name: Option<String>,
    #[serde(rename = "headRefName")]
    head_ref_name: Option<String>,
    repository: Option<RepoNode>,
    author: Option<AuthorNode>,
    #[serde(rename = "reviewRequests")]
    review_requests: Option<ReviewRequests>,
    #[serde(rename = "latestReviews")]
    latest_reviews: Option<LatestReviews>,
    reviews: Option<Reviews>,
    #[serde(rename = "reviewThreads")]
    review_threads: Option<ReviewThreads>,
    /// `MergeableState`: MERGEABLE | CONFLICTING | UNKNOWN. Only fetched by the
    /// authored fragment. UNKNOWN/absent means GitHub is still computing.
    mergeable: Option<String>,
    /// Last commit's status-check rollup. Only fetched by the authored fragment.
    commits: Option<CommitsConnection>,
}

#[derive(Deserialize)]
struct ReviewThreads {
    nodes: Vec<ReviewThreadNode>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ReviewThreadNode {
    id: Option<String>,
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    #[serde(rename = "isOutdated")]
    is_outdated: bool,
    comments: Option<ThreadComments>,
}

#[derive(Deserialize)]
struct ThreadComments {
    nodes: Vec<ThreadCommentNode>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ThreadCommentNode {
    author: Option<AuthorNode>,
    #[serde(rename = "bodyText")]
    body_text: Option<String>,
    url: Option<String>,
    path: Option<String>,
}

#[derive(Deserialize)]
struct RepoNode {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Deserialize)]
struct AuthorNode {
    #[serde(rename = "__typename")]
    kind: Option<String>,
    login: String,
}

#[derive(Deserialize)]
struct Reviews {
    nodes: Vec<ReviewNode>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ReviewNode {
    author: Option<AuthorNode>,
    #[serde(rename = "bodyText")]
    body_text: Option<String>,
    url: Option<String>,
}

#[derive(Deserialize)]
struct ReviewRequests {
    nodes: Vec<ReviewRequestNode>,
}

#[derive(Deserialize)]
struct ReviewRequestNode {
    #[serde(rename = "requestedReviewer")]
    requested_reviewer: Option<RequestedReviewer>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum RequestedReviewer {
    User {
        login: String,
    },
    Team {
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct LatestReviews {
    nodes: Vec<LatestReviewNode>,
}

#[derive(Deserialize)]
struct LatestReviewNode {
    author: Option<AuthorNode>,
    state: String,
}

// --- checks ---

#[derive(Deserialize, Default)]
#[serde(default)]
struct CommitsConnection {
    /// `null` node tolerance for SAML-partial payloads.
    nodes: Vec<Option<CommitNode>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CommitNode {
    commit: Option<CommitInner>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CommitInner {
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<StatusCheckRollup>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StatusCheckRollup {
    // `state` is fetched for parity/debuggability but the rollup is derived from
    // the required contexts + `mergeable`, so serde just drops it here.
    contexts: Option<CheckContexts>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CheckContexts {
    nodes: Vec<Option<CheckContextNode>>,
    /// Only fetched by the paginated required-checks call. The bulk authored
    /// query omits it, so serde defaults it to a no-more-pages `PageInfo`.
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

/// One status-check context. Shared by both the authored fetch (which pulls
/// `name`/`status`/`conclusion`/`detailsUrl` or `context`/`state`/`targetUrl`)
/// and the required-checks fetch (which pulls `name`/`context` + `isRequired`);
/// every field is optional so either payload deserializes.
#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum CheckContextNode {
    CheckRun {
        name: Option<String>,
        status: Option<String>,
        conclusion: Option<String>,
        #[serde(rename = "startedAt")]
        started_at: Option<String>,
        #[serde(rename = "detailsUrl")]
        details_url: Option<String>,
        #[serde(rename = "isRequired")]
        is_required: Option<bool>,
    },
    StatusContext {
        context: Option<String>,
        state: Option<String>,
        #[serde(rename = "targetUrl")]
        target_url: Option<String>,
        #[serde(rename = "isRequired")]
        is_required: Option<bool>,
    },
    #[serde(other)]
    Other,
}

/// The last commit's status-check rollup contexts from `commits`. Returns the
/// normalized checks (with `required` defaulted to false — the second call fills
/// it in) plus whether a rollup was present at all (false → no checks, so the
/// Checks section is omitted).
fn checks_from_commits(commits: &Option<CommitsConnection>) -> (Vec<CheckStatus>, bool) {
    let rollup = commits
        .as_ref()
        .and_then(|c| c.nodes.iter().flatten().next())
        .and_then(|cn| cn.commit.as_ref())
        .and_then(|ci| ci.status_check_rollup.as_ref());
    let Some(rollup) = rollup else {
        return (Vec::new(), false);
    };
    let checks = match rollup.contexts.as_ref() {
        Some(contexts) => checks_from_context_nodes(contexts.nodes.iter().flatten()),
        None => Vec::new(),
    };
    (checks, true)
}

/// Normalize a rollup's status-check context nodes into [`CheckStatus`] values.
///
/// Shared by the bulk authored query (which leaves `isRequired` absent, so
/// `required` defaults to false — the second call fills it in) and the paginated
/// required-checks call (which supplies both state and `isRequired`, making its
/// result authoritative). A retried/re-run workflow leaves the superseded
/// CheckRun in the rollup alongside its replacement; keep one run per check name,
/// selected by its start time, so an older failure/cancellation cannot mask a
/// newer result. StatusContext nodes are not CheckRun instances and GitHub
/// already rolls them up by context, so they remain untouched.
fn checks_from_context_nodes<'a>(
    nodes: impl Iterator<Item = &'a CheckContextNode>,
) -> Vec<CheckStatus> {
    let mut checks = Vec::new();
    let mut check_run_indices: HashMap<String, (usize, Option<String>)> = HashMap::new();
    for ctx in nodes {
        match ctx {
            CheckContextNode::CheckRun {
                name,
                status,
                conclusion,
                started_at,
                details_url,
                is_required,
            } => {
                let Some(name) = name.clone() else { continue };
                let check = CheckStatus {
                    name: name.clone(),
                    state: check_run_state(status.as_deref(), conclusion.as_deref()),
                    url: details_url.clone().filter(|u| !u.is_empty()),
                    required: is_required.unwrap_or(false),
                };
                if let Some((index, previous_started_at)) = check_run_indices.get_mut(&name) {
                    if check_run_is_newer(started_at.as_deref(), previous_started_at.as_deref()) {
                        checks[*index] = check;
                        *previous_started_at = started_at.clone();
                    }
                } else {
                    check_run_indices.insert(name, (checks.len(), started_at.clone()));
                    checks.push(check);
                }
            }
            CheckContextNode::StatusContext {
                context,
                state,
                target_url,
                is_required,
            } => {
                let Some(name) = context.clone() else {
                    continue;
                };
                checks.push(CheckStatus {
                    name,
                    state: status_context_state(state.as_deref()),
                    url: target_url.clone().filter(|u| !u.is_empty()),
                    required: is_required.unwrap_or(false),
                });
            }
            CheckContextNode::Other => {}
        }
    }
    checks
}

/// Whether a duplicate check run should replace the instance currently kept.
/// `startedAt` is an RFC 3339 timestamp, whose normalized GitHub representation
/// sorts chronologically as text. Missing timestamps only arise in malformed or
/// partial payloads; prefer a timestamped run, then the later payload entry as a
/// deterministic fallback when both are missing.
fn check_run_is_newer(candidate: Option<&str>, current: Option<&str>) -> bool {
    match (candidate, current) {
        (Some(candidate), Some(current)) => candidate >= current,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => true,
    }
}

/// Map a `CheckRun`'s status/conclusion to a [`CheckState`]. A run that hasn't
/// completed is Pending. A completed run maps its conclusion: a clean pass or
/// skip records as such, a genuine `FAILURE` is a failure, and every other
/// terminal conclusion — cancellation, timeout, startup failure, action
/// required, or anything we don't recognize — is an Error. A completed run that
/// simply hasn't reported a conclusion yet stays Pending. This guarantees a
/// finished check that neither passed nor was skipped always surfaces in the
/// failing set rather than being mistaken for still-running.
fn check_run_state(status: Option<&str>, conclusion: Option<&str>) -> CheckState {
    match status {
        // COMPLETED (or an absent status) → decide from the conclusion.
        Some("COMPLETED") | None => match conclusion {
            Some("SUCCESS") => CheckState::Success,
            Some("SKIPPED") => CheckState::Skipped,
            Some("NEUTRAL") | Some("STALE") => CheckState::Neutral,
            Some("FAILURE") => CheckState::Failure,
            // Completed but no conclusion reported yet → still running.
            None => CheckState::Pending,
            // CANCELLED / TIMED_OUT / STARTUP_FAILURE / ACTION_REQUIRED, or any
            // conclusion we don't recognize: the run finished without passing,
            // so surface it as an error instead of hiding it as pending.
            Some(_) => CheckState::Error,
        },
        // QUEUED / IN_PROGRESS / WAITING / PENDING / REQUESTED → still running.
        Some(_) => CheckState::Pending,
    }
}

/// Map a legacy `StatusContext`'s state to a [`CheckState`].
fn status_context_state(state: Option<&str>) -> CheckState {
    match state {
        Some("SUCCESS") => CheckState::Success,
        Some("FAILURE") => CheckState::Failure,
        Some("ERROR") => CheckState::Error,
        // PENDING / EXPECTED / anything unrecognized → not yet resolved.
        _ => CheckState::Pending,
    }
}

/// Maps an authored-PR index to that PR's complete, deduplicated checks — with
/// `required` flags and states resolved — as learned by [`fetch_required_checks`].
type AuthoritativeChecks = HashMap<usize, Vec<CheckStatus>>;

/// Ceiling on rollup-context pages fetched per PR in [`fetch_required_checks`]
/// (100 contexts each). A PR still paginating past this many pages is left out of
/// the authoritative map so [`finalize_checks`] marks it `Unknown` rather than
/// claim a rollup from truncated data. 20 pages = 2000 contexts, far above any
/// realistic PR.
const MAX_CHECK_PAGES: usize = 20;

/// Aliased target for one authored PR in the required-checks call.
struct ReqTarget {
    index: usize,
    owner: String,
    name: String,
    number: u64,
}

/// Second GraphQL round-trip: for each authored PR, fetch the *complete* set of
/// status-check contexts with their `isRequired(pullRequestNumber:)` flags (which
/// need a literal PR number the bulk `search(...)` query can't provide) and
/// states. We alias one `repository(...) { pullRequest(number: N) { ... } }` per
/// PR (index `i` → key `p{i}`), mirroring the aliased-per-repo `fetch_releases`
/// pattern, and paginate each PR's `contexts` connection until exhausted — a
/// required check beyond the first 100 contexts (common on large monorepos) would
/// otherwise be silently dropped and let a blocked merge read green. Returns a map
/// from authored-PR index to its authoritative checks.
fn fetch_required_checks(authored: &[Pr]) -> Result<(AuthoritativeChecks, Vec<String>)> {
    let targets: Vec<ReqTarget> = authored
        .iter()
        .enumerate()
        .filter_map(|(index, pr)| {
            let (owner, name) = pr.repo.split_once('/')?;
            Some(ReqTarget {
                index,
                owner: owner.to_string(),
                name: name.to_string(),
                number: pr.number,
            })
        })
        .collect();
    if targets.is_empty() {
        return Ok((HashMap::new(), Vec::new()));
    }

    let mut warnings = Vec::new();
    // Accumulated context nodes per PR index, appended across pages. A key is
    // present iff at least one page of that PR's contexts was read successfully.
    let mut accum: HashMap<usize, Vec<CheckContextNode>> = HashMap::new();
    // PR indices whose contexts are known truncated (page cap hit, cursor
    // missing, or a continuation page failed to resolve). Excluded from the
    // output so `finalize_checks` declines to claim a rollup for them.
    let mut incomplete: HashSet<usize> = HashSet::new();
    // (PR index, cursor for the next page). First page uses `None`.
    let mut pending: Vec<(usize, Option<String>)> =
        targets.iter().map(|t| (t.index, None)).collect();

    for page in 0..MAX_CHECK_PAGES {
        if pending.is_empty() {
            break;
        }
        let q = build_required_query(&targets, &pending);
        let output = Command::new("gh")
            .args(["api", "graphql", "-f"])
            .arg(format!("query={q}"))
            .output()
            .context("failed to invoke gh; is it installed and on PATH?")?;
        let (mut data, w) = parse_graphql::<RequiredChecksData>(&output, "required-checks")?;
        warnings.extend(w);

        let mut next_pending = Vec::new();
        for (index, cursor) in &pending {
            let key = format!("p{index}");
            let contexts = match data.aliases.remove(&key) {
                Some(Some(repo)) => repo.pull_request.and_then(take_req_contexts),
                _ => None,
            };
            let Some(contexts) = contexts else {
                // No contexts page for this alias. On a continuation, that
                // truncates a PR we'd already started paginating → decline to
                // claim a rollup. On the first page (a missing/null alias, e.g. a
                // SAML-blocked repo, or a head commit with no rollup) the PR is
                // simply absent → Unknown via `finalize_checks`'s `None` arm.
                if cursor.is_some() {
                    incomplete.insert(*index);
                }
                continue;
            };
            let has_next = contexts.page_info.has_next_page;
            let end_cursor = contexts.page_info.end_cursor;
            accum
                .entry(*index)
                .or_default()
                .extend(contexts.nodes.into_iter().flatten());
            if has_next {
                match end_cursor {
                    Some(cursor) => next_pending.push((*index, Some(cursor))),
                    // hasNextPage with no cursor can't be followed → truncated.
                    None => {
                        incomplete.insert(*index);
                    }
                }
            }
        }
        pending = next_pending;

        // Exhausted the page budget with PRs still paginating: their context set
        // is truncated, so decline to claim a rollup for them.
        if page + 1 == MAX_CHECK_PAGES && !pending.is_empty() {
            for (index, _) in &pending {
                incomplete.insert(*index);
            }
            warnings.push(format!(
                "checks: {} PR(s) have more than {} check contexts; merge-readiness left unknown",
                pending.len(),
                MAX_CHECK_PAGES * 100
            ));
        }
    }

    let mut out: AuthoritativeChecks = HashMap::new();
    for (index, nodes) in accum {
        if incomplete.contains(&index) {
            continue;
        }
        out.insert(index, checks_from_context_nodes(nodes.iter()));
    }
    Ok((out, warnings))
}

/// Build one required-checks GraphQL query for the currently `pending` aliases,
/// resuming each PR's `contexts` connection from its cursor (`after:`) when set.
fn build_required_query(targets: &[ReqTarget], pending: &[(usize, Option<String>)]) -> String {
    use std::fmt::Write as _;

    let by_index: HashMap<usize, &ReqTarget> = targets.iter().map(|t| (t.index, t)).collect();
    let mut q = String::from("query {\n");
    for (index, cursor) in pending {
        let Some(target) = by_index.get(index) else {
            continue;
        };
        let after = match cursor {
            Some(c) => format!(", after: \"{}\"", escape_graphql_string(c)),
            None => String::new(),
        };
        let num = target.number;
        writeln!(
            &mut q,
            "  p{index}: repository(owner: \"{owner}\", name: \"{name}\") {{ pullRequest(number: {num}) {{ commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ contexts(first: 100{after}) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ __typename ... on CheckRun {{ name status conclusion startedAt detailsUrl isRequired(pullRequestNumber: {num}) }} ... on StatusContext {{ context state targetUrl isRequired(pullRequestNumber: {num}) }} }} }} }} }} }} }} }} }}",
            owner = escape_graphql_string(&target.owner),
            name = escape_graphql_string(&target.name),
        )
        .unwrap();
    }
    q.push_str("}\n");
    q
}

/// Extract (by value) the status-check-rollup contexts connection from one
/// required-checks PR node, or `None` if the head commit had no rollup at all.
fn take_req_contexts(node: ReqPrNode) -> Option<CheckContexts> {
    let commit = node.commits?.nodes.into_iter().flatten().next()?.commit?;
    commit.status_check_rollup?.contexts
}

/// Replace each authored PR's checks with the authoritative set from the second
/// call and recompute its rollup. A PR already Unknown (mergeable/rollup not yet
/// computed) stays Unknown. A PR with checks but no authoritative entry (its
/// contexts couldn't be fetched, or were truncated past the page cap) is marked
/// Unknown rather than claiming a false green. An empty authoritative list for a
/// PR that had checks is treated as the same discrepancy → Unknown.
fn finalize_checks(authored: &mut [Pr], authoritative: &AuthoritativeChecks) {
    for (i, pr) in authored.iter_mut().enumerate() {
        if pr.checks.is_empty() || pr.checks_rollup == ChecksRollup::Unknown {
            continue;
        }
        match authoritative.get(&i) {
            Some(checks) if !checks.is_empty() => {
                pr.checks = checks.clone();
                pr.checks_rollup = model::rollup_from_required(&pr.checks);
            }
            _ => pr.checks_rollup = ChecksRollup::Unknown,
        }
    }
}

#[derive(Deserialize)]
struct RequiredChecksData {
    #[serde(flatten)]
    aliases: HashMap<String, Option<ReqRepoNode>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ReqRepoNode {
    #[serde(rename = "pullRequest")]
    pull_request: Option<ReqPrNode>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ReqPrNode {
    commits: Option<CommitsConnection>,
}

// --- releases / tags ---

const RR_FRAGMENT: &str = r#"
fragment RR on Repository {
  nameWithOwner
  releases(first: 3, orderBy: {field: CREATED_AT, direction: DESC}) {
    nodes {
      name
      tagName
      createdAt
      publishedAt
      url
      isPrerelease
    }
  }
  refs(refPrefix: "refs/tags/", first: 1, orderBy: {field: TAG_COMMIT_DATE, direction: DESC}) {
    nodes {
      name
      target {
        __typename
        ... on Commit { committedDate }
        ... on Tag {
          tagger { date }
          target {
            __typename
            ... on Commit { committedDate }
          }
        }
      }
    }
  }
}
"#;

fn fetch_releases(repos: &[config::RepoRef]) -> Result<(Vec<RepoReleaseInfo>, Vec<String>)> {
    use std::fmt::Write as _;

    let mut q = String::from("query {\n");
    for (i, r) in repos.iter().enumerate() {
        // `RepoRef` parsing already rejects empties and enforces owner/name;
        // escape quotes+backslashes defensively anyway so we can't be
        // surprised by exotic repo names.
        writeln!(
            &mut q,
            "  r{i}: repository(owner: \"{owner}\", name: \"{name}\") {{ ...RR }}",
            owner = escape_graphql_string(&r.owner),
            name = escape_graphql_string(&r.name),
        )
        .unwrap();
    }
    q.push_str("}\n");
    q.push_str(RR_FRAGMENT);

    let output = Command::new("gh")
        .args(["api", "graphql", "-f"])
        .arg(format!("query={q}"))
        .output()
        .context("failed to invoke gh; is it installed and on PATH?")?;
    let (data, warnings) = parse_graphql::<ReleasesData>(&output, "releases")?;
    let mut out = Vec::with_capacity(repos.len());
    for (i, r) in repos.iter().enumerate() {
        let key = format!("r{i}");
        let node = data.aliases.get(&key).and_then(|v| v.as_ref());
        out.push(node_to_repo_release_info(r, node));
    }
    Ok((out, warnings))
}

fn escape_graphql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c => out.push(c),
        }
    }
    out
}

fn node_to_repo_release_info(r: &config::RepoRef, node: Option<&RRNode>) -> RepoReleaseInfo {
    let repo = r.full();
    let Some(node) = node else {
        return RepoReleaseInfo {
            repo,
            recent_releases: Vec::new(),
            latest_tag: None,
        };
    };
    let recent_releases: Vec<ReleaseInfo> = node
        .releases
        .as_ref()
        .map(|conn| {
            conn.nodes
                .iter()
                .map(|rel| {
                    let created = rel
                        .published_at
                        .as_deref()
                        .or(rel.created_at.as_deref())
                        .and_then(parse_ts)
                        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch"));
                    ReleaseInfo {
                        tag_name: rel.tag_name.clone().unwrap_or_default(),
                        name: rel.name.clone(),
                        url: rel.url.clone().unwrap_or_default(),
                        created_at: created,
                        is_prerelease: rel.is_prerelease.unwrap_or(false),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let latest_tag = node
        .refs
        .as_ref()
        .and_then(|refs| refs.nodes.first())
        .and_then(|ref_node| {
            let committed_at = tag_target_date(&ref_node.target)?;
            Some(TagInfo {
                name: ref_node.name.clone(),
                committed_at,
            })
        });
    RepoReleaseInfo {
        repo,
        recent_releases,
        latest_tag,
    }
}

fn tag_target_date(target: &Option<TagTarget>) -> Option<DateTime<Utc>> {
    let target = target.as_ref()?;
    match target {
        TagTarget::Commit { committed_date } => committed_date.as_deref().and_then(parse_ts),
        TagTarget::Tag {
            tagger,
            target: inner,
        } => {
            // Annotated tags: the tag object itself has a `tagger.date`, and
            // the underlying Commit carries `committedDate`. Prefer the
            // commit date; fall back to the tagger date.
            if let Some(inner) = inner.as_ref()
                && let TagTarget::Commit { committed_date } = inner.as_ref()
                && let Some(ts) = committed_date.as_deref().and_then(parse_ts)
            {
                return Some(ts);
            }
            tagger
                .as_ref()
                .and_then(|t| t.date.as_deref())
                .and_then(parse_ts)
        }
        TagTarget::Other => None,
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[derive(Deserialize)]
struct ReleasesData {
    #[serde(flatten)]
    aliases: HashMap<String, Option<RRNode>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RRNode {
    releases: Option<ReleasesConnection>,
    refs: Option<RefsConnection>,
}

#[derive(Deserialize)]
struct ReleasesConnection {
    nodes: Vec<ReleaseNode>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ReleaseNode {
    name: Option<String>,
    #[serde(rename = "tagName")]
    tag_name: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "publishedAt")]
    published_at: Option<String>,
    url: Option<String>,
    #[serde(rename = "isPrerelease")]
    is_prerelease: Option<bool>,
}

#[derive(Deserialize)]
struct RefsConnection {
    nodes: Vec<RefNode>,
}

#[derive(Deserialize)]
struct RefNode {
    name: String,
    target: Option<TagTarget>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum TagTarget {
    Commit {
        #[serde(rename = "committedDate")]
        committed_date: Option<String>,
    },
    Tag {
        tagger: Option<Tagger>,
        target: Option<Box<TagTarget>>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Tagger {
    date: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(owner: &str, name: &str) -> config::RepoRef {
        config::RepoRef {
            owner: owner.into(),
            name: name.into(),
        }
    }

    fn parse_node(json: &str) -> Option<RRNode> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn release_only() {
        let json = r#"{
            "releases": { "nodes": [{
                "name": "v1.2.3",
                "tagName": "v1.2.3",
                "createdAt": "2024-05-01T00:00:00Z",
                "publishedAt": "2024-05-02T00:00:00Z",
                "url": "https://github.com/o/r/releases/tag/v1.2.3",
                "isPrerelease": false
            }]},
            "refs": { "nodes": [] }
        }"#;
        let node = parse_node(json);
        let info = node_to_repo_release_info(&repo("o", "r"), node.as_ref());
        assert_eq!(info.repo, "o/r");
        assert_eq!(info.recent_releases.len(), 1);
        let rel = &info.recent_releases[0];
        assert_eq!(rel.tag_name, "v1.2.3");
        // Prefer publishedAt.
        assert_eq!(rel.created_at.to_rfc3339(), "2024-05-02T00:00:00+00:00");
        assert!(!rel.is_prerelease);
        assert!(info.latest_tag.is_none());
    }

    #[test]
    fn releases_multiple_ordered_newest_first() {
        let json = r#"{
            "releases": { "nodes": [
                {"name": "v1.2.3", "tagName": "v1.2.3", "createdAt": "2024-05-03T00:00:00Z", "publishedAt": null, "url": "https://github.com/o/r/releases/tag/v1.2.3", "isPrerelease": false},
                {"name": "v1.2.2", "tagName": "v1.2.2", "createdAt": "2024-05-02T00:00:00Z", "publishedAt": null, "url": "https://github.com/o/r/releases/tag/v1.2.2", "isPrerelease": true},
                {"name": "v1.2.1", "tagName": "v1.2.1", "createdAt": "2024-05-01T00:00:00Z", "publishedAt": null, "url": "https://github.com/o/r/releases/tag/v1.2.1", "isPrerelease": false}
            ]},
            "refs": { "nodes": [] }
        }"#;
        let node = parse_node(json);
        let info = node_to_repo_release_info(&repo("o", "r"), node.as_ref());
        let tags: Vec<&str> = info
            .recent_releases
            .iter()
            .map(|r| r.tag_name.as_str())
            .collect();
        assert_eq!(tags, vec!["v1.2.3", "v1.2.2", "v1.2.1"]);
        assert!(info.recent_releases[1].is_prerelease);
    }

    #[test]
    fn tag_only_lightweight_commit() {
        let json = r#"{
            "releases": { "nodes": [] },
            "refs": {
                "nodes": [{
                    "name": "v0.9.0",
                    "target": {
                        "__typename": "Commit",
                        "committedDate": "2024-04-01T12:00:00Z"
                    }
                }]
            }
        }"#;
        let node = parse_node(json);
        let info = node_to_repo_release_info(&repo("o", "r"), node.as_ref());
        assert!(info.recent_releases.is_empty());
        let tag = info.latest_tag.expect("tag present");
        assert_eq!(tag.name, "v0.9.0");
        assert_eq!(tag.committed_at.to_rfc3339(), "2024-04-01T12:00:00+00:00");
    }

    #[test]
    fn tag_only_annotated() {
        let json = r#"{
            "releases": { "nodes": [] },
            "refs": {
                "nodes": [{
                    "name": "v0.9.1",
                    "target": {
                        "__typename": "Tag",
                        "tagger": { "date": "2024-04-03T00:00:00Z" },
                        "target": {
                            "__typename": "Commit",
                            "committedDate": "2024-04-02T00:00:00Z"
                        }
                    }
                }]
            }
        }"#;
        let node = parse_node(json);
        let info = node_to_repo_release_info(&repo("o", "r"), node.as_ref());
        let tag = info.latest_tag.expect("tag present");
        assert_eq!(tag.name, "v0.9.1");
        // Prefer the commit date over tagger date.
        assert_eq!(tag.committed_at.to_rfc3339(), "2024-04-02T00:00:00+00:00");
    }

    #[test]
    fn tag_only_annotated_falls_back_to_tagger() {
        let json = r#"{
            "releases": { "nodes": [] },
            "refs": {
                "nodes": [{
                    "name": "v0.9.2",
                    "target": {
                        "__typename": "Tag",
                        "tagger": { "date": "2024-04-10T00:00:00Z" },
                        "target": null
                    }
                }]
            }
        }"#;
        let node = parse_node(json);
        let info = node_to_repo_release_info(&repo("o", "r"), node.as_ref());
        let tag = info.latest_tag.expect("tag present");
        assert_eq!(tag.committed_at.to_rfc3339(), "2024-04-10T00:00:00+00:00");
    }

    #[test]
    fn release_and_tag_both() {
        let json = r#"{
            "releases": { "nodes": [{
                "name": "v2.0.0",
                "tagName": "v2.0.0",
                "createdAt": "2024-05-01T00:00:00Z",
                "publishedAt": null,
                "url": "https://github.com/o/r/releases/tag/v2.0.0",
                "isPrerelease": true
            }]},
            "refs": {
                "nodes": [{
                    "name": "v2.0.1",
                    "target": {
                        "__typename": "Commit",
                        "committedDate": "2024-05-05T00:00:00Z"
                    }
                }]
            }
        }"#;
        let node = parse_node(json);
        let info = node_to_repo_release_info(&repo("o", "r"), node.as_ref());
        assert_eq!(info.recent_releases.len(), 1);
        let rel = &info.recent_releases[0];
        assert_eq!(rel.tag_name, "v2.0.0");
        // Falls back to createdAt when publishedAt is null.
        assert_eq!(rel.created_at.to_rfc3339(), "2024-05-01T00:00:00+00:00");
        assert!(rel.is_prerelease);
        let tag = info.latest_tag.expect("tag");
        assert_eq!(tag.name, "v2.0.1");
    }

    #[test]
    fn neither_release_nor_tag() {
        let json = r#"{ "releases": { "nodes": [] }, "refs": { "nodes": [] } }"#;
        let node = parse_node(json);
        let info = node_to_repo_release_info(&repo("o", "r"), node.as_ref());
        assert!(info.recent_releases.is_empty());
        assert!(info.latest_tag.is_none());
        assert_eq!(info.repo, "o/r");
    }

    #[test]
    fn missing_repo_alias_none_node() {
        let info = node_to_repo_release_info(&repo("o", "gone"), None);
        assert_eq!(info.repo, "o/gone");
        assert!(info.recent_releases.is_empty());
        assert!(info.latest_tag.is_none());
    }

    #[test]
    fn escape_graphql_string_escapes_quotes_and_backslashes() {
        assert_eq!(escape_graphql_string("plain"), "plain");
        assert_eq!(escape_graphql_string("a\"b"), "a\\\"b");
        assert_eq!(escape_graphql_string("a\\b"), "a\\\\b");
    }

    #[test]
    fn resolve_review_thread_mutation_uses_thread_id_and_confirms_resolution() {
        assert!(RESOLVE_REVIEW_THREAD_MUTATION.contains("$threadId: ID!"));
        assert!(RESOLVE_REVIEW_THREAD_MUTATION.contains("resolveReviewThread"));
        let data: ResolveReviewThreadData = serde_json::from_str(
            r#"{
                "resolveReviewThread": {
                    "thread": { "id": "PRRT_1", "isResolved": true }
                }
            }"#,
        )
        .unwrap();
        assert!(confirm_resolved_review_thread("PRRT_1", data, vec![]).is_ok());
    }

    #[test]
    fn resolve_review_thread_rejects_unconfirmed_or_mismatched_responses() {
        for response in [
            r#"{
                "resolveReviewThread": {
                    "thread": { "id": "PRRT_1", "isResolved": false }
                }
            }"#,
            r#"{
                "resolveReviewThread": {
                    "thread": { "id": "PRRT_other", "isResolved": true }
                }
            }"#,
            r#"{ "resolveReviewThread": { "thread": null } }"#,
        ] {
            let data: ResolveReviewThreadData = serde_json::from_str(response).unwrap();
            let error = confirm_resolved_review_thread("PRRT_1", data, vec![]).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("GitHub did not confirm the thread was resolved")
            );
        }
    }

    #[test]
    fn node_to_pr_keeps_unresolved_threads_including_outdated() {
        // Three threads: resolved (dropped), unresolved (kept), and
        // unresolved+outdated (kept with the [outdated] flag).
        let json = r#"{
            "number": 12,
            "title": "Fix the thing",
            "url": "https://github.com/o/r/pull/12",
            "repository": { "nameWithOwner": "o/r" },
            "author": { "login": "me" },
            "reviewThreads": { "nodes": [
                {
                    "id": "PRRT_resolved",
                    "isResolved": true,
                    "isOutdated": false,
                    "comments": { "nodes": [
                        { "author": { "login": "resolved-guy" }, "bodyText": "done", "url": "https://x/1", "path": "src/a.rs" }
                    ]}
                },
                {
                    "id": "PRRT_current",
                    "isResolved": false,
                    "isOutdated": false,
                    "comments": { "nodes": [
                        { "author": { "login": "carol" }, "bodyText": "add a test here", "url": "https://x/2", "path": "src/foo.rs" }
                    ]}
                },
                {
                    "id": "PRRT_outdated",
                    "isResolved": false,
                    "isOutdated": true,
                    "comments": { "nodes": [
                        { "author": { "login": "dave" }, "bodyText": "nit: rename", "url": "https://x/3", "path": "" }
                    ]}
                }
            ]}
        }"#;
        let node: PrNode = serde_json::from_str(json).unwrap();
        let pr = node_to_pr(node).expect("pr parsed");
        assert_eq!(pr.unresolved_comments.len(), 2);

        let carol = &pr.unresolved_comments[0];
        assert_eq!(carol.author, "carol");
        assert_eq!(carol.body, "add a test here");
        assert_eq!(carol.url, "https://x/2");
        assert_eq!(carol.thread_id.as_deref(), Some("PRRT_current"));
        assert_eq!(carol.path.as_deref(), Some("src/foo.rs"));
        assert!(!carol.is_outdated);

        let dave = &pr.unresolved_comments[1];
        assert_eq!(dave.author, "dave");
        assert_eq!(dave.thread_id.as_deref(), Some("PRRT_outdated"));
        assert!(dave.is_outdated);
        // Empty path collapses to None.
        assert_eq!(dave.path, None);
    }

    #[test]
    fn node_to_pr_keeps_human_review_comments_without_threads() {
        let json = r#"{
            "number": 12,
            "title": "Fix the thing",
            "url": "https://github.com/o/r/pull/12",
            "repository": { "nameWithOwner": "o/r" },
            "author": { "login": "me" },
            "reviews": { "nodes": [
                {
                    "author": { "__typename": "User", "login": "carol" },
                    "bodyText": "Please skip the legacy flow.\nMore detail here.",
                    "url": "https://github.com/o/r/pull/12#pullrequestreview-42"
                },
                {
                    "author": { "__typename": "Bot", "login": "review-bot" },
                    "bodyText": "Automated review passed",
                    "url": "https://github.com/o/r/pull/12#pullrequestreview-43"
                },
                {
                    "author": { "__typename": "User", "login": "dave" },
                    "bodyText": "   ",
                    "url": "https://github.com/o/r/pull/12#pullrequestreview-44"
                }
            ]},
            "reviewThreads": { "nodes": [] }
        }"#;
        let node: PrNode = serde_json::from_str(json).unwrap();
        let pr = node_to_pr(node).expect("pr parsed");
        assert_eq!(pr.unresolved_comments.len(), 1);
        let comment = &pr.unresolved_comments[0];
        assert_eq!(comment.kind, PrCommentKind::ReviewSummary);
        assert_eq!(comment.author, "carol");
        assert_eq!(
            comment.body,
            "Please skip the legacy flow. More detail here."
        );
        assert_eq!(
            comment.url,
            "https://github.com/o/r/pull/12#pullrequestreview-42"
        );
        assert_eq!(comment.path, None);
        assert_eq!(comment.thread_id, None);
        assert!(!comment.is_outdated);
        assert!(pr.reviewers.iter().any(|reviewer| {
            reviewer.login == "carol" && reviewer.state == ReviewState::Commented
        }));
    }

    #[test]
    fn node_to_pr_skips_threads_without_first_comment_or_url() {
        let json = r#"{
            "number": 5,
            "repository": { "nameWithOwner": "o/r" },
            "reviewThreads": { "nodes": [
                { "isResolved": false, "isOutdated": false, "comments": { "nodes": [] } },
                { "isResolved": false, "isOutdated": false, "comments": { "nodes": [
                    { "author": { "login": "eve" }, "bodyText": "hmm", "url": "", "path": null }
                ]}}
            ]}
        }"#;
        let node: PrNode = serde_json::from_str(json).unwrap();
        let pr = node_to_pr(node).expect("pr parsed");
        assert!(pr.unresolved_comments.is_empty());
    }

    #[test]
    fn checks_from_commits_normalizes_contexts() {
        let json = r#"{
            "nodes": [{
                "commit": {
                    "statusCheckRollup": {
                        "state": "FAILURE",
                        "contexts": { "nodes": [
                            { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "https://ci/build" },
                            { "__typename": "CheckRun", "name": "test", "status": "IN_PROGRESS", "conclusion": null, "detailsUrl": "" },
                            { "__typename": "StatusContext", "context": "legacy", "state": "FAILURE", "targetUrl": "https://ci/legacy" }
                        ]}
                    }
                }
            }]
        }"#;
        let commits: CommitsConnection = serde_json::from_str(json).unwrap();
        let (checks, present) = checks_from_commits(&Some(commits));
        assert!(present);
        assert_eq!(checks.len(), 3);
        assert_eq!(checks[0].name, "build");
        assert_eq!(checks[0].state, CheckState::Success);
        assert_eq!(checks[0].url.as_deref(), Some("https://ci/build"));
        assert!(!checks[0].required);
        assert_eq!(checks[1].name, "test");
        assert_eq!(checks[1].state, CheckState::Pending);
        // Empty detailsUrl collapses to None.
        assert_eq!(checks[1].url, None);
        assert_eq!(checks[2].name, "legacy");
        assert_eq!(checks[2].state, CheckState::Failure);
    }

    #[test]
    fn checks_from_commits_keeps_latest_duplicate_check_run() {
        // GitHub returns superseded workflow jobs alongside their replacements.
        // Deliberately put the newest run first to prove selection is based on
        // startedAt rather than response order.
        let json = r#"{
            "nodes": [{
                "commit": {
                    "statusCheckRollup": {
                        "contexts": { "nodes": [
                            { "__typename": "CheckRun", "name": "validate-openapi", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-07-14T17:58:08Z", "detailsUrl": "https://ci/new" },
                            { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-07-14T17:58:05Z", "detailsUrl": "https://ci/build" },
                            { "__typename": "CheckRun", "name": "validate-openapi", "status": "COMPLETED", "conclusion": "CANCELLED", "startedAt": "2026-07-14T17:58:03Z", "detailsUrl": "https://ci/old" }
                        ]}
                    }
                }
            }]
        }"#;
        let commits: CommitsConnection = serde_json::from_str(json).unwrap();
        let (checks, present) = checks_from_commits(&Some(commits));
        assert!(present);
        assert_eq!(checks.len(), 2);

        let validate = checks
            .iter()
            .find(|check| check.name == "validate-openapi")
            .unwrap();
        assert_eq!(validate.state, CheckState::Success);
        assert_eq!(validate.url.as_deref(), Some("https://ci/new"));
    }

    #[test]
    fn check_run_state_maps_conclusions() {
        // Clean outcomes.
        assert_eq!(
            check_run_state(Some("COMPLETED"), Some("SUCCESS")),
            CheckState::Success
        );
        assert_eq!(
            check_run_state(Some("COMPLETED"), Some("SKIPPED")),
            CheckState::Skipped
        );
        assert_eq!(
            check_run_state(Some("COMPLETED"), Some("NEUTRAL")),
            CheckState::Neutral
        );
        assert_eq!(
            check_run_state(Some("COMPLETED"), Some("STALE")),
            CheckState::Neutral
        );
        // A genuine test failure stays a failure.
        assert_eq!(
            check_run_state(Some("COMPLETED"), Some("FAILURE")),
            CheckState::Failure
        );
        // Abnormal terminations — including cancellation — are errors, not
        // failures, and must not fall through to Pending.
        for conclusion in [
            "CANCELLED",
            "TIMED_OUT",
            "STARTUP_FAILURE",
            "ACTION_REQUIRED",
            "SOMETHING_NEW", // an unrecognized conclusion still surfaces.
        ] {
            assert_eq!(
                check_run_state(Some("COMPLETED"), Some(conclusion)),
                CheckState::Error,
                "conclusion {conclusion} should map to Error"
            );
        }
        // Completed but no conclusion reported yet, and not-yet-completed runs,
        // are still pending.
        assert_eq!(
            check_run_state(Some("COMPLETED"), None),
            CheckState::Pending
        );
        assert_eq!(
            check_run_state(Some("IN_PROGRESS"), None),
            CheckState::Pending
        );
        assert_eq!(check_run_state(Some("QUEUED"), None), CheckState::Pending);
    }

    #[test]
    fn checks_from_commits_cancelled_run_is_error() {
        // A cancelled CheckRun that is the current result (no newer duplicate)
        // must land in the error/failing set, not be hidden as pending.
        let json = r#"{
            "nodes": [{
                "commit": {
                    "statusCheckRollup": {
                        "contexts": { "nodes": [
                            { "__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "CANCELLED", "startedAt": "2026-07-14T17:58:03Z", "detailsUrl": "https://ci/test" }
                        ]}
                    }
                }
            }]
        }"#;
        let commits: CommitsConnection = serde_json::from_str(json).unwrap();
        let (checks, present) = checks_from_commits(&Some(commits));
        assert!(present);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].state, CheckState::Error);
    }

    #[test]
    fn checks_from_commits_absent_rollup_is_no_checks() {
        let json = r#"{ "nodes": [{ "commit": { "statusCheckRollup": null } }] }"#;
        let commits: CommitsConnection = serde_json::from_str(json).unwrap();
        let (checks, present) = checks_from_commits(&Some(commits));
        assert!(!present);
        assert!(checks.is_empty());
        // Entirely absent commits connection is also "no checks".
        let (checks, present) = checks_from_commits(&None);
        assert!(!present);
        assert!(checks.is_empty());
    }

    #[test]
    fn req_contexts_map_state_and_required() {
        // The required-checks call fetches state + isRequired together, so the
        // shared mapper must resolve both — the rollup is then computable from
        // the second call's authoritative data alone.
        let json = r#"{
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "contexts": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [
                    { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "https://ci/build", "isRequired": true },
                    { "__typename": "CheckRun", "name": "optional", "status": "COMPLETED", "conclusion": "FAILURE", "detailsUrl": "", "isRequired": false },
                    { "__typename": "StatusContext", "context": "legacy", "state": "FAILURE", "targetUrl": "https://ci/legacy", "isRequired": true }
                ]
            }}}}]}
        }"#;
        let node: ReqPrNode = serde_json::from_str(json).unwrap();
        let contexts = take_req_contexts(node).unwrap();
        assert!(!contexts.page_info.has_next_page);
        let checks = checks_from_context_nodes(contexts.nodes.iter().flatten());

        let build = checks.iter().find(|c| c.name == "build").unwrap();
        assert_eq!(build.state, CheckState::Success);
        assert!(build.required);
        assert_eq!(build.url.as_deref(), Some("https://ci/build"));

        let optional = checks.iter().find(|c| c.name == "optional").unwrap();
        assert_eq!(optional.state, CheckState::Failure);
        assert!(!optional.required);

        let legacy = checks.iter().find(|c| c.name == "legacy").unwrap();
        assert_eq!(legacy.state, CheckState::Failure);
        assert!(legacy.required);
    }

    #[test]
    fn checks_from_context_nodes_dedups_across_pages() {
        // Accumulated nodes may include the same check name from a retried run
        // (potentially split across pages); the newest by startedAt wins
        // regardless of order, and its required flag is preserved.
        let json = r#"{ "nodes": [
            { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "CANCELLED", "startedAt": "2026-07-14T10:00:00Z", "detailsUrl": "https://ci/old", "isRequired": true },
            { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-07-14T11:00:00Z", "detailsUrl": "https://ci/new", "isRequired": true }
        ]}"#;
        let contexts: CheckContexts = serde_json::from_str(json).unwrap();
        let checks = checks_from_context_nodes(contexts.nodes.iter().flatten());
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].state, CheckState::Success);
        assert_eq!(checks[0].url.as_deref(), Some("https://ci/new"));
        assert!(checks[0].required);
    }

    #[test]
    fn build_required_query_requests_pageinfo_and_paginates() {
        let targets = vec![
            ReqTarget {
                index: 0,
                owner: "o".into(),
                name: "r".into(),
                number: 7,
            },
            ReqTarget {
                index: 1,
                owner: "o".into(),
                name: "r".into(),
                number: 8,
            },
        ];
        // p0 starts fresh; p1 resumes from a cursor.
        let pending = vec![(0usize, None), (1usize, Some("CUR123".to_string()))];
        let q = build_required_query(&targets, &pending);

        assert!(q.contains("p0: repository(owner: \"o\", name: \"r\")"));
        assert!(q.contains("pullRequest(number: 7)"));
        assert!(q.contains("pageInfo { hasNextPage endCursor }"));
        assert!(q.contains("isRequired(pullRequestNumber: 7)"));
        assert!(q.contains("isRequired(pullRequestNumber: 8)"));
        // State fields present so the rollup is computable from this call alone.
        assert!(q.contains("status conclusion startedAt detailsUrl"));
        // No cursor → no `after:`; a cursor → resume from it.
        assert!(q.contains("contexts(first: 100) {"));
        assert!(q.contains("contexts(first: 100, after: \"CUR123\") {"));
    }

    #[test]
    fn finalize_checks_green_when_only_nonrequired_fails() {
        // Provisional preview from the bulk query: two checks, no required flags.
        let json = r#"{
            "number": 1,
            "repository": { "nameWithOwner": "o/r" },
            "mergeable": "MERGEABLE",
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "state": "FAILURE", "contexts": { "nodes": [
                { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "" },
                { "__typename": "CheckRun", "name": "lint", "status": "COMPLETED", "conclusion": "FAILURE", "detailsUrl": "" }
            ]}}}}]}
        }"#;
        let node: PrNode = serde_json::from_str(json).unwrap();
        let pr = node_to_pr(node).unwrap();
        assert_eq!(pr.checks.len(), 2);
        // Provisional: mergeable known, no required flags yet → Green.
        assert_eq!(pr.checks_rollup, ChecksRollup::Green);

        // Authoritative set from the second call: required build passes,
        // non-required lint fails → the signal stays green.
        let authoritative_checks = vec![
            CheckStatus {
                name: "build".into(),
                state: CheckState::Success,
                url: None,
                required: true,
            },
            CheckStatus {
                name: "lint".into(),
                state: CheckState::Failure,
                url: None,
                required: false,
            },
        ];
        let mut authoritative = HashMap::new();
        authoritative.insert(0usize, authoritative_checks);

        let mut authored = vec![pr];
        finalize_checks(&mut authored, &authoritative);
        let pr = &authored[0];
        assert!(
            pr.checks
                .iter()
                .find(|c| c.name == "build")
                .unwrap()
                .required
        );
        assert!(
            !pr.checks
                .iter()
                .find(|c| c.name == "lint")
                .unwrap()
                .required
        );
        assert_eq!(pr.checks_rollup, ChecksRollup::Green);
    }

    #[test]
    fn finalize_checks_reconstructs_list_with_required_failure_beyond_preview() {
        // The bulk query's first-100-context preview shows only a passing build,
        // so the provisional rollup is Green. The paginated second call surfaces
        // a required failing check the preview never included; the rollup must go
        // Red and that check must appear in the finalized list.
        let json = r#"{
            "number": 1, "repository": { "nameWithOwner": "o/r" }, "mergeable": "MERGEABLE",
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "contexts": { "nodes": [
                { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "" }
            ]}}}}]}
        }"#;
        let node: PrNode = serde_json::from_str(json).unwrap();
        let pr = node_to_pr(node).unwrap();
        assert_eq!(pr.checks.len(), 1);
        assert_eq!(pr.checks_rollup, ChecksRollup::Green);

        let authoritative_checks = vec![
            CheckStatus {
                name: "build".into(),
                state: CheckState::Success,
                url: None,
                required: true,
            },
            CheckStatus {
                name: "e2e".into(),
                state: CheckState::Failure,
                url: None,
                required: true,
            },
        ];
        let mut authoritative = HashMap::new();
        authoritative.insert(0usize, authoritative_checks);

        let mut authored = vec![pr];
        finalize_checks(&mut authored, &authoritative);
        let pr = &authored[0];
        assert_eq!(pr.checks.len(), 2);
        assert!(
            pr.checks
                .iter()
                .any(|c| c.name == "e2e" && c.state == CheckState::Failure)
        );
        assert_eq!(pr.checks_rollup, ChecksRollup::Red);
    }

    #[test]
    fn finalize_checks_unknown_when_authoritative_missing() {
        // A PR with checks and known mergeability but absent from the
        // authoritative map (its contexts couldn't be fetched, or were truncated
        // past the page cap) must not claim a false green.
        let json = r#"{
            "number": 1, "repository": { "nameWithOwner": "o/r" },
            "mergeable": "MERGEABLE",
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "contexts": { "nodes": [
                { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "" }
            ]}}}}]}
        }"#;
        let node: PrNode = serde_json::from_str(json).unwrap();
        let pr = node_to_pr(node).unwrap();
        assert_eq!(pr.checks_rollup, ChecksRollup::Green);
        let mut authored = vec![pr];
        finalize_checks(&mut authored, &HashMap::new());
        assert_eq!(authored[0].checks_rollup, ChecksRollup::Unknown);
    }

    #[test]
    fn node_to_pr_mergeable_unknown_yields_unknown_rollup() {
        let json = r#"{
            "number": 1, "repository": { "nameWithOwner": "o/r" },
            "mergeable": "UNKNOWN",
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "contexts": { "nodes": [
                { "__typename": "CheckRun", "name": "build", "status": "IN_PROGRESS", "conclusion": null, "detailsUrl": "" }
            ]}}}}]}
        }"#;
        let node: PrNode = serde_json::from_str(json).unwrap();
        let pr = node_to_pr(node).unwrap();
        assert_eq!(pr.checks.len(), 1);
        assert_eq!(pr.checks_rollup, ChecksRollup::Unknown);
        // A PR already Unknown is left alone by finalize_checks.
        let mut authored = vec![pr];
        finalize_checks(&mut authored, &HashMap::new());
        assert_eq!(authored[0].checks_rollup, ChecksRollup::Unknown);
    }

    #[test]
    fn normalized_comment_body_flattens_whitespace_but_does_not_truncate() {
        assert_eq!(normalized_comment_body(""), "");
        assert_eq!(
            normalized_comment_body("\n\n  hello  \nworld"),
            "hello world"
        );
        let long = "x".repeat(100);
        assert_eq!(normalized_comment_body(&long), long);
    }

    /// Build an `Output` with the given exit code and stdout/stderr text. On
    /// unix, `ExitStatus::from_raw` takes the wait-status word, so the exit code
    /// goes in the high byte (`code << 8`).
    #[cfg(unix)]
    fn output(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    // (a) A partial `authored` payload: `data` present with a leading `null`
    // node plus a populated `errors` array. `gh` exited non-zero (it carries
    // errors), but the accessible node survives and the warning is collected.
    #[cfg(unix)]
    #[test]
    fn parse_graphql_partial_payload_keeps_data_and_collects_warning() {
        let stdout = r#"{
            "data": {
                "viewer": { "login": "me" },
                "authored": { "nodes": [
                    null,
                    { "number": 338, "repository": { "nameWithOwner": "o/r" } }
                ]},
                "reviewing": { "nodes": [] }
            },
            "errors": [
                { "type": "FORBIDDEN", "path": ["authored", "nodes", 0],
                  "extensions": { "saml_failure": true },
                  "message": "Resource protected by organization SAML enforcement." }
            ]
        }"#;
        let out = output(
            1,
            stdout,
            "gh: Resource protected by organization SAML enforcement.",
        );
        let (data, warnings) = parse_graphql::<OpenDataResp>(&out, "open").unwrap();

        let authored: Vec<Pr> = data
            .authored
            .nodes
            .into_iter()
            .flatten()
            .filter_map(node_to_pr)
            .collect();
        assert_eq!(authored.len(), 1);
        assert_eq!(authored[0].number, 338);
        assert_eq!(data.viewer.login, "me");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("SAML enforcement"));
    }

    // (b) An errors-only / no-`data` payload is genuinely fatal.
    #[cfg(unix)]
    #[test]
    fn parse_graphql_errors_only_is_fatal() {
        let stdout = r#"{
            "errors": [ { "message": "Resource protected by organization SAML enforcement." } ]
        }"#;
        let out = output(1, stdout, "gh: some stderr");
        let err = match parse_graphql::<OpenDataResp>(&out, "open") {
            Ok(_) => panic!("errors-only payload must be fatal"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("SAML enforcement"));
    }

    // (c) `SearchResp` deserializes a `nodes: [null, {...}]` array without error,
    // and `.flatten()` drops the null.
    #[test]
    fn search_resp_tolerates_null_nodes() {
        let json = r#"{ "nodes": [
            null,
            { "number": 7, "repository": { "nameWithOwner": "o/r" } }
        ]}"#;
        let resp: SearchResp = serde_json::from_str(json).unwrap();
        assert_eq!(resp.nodes.len(), 2);
        let prs: Vec<Pr> = resp
            .nodes
            .into_iter()
            .flatten()
            .filter_map(node_to_pr)
            .collect();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 7);
    }

    // (d) A zero-exit envelope carrying an `errors` array still collects the
    // warning (partial-success-with-exit-0 path).
    #[cfg(unix)]
    #[test]
    fn parse_graphql_zero_exit_with_errors_collects_warning() {
        let stdout = r#"{
            "data": { "viewer": { "login": "me" },
                      "authored": { "nodes": [] },
                      "reviewing": { "nodes": [] } },
            "errors": [ { "message": "partial success warning" } ]
        }"#;
        let out = output(0, stdout, "");
        let (_data, warnings) = parse_graphql::<OpenDataResp>(&out, "open").unwrap();
        assert_eq!(warnings, vec!["partial success warning".to_string()]);
    }
}
