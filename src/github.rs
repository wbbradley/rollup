use std::{collections::HashMap, process::Command};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::{
    config,
    model::{
        self,
        Pr,
        ReleaseInfo,
        RepoReleaseInfo,
        ReviewState,
        ReviewerKind,
        ReviewerStatus,
        TagInfo,
    },
};

const QUERY: &str = r#"
query {
  viewer { login }
  authored: search(query: "is:pr is:open author:@me archived:false", type: ISSUE, first: 100) {
    nodes { ...PrFields }
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
    let (viewer, authored, reviewing) = fetch_open()?;
    // One fetch feeds both Me and People views; the render layer filters per
    // view. The fetch set must cover BOTH — `authors_for_people` excludes the
    // viewer, so on its own it would hide the viewer's own merged PRs in Me
    // mode. Take the union.
    let authors = model::merged_fetch_authors(&viewer, &authored, &reviewing);
    let merged = if authors.is_empty() {
        Vec::new()
    } else {
        fetch_merged(&authors, MERGED_AUTHOR_CAP)?
    };
    let releases = if config.repos.is_empty() {
        Vec::new()
    } else {
        fetch_releases(&config.repos)?
    };
    Ok(Data {
        viewer,
        authored,
        reviewing,
        merged,
        releases,
        config_error,
    })
}

fn fetch_open() -> Result<(String, Vec<Pr>, Vec<Pr>)> {
    let output = Command::new("gh")
        .args(["api", "graphql", "-f"])
        .arg(format!("query={QUERY}"))
        .output()
        .context("failed to invoke gh; is it installed and on PATH?")?;
    if !output.status.success() {
        return Err(anyhow!(
            "gh api graphql failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let root: OpenRoot =
        serde_json::from_slice(&output.stdout).context("parsing gh response JSON")?;
    let authored = root
        .data
        .authored
        .nodes
        .into_iter()
        .filter_map(node_to_pr)
        .collect();
    let reviewing = root
        .data
        .reviewing
        .nodes
        .into_iter()
        .filter_map(node_to_pr)
        .collect();
    Ok((root.data.viewer.login, authored, reviewing))
}

fn fetch_merged(authors: &[String], cap: usize) -> Result<Vec<Pr>> {
    let clauses: Vec<String> = authors
        .iter()
        .take(MERGED_AUTHOR_CAP)
        .map(|a| format!("author:{a}"))
        .collect();
    if clauses.is_empty() {
        return Ok(Vec::new());
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
    if !output.status.success() {
        return Err(anyhow!(
            "gh api graphql (merged) failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let root: MergedRoot =
        serde_json::from_slice(&output.stdout).context("parsing gh merged response JSON")?;
    let all: Vec<Pr> = root
        .data
        .merged
        .nodes
        .into_iter()
        .filter_map(node_to_pr)
        .filter(|p| p.merged_at.is_some())
        .collect();
    Ok(model::recent_merged(&all, cap)
        .into_iter()
        .cloned()
        .collect())
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

    reviewers.sort_by(|a, b| a.login.to_lowercase().cmp(&b.login.to_lowercase()));

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

    Some(Pr {
        number,
        title: node.title.unwrap_or_default(),
        url: node.url.unwrap_or_default(),
        is_draft: node.is_draft.unwrap_or(false),
        repo,
        author: node
            .author
            .map(|a| a.login)
            .unwrap_or_else(|| "ghost".into()),
        reviewers,
        updated_at,
        merged_at,
    })
}

#[derive(Deserialize)]
struct OpenRoot {
    data: OpenDataResp,
}

#[derive(Deserialize)]
struct OpenDataResp {
    viewer: Viewer,
    authored: SearchResp,
    reviewing: SearchResp,
}

#[derive(Deserialize)]
struct MergedRoot {
    data: MergedDataResp,
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
    nodes: Vec<PrNode>,
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
    repository: Option<RepoNode>,
    author: Option<AuthorNode>,
    #[serde(rename = "reviewRequests")]
    review_requests: Option<ReviewRequests>,
    #[serde(rename = "latestReviews")]
    latest_reviews: Option<LatestReviews>,
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

fn fetch_releases(repos: &[config::RepoRef]) -> Result<Vec<RepoReleaseInfo>> {
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
        .context("failed to invoke gh")?;
    if !output.status.success() {
        return Err(anyhow!(
            "gh api graphql (releases) failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let root: ReleasesRoot =
        serde_json::from_slice(&output.stdout).context("parsing gh releases response JSON")?;
    let mut out = Vec::with_capacity(repos.len());
    for (i, r) in repos.iter().enumerate() {
        let key = format!("r{i}");
        let node = root.data.aliases.get(&key).and_then(|v| v.as_ref());
        out.push(node_to_repo_release_info(r, node));
    }
    Ok(out)
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
struct ReleasesRoot {
    data: ReleasesData,
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
}
