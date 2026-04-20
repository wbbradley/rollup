use std::process::Command;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::model::{self, Pr, ReviewState, ReviewerKind, ReviewerStatus};

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
    Ok(Data {
        viewer,
        authored,
        reviewing,
        merged,
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
