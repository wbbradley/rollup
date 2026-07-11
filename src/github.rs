use std::{collections::HashMap, process::Command};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, de::DeserializeOwned};

use crate::{
    config,
    model::{
        self, Pr, PrComment, ReleaseInfo, RepoReleaseInfo, ReviewState, ReviewerKind,
        ReviewerStatus, TagInfo,
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
  reviewThreads(first: 50) {
    nodes {
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
    let (viewer, authored, reviewing, w) = fetch_open()?;
    warnings.extend(w);
    // One fetch feeds both Me and People views; the render layer filters per
    // view. The fetch set must cover BOTH — `authors_for_people` excludes the
    // viewer, so on its own it would hide the viewer's own merged PRs in Me
    // mode. Take the union.
    let authors = model::merged_fetch_authors(&viewer, &authored, &reviewing);
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

    reviewers.sort_by_key(|r| r.login.to_lowercase());

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

    // Only the `authored:` query fetches `reviewThreads`; reviewing/merged nodes
    // leave it `None`, so their `unresolved_comments` come out empty. Surface the
    // first comment of each unresolved thread (including outdated ones).
    let mut unresolved_comments: Vec<PrComment> = Vec::new();
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
                author: comment
                    .author
                    .map(|a| a.login)
                    .unwrap_or_else(|| "ghost".into()),
                body: excerpt(&comment.body_text.unwrap_or_default()),
                url,
                path,
                is_outdated: thread.is_outdated,
            });
        }
    }

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
    })
}

/// Max characters kept from a review-thread comment's first line for display.
const COMMENT_EXCERPT_MAX: usize = 60;

/// A short, single-line excerpt of a review comment body: the first non-empty
/// line, trimmed, char-truncated to [`COMMENT_EXCERPT_MAX`] with a trailing `…`.
fn excerpt(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() <= COMMENT_EXCERPT_MAX {
        return line.to_string();
    }
    let mut s: String = line.chars().take(COMMENT_EXCERPT_MAX - 1).collect();
    s.push('…');
    s
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
    #[serde(rename = "reviewThreads")]
    review_threads: Option<ReviewThreads>,
}

#[derive(Deserialize)]
struct ReviewThreads {
    nodes: Vec<ReviewThreadNode>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ReviewThreadNode {
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
    login: String,
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
                    "isResolved": true,
                    "isOutdated": false,
                    "comments": { "nodes": [
                        { "author": { "login": "resolved-guy" }, "bodyText": "done", "url": "https://x/1", "path": "src/a.rs" }
                    ]}
                },
                {
                    "isResolved": false,
                    "isOutdated": false,
                    "comments": { "nodes": [
                        { "author": { "login": "carol" }, "bodyText": "add a test here", "url": "https://x/2", "path": "src/foo.rs" }
                    ]}
                },
                {
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
        assert_eq!(carol.path.as_deref(), Some("src/foo.rs"));
        assert!(!carol.is_outdated);

        let dave = &pr.unresolved_comments[1];
        assert_eq!(dave.author, "dave");
        assert!(dave.is_outdated);
        // Empty path collapses to None.
        assert_eq!(dave.path, None);
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
    fn excerpt_takes_first_nonempty_line_and_truncates() {
        assert_eq!(excerpt(""), "");
        assert_eq!(excerpt("\n\n  hello  \nworld"), "hello");
        let long = "x".repeat(100);
        let e = excerpt(&long);
        assert_eq!(e.chars().count(), COMMENT_EXCERPT_MAX);
        assert!(e.ends_with('…'));
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
