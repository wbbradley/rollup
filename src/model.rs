#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    /// This reviewer has never submitted a review. They're on the list purely
    /// because GitHub has them in `reviewRequests`.
    NoReview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewerKind {
    User,
    Team,
}

#[derive(Debug, Clone)]
pub struct ReviewerStatus {
    pub login: String,
    pub kind: ReviewerKind,
    pub state: ReviewState,
    /// True iff this reviewer currently appears in the PR's `reviewRequests`.
    /// Only requested reviewers can be removed via the REST API; reviewers
    /// whose only appearance is a submitted review are stuck there until they
    /// dismiss or the review itself is dismissed.
    pub requested: bool,
}

#[derive(Debug, Clone)]
pub struct Pr {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub is_draft: bool,
    pub repo: String,
    pub author: String,
    pub reviewers: Vec<ReviewerStatus>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Group by repo, newest-updated PR first within each group, and order the
/// repo groups themselves by the freshness of their most-recently-updated PR
/// (so a repo where something just landed bubbles to the top).
pub fn group_by_repo(prs: &[Pr]) -> Vec<(String, Vec<&Pr>)> {
    let mut groups: Vec<(String, Vec<&Pr>)> = Vec::new();
    for pr in prs {
        if let Some(entry) = groups.iter_mut().find(|(repo, _)| repo == &pr.repo) {
            entry.1.push(pr);
        } else {
            groups.push((pr.repo.clone(), vec![pr]));
        }
    }
    for (_, prs) in groups.iter_mut() {
        prs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    }
    groups.sort_by(|a, b| {
        let a_newest = a.1.first().map(|p| p.updated_at);
        let b_newest = b.1.first().map(|p| p.updated_at);
        b_newest.cmp(&a_newest)
    });
    groups
}
