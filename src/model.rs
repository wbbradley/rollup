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

/// The first comment of an unresolved review thread on a PR. Surfaced in the
/// Authored pane's "Open comments" section.
#[derive(Debug, Clone)]
pub struct PrComment {
    pub author: String,
    /// First line / short excerpt of the comment body.
    pub body: String,
    /// Direct comment permalink (…#discussion_r…).
    pub url: String,
    /// Inline file path for a code comment; `None` for a file-level comment.
    pub path: Option<String>,
    /// The thread's diff hunk moved or collapsed ("orphaned"); tagged
    /// `[outdated]` in the UI.
    pub is_outdated: bool,
}

#[derive(Debug, Clone)]
pub struct Pr {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub is_draft: bool,
    pub repo: String,
    /// The branch this PR merges into (GitHub `baseRefName`). Its merge target;
    /// used to build the stacked-PR tree in the Authored pane.
    pub base_ref: String,
    /// The branch this PR is built from (GitHub `headRefName`). A PR is the
    /// parent of any PR whose `base_ref` equals this `head_ref`.
    pub head_ref: String,
    pub author: String,
    pub reviewers: Vec<ReviewerStatus>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// `Some(_)` iff the PR is merged. Open PRs leave this `None`.
    pub merged_at: Option<chrono::DateTime<chrono::Utc>>,
    /// First comment of each unresolved review thread. Only populated for the
    /// Authored pane's PRs (the reviewing/merged fetches don't request threads).
    pub unresolved_comments: Vec<PrComment>,
}

/// Group by repo: repos sorted alphabetically at the top level, and within
/// each repo PRs sorted newest-updated first.
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
        // Non-drafts first, then newest-updated within each bucket.
        prs.sort_by(|a, b| {
            a.is_draft
                .cmp(&b.is_draft)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
    }
    groups.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    groups
}

/// A node in the Authored pane's merge-target tree: a PR plus its stacked
/// children (PRs that target this PR's head branch). Produced by
/// [`authored_tree`].
#[derive(Debug)]
pub struct PrTreeNode<'a> {
    pub pr: &'a Pr,
    pub children: Vec<PrTreeNode<'a>>,
}

/// Build the merge-target tree for a single repo's authored PRs. Returns the
/// roots, each carrying its stacked children nested via `children`.
///
/// `prs` is expected to already be sorted the way the Authored pane sorts (see
/// [`group_by_repo`]: non-drafts first, then newest-updated); sibling order in
/// the output preserves the input order.
///
/// Parenting: a PR *C* is a child of PR *P* iff `P.head_ref == C.base_ref`.
/// PRs whose `base_ref` is not the `head_ref` of any PR here are roots. When
/// two PRs share a `head_ref` (unusual), the first in the sorted input wins, so
/// the result is deterministic. Self-parenting (`base_ref == head_ref`) is
/// ignored, and any PRs caught in a reference cycle are defensively promoted to
/// roots so the walk always terminates.
pub fn authored_tree<'a>(prs: &[&'a Pr]) -> Vec<PrTreeNode<'a>> {
    // Map each non-empty head ref to the first PR that declares it.
    let mut head_to_idx: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, pr) in prs.iter().enumerate() {
        if !pr.head_ref.is_empty() {
            head_to_idx.entry(pr.head_ref.as_str()).or_insert(i);
        }
    }

    // Resolve each PR's parent by its base ref, skipping self-parents.
    let parent: Vec<Option<usize>> = prs
        .iter()
        .enumerate()
        .map(|(i, pr)| {
            if pr.base_ref.is_empty() {
                return None;
            }
            match head_to_idx.get(pr.base_ref.as_str()) {
                Some(&p) if p != i => Some(p),
                _ => None,
            }
        })
        .collect();

    // Children lists and root list, both in input order (preserves the sort).
    let mut children_idx: Vec<Vec<usize>> = vec![Vec::new(); prs.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (i, p) in parent.iter().enumerate() {
        match p {
            Some(p) => children_idx[*p].push(i),
            None => roots.push(i),
        }
    }

    let mut visited: Vec<bool> = vec![false; prs.len()];
    let mut out = build_nodes(&roots, &children_idx, prs, &mut visited);

    // Cycle guard: any PR not reached from a root is part of a reference cycle
    // (e.g. A→B→A). Promote each such PR to a root, in input order, so it still
    // appears exactly once and the walk terminates.
    for i in 0..prs.len() {
        if !visited[i] {
            out.extend(build_nodes(&[i], &children_idx, prs, &mut visited));
        }
    }

    out
}

fn build_nodes<'a>(
    sibs: &[usize],
    children_idx: &[Vec<usize>],
    prs: &[&'a Pr],
    visited: &mut [bool],
) -> Vec<PrTreeNode<'a>> {
    let mut out: Vec<PrTreeNode<'a>> = Vec::new();
    for &idx in sibs {
        if visited[idx] {
            continue;
        }
        visited[idx] = true;
        let children = build_nodes(&children_idx[idx], children_idx, prs, visited);
        out.push(PrTreeNode {
            pr: prs[idx],
            children,
        });
    }
    out
}

#[derive(Debug)]
pub struct PersonGroup<'a> {
    pub login: String,
    pub authored: Vec<&'a Pr>,
    pub reviewing: Vec<&'a Pr>,
}

/// Build the People-mode grouping. See PLAN "Add People-pivot view" for the
/// contract. Excludes the viewer and Team-kind reviewers. Inclusion: a login
/// appears iff they are either `pr.author` or a User-kind requested reviewer
/// on any PR in the union of `authored` and `reviewing` (deduped by
/// `(repo, number)`). Per-person: `authored` collects union PRs authored by
/// the login; `reviewing` collects union PRs where the login is a requested
/// User reviewer and is not already under `authored`. Within each sub-group,
/// non-drafts come first, then newest-updated. Top-level sort is login
/// case-insensitive alphabetical.
pub fn group_by_person<'a>(
    authored: &'a [Pr],
    reviewing: &'a [Pr],
    viewer: &str,
) -> Vec<PersonGroup<'a>> {
    // Union of the two input slices, deduped by (repo, number). Keep the
    // first reference seen so lifetimes stay tied to the input slices.
    let mut seen: std::collections::HashSet<(&str, u64)> = std::collections::HashSet::new();
    let mut union: Vec<&'a Pr> = Vec::new();
    for pr in authored.iter().chain(reviewing.iter()) {
        if seen.insert((pr.repo.as_str(), pr.number)) {
            union.push(pr);
        }
    }

    let viewer_lc = viewer.to_ascii_lowercase();
    let is_viewer = |login: &str| login.to_ascii_lowercase() == viewer_lc;

    // Collect candidate logins, keyed by lowercased login so we dedupe across
    // case variants but preserve a stable display form (first seen).
    let mut candidates: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let note = |login: &str, candidates: &mut std::collections::BTreeMap<String, String>| {
        if is_viewer(login) {
            return;
        }
        candidates
            .entry(login.to_ascii_lowercase())
            .or_insert_with(|| login.to_string());
    };
    for pr in &union {
        note(&pr.author, &mut candidates);
        for r in &pr.reviewers {
            if r.kind == ReviewerKind::User && r.requested {
                note(&r.login, &mut candidates);
            }
        }
    }

    let sort_prs = |prs: &mut Vec<&'a Pr>| {
        prs.sort_by(|a, b| {
            a.is_draft
                .cmp(&b.is_draft)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
    };

    let mut groups: Vec<PersonGroup<'a>> = Vec::new();
    for (login_lc, display) in &candidates {
        let mut authored_vec: Vec<&'a Pr> = Vec::new();
        let mut authored_keys: std::collections::HashSet<(&str, u64)> =
            std::collections::HashSet::new();
        for pr in &union {
            if pr.author.to_ascii_lowercase() == *login_lc
                && authored_keys.insert((pr.repo.as_str(), pr.number))
            {
                authored_vec.push(pr);
            }
        }

        let mut reviewing_vec: Vec<&'a Pr> = Vec::new();
        let mut reviewing_keys: std::collections::HashSet<(&str, u64)> =
            std::collections::HashSet::new();
        for pr in &union {
            if authored_keys.contains(&(pr.repo.as_str(), pr.number)) {
                continue;
            }
            let is_requested = pr.reviewers.iter().any(|r| {
                r.kind == ReviewerKind::User
                    && r.requested
                    && r.login.to_ascii_lowercase() == *login_lc
            });
            if is_requested && reviewing_keys.insert((pr.repo.as_str(), pr.number)) {
                reviewing_vec.push(pr);
            }
        }

        if authored_vec.is_empty() && reviewing_vec.is_empty() {
            continue;
        }
        sort_prs(&mut authored_vec);
        sort_prs(&mut reviewing_vec);
        groups.push(PersonGroup {
            login: display.clone(),
            authored: authored_vec,
            reviewing: reviewing_vec,
        });
    }

    groups.sort_by(|a, b| {
        a.login
            .to_ascii_lowercase()
            .cmp(&b.login.to_ascii_lowercase())
    });
    groups
}

/// Lowercased, deduped authors visible in Me mode: the viewer plus every
/// distinct author of a PR in `reviewing`. The Authored pane is always the
/// viewer, so it contributes no extra logins.
pub fn authors_for_me(viewer: &str, reviewing: &[Pr]) -> Vec<String> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    let push =
        |login: &str, seen: &mut std::collections::BTreeSet<String>, out: &mut Vec<String>| {
            let lc = login.to_ascii_lowercase();
            if seen.insert(lc.clone()) {
                out.push(lc);
            }
        };
    push(viewer, &mut seen, &mut out);
    for pr in reviewing {
        push(&pr.author, &mut seen, &mut out);
    }
    out
}

/// Every person surfaced by the People view as lowercased login strings.
/// Mirrors the set that `group_by_person` materializes (authors + User-kind
/// requested reviewers, excluding viewer and Teams).
pub fn authors_for_people(authored: &[Pr], reviewing: &[Pr], viewer: &str) -> Vec<String> {
    group_by_person(authored, reviewing, viewer)
        .into_iter()
        .map(|g| g.login.to_ascii_lowercase())
        .collect()
}

/// Union of the Me-mode and People-mode author sets, for the single merged-PR
/// fetch that feeds both views. `authors_for_people` excludes the viewer
/// by design, so this pulls the viewer and reviewing-pane authors back in.
/// Lowercased, deduped; order is Me-set first (viewer, then reviewing
/// authors), then any extra People-set logins.
pub fn merged_fetch_authors(viewer: &str, authored: &[Pr], reviewing: &[Pr]) -> Vec<String> {
    let mut out: Vec<String> = authors_for_me(viewer, reviewing);
    let mut seen: std::collections::BTreeSet<String> = out.iter().cloned().collect();
    for a in authors_for_people(authored, reviewing, viewer) {
        if seen.insert(a.clone()) {
            out.push(a);
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub tag_name: String,
    /// Display name; falls back to `tag_name` when the release has no name.
    pub name: Option<String>,
    pub url: String,
    /// Prefer `publishedAt`, fall back to `createdAt`.
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub is_prerelease: bool,
}

#[derive(Debug, Clone)]
pub struct TagInfo {
    pub name: String,
    pub committed_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct RepoReleaseInfo {
    /// `owner/name`.
    pub repo: String,
    /// Up to N most recent releases, newest first.
    pub recent_releases: Vec<ReleaseInfo>,
    pub latest_tag: Option<TagInfo>,
}

/// Short human-friendly age string (seconds, minutes, hours, days, weeks,
/// months, years — first unit that fits). Values clamp to zero for future
/// timestamps.
pub fn human_age(dt: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> String {
    let delta = (now - dt).num_seconds().max(0);
    match delta {
        d if d < 60 => format!("{d}s"),
        d if d < 3_600 => format!("{}m", d / 60),
        d if d < 86_400 => format!("{}h", d / 3_600),
        d if d < 7 * 86_400 => format!("{}d", d / 86_400),
        d if d < 30 * 86_400 => format!("{}w", d / (7 * 86_400)),
        d if d < 365 * 86_400 => format!("{}mo", d / (30 * 86_400)),
        d => format!("{}y", d / (365 * 86_400)),
    }
}

/// Sort by `merged_at` descending (open PRs with `None` are dropped), take up
/// to `cap` entries.
pub fn recent_merged(prs: &[Pr], cap: usize) -> Vec<&Pr> {
    let mut merged: Vec<&Pr> = prs.iter().filter(|p| p.merged_at.is_some()).collect();
    merged.sort_by(|a, b| b.merged_at.cmp(&a.merged_at));
    merged.truncate(cap);
    merged
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

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
        }
    }

    /// Like `pr`, but with explicit merge-target (`base`) and source (`head`)
    /// branches so tests can wire up stacked-PR relationships.
    fn pr_refs(
        repo: &str,
        number: u64,
        base: &str,
        head: &str,
        is_draft: bool,
        updated: i64,
    ) -> Pr {
        Pr {
            base_ref: base.to_string(),
            head_ref: head.to_string(),
            ..pr(repo, number, "me", is_draft, updated, vec![])
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
    fn group_by_person_excludes_viewer_and_teams() {
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
        let groups = group_by_person(&authored, &reviewing, viewer);
        let logins: Vec<&str> = groups.iter().map(|g| g.login.as_str()).collect();
        assert!(!logins.contains(&viewer));
        assert!(!logins.iter().any(|l| l.starts_with('@')));
        assert!(logins.contains(&"alice"));
        assert!(logins.contains(&"bob"));
    }

    #[test]
    fn group_by_person_dedupes_and_sorts() {
        let viewer = "me";
        // Same PR shows up in both slices: once as viewer-authored, once as
        // review-requested. Should appear exactly once, under its author.
        let shared = pr(
            "o/r",
            1,
            "Bob",
            false,
            100,
            vec![user("alice", true), user(viewer, true)],
        );
        let also_alice_draft = pr("o/r", 2, "alice", true, 300, vec![]);
        let also_alice_ready = pr("o/r", 3, "alice", false, 200, vec![]);
        let authored = vec![shared.clone(), also_alice_draft, also_alice_ready];
        let reviewing = vec![shared];

        let groups = group_by_person(&authored, &reviewing, viewer);
        let logins: Vec<&str> = groups.iter().map(|g| g.login.as_str()).collect();
        // Case-insensitive alphabetical: "alice" then "Bob".
        assert_eq!(logins, vec!["alice", "Bob"]);

        let alice = &groups[0];
        // Alice's authored: her own PRs — non-draft (#3) before draft (#2).
        let authored_nums: Vec<u64> = alice.authored.iter().map(|p| p.number).collect();
        assert_eq!(authored_nums, vec![3, 2]);
        // Alice is a requested reviewer on #1, so it appears exactly once in
        // her `reviewing` list (even though it's in both input slices).
        let reviewing_nums: Vec<u64> = alice.reviewing.iter().map(|p| p.number).collect();
        assert_eq!(reviewing_nums, vec![1]);

        let bob = &groups[1];
        // Bob authored #1; should appear exactly once despite being in both
        // input slices.
        let bob_authored_nums: Vec<u64> = bob.authored.iter().map(|p| p.number).collect();
        assert_eq!(bob_authored_nums, vec![1]);
        assert!(bob.reviewing.is_empty());
    }

    #[test]
    fn recent_merged_sorts_by_merged_at_desc_and_caps() {
        let prs = vec![
            merged_pr("o/r", 1, "a", 100),
            merged_pr("o/r", 2, "b", 300),
            merged_pr("o/r", 3, "c", 200),
            merged_pr("o/r", 4, "d", 400),
        ];
        let out = recent_merged(&prs, 3);
        let nums: Vec<u64> = out.iter().map(|p| p.number).collect();
        assert_eq!(nums, vec![4, 2, 3]);
    }

    #[test]
    fn recent_merged_skips_open_prs() {
        let prs = vec![
            pr("o/r", 1, "a", false, 100, vec![]),
            merged_pr("o/r", 2, "b", 200),
            pr("o/r", 3, "c", false, 300, vec![]),
        ];
        let out = recent_merged(&prs, 10);
        let nums: Vec<u64> = out.iter().map(|p| p.number).collect();
        assert_eq!(nums, vec![2]);
    }

    #[test]
    fn authors_for_me_includes_viewer_and_reviewing_authors() {
        let viewer = "Me";
        let reviewing = vec![
            pr("o/r", 1, "Alice", false, 100, vec![]),
            pr("o/r", 2, "bob", false, 200, vec![]),
            pr("o/r", 3, "ALICE", false, 300, vec![]),
        ];
        let out = authors_for_me(viewer, &reviewing);
        // All lowercased, deduped, viewer included.
        let set: std::collections::BTreeSet<&str> = out.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            set,
            ["me", "alice", "bob"]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
        );
    }

    #[test]
    fn authors_for_people_matches_group_by_person_logins() {
        let viewer = "me";
        let authored = vec![pr(
            "o/r",
            1,
            viewer,
            false,
            100,
            vec![user("Alice", true), team("@core", true)],
        )];
        let reviewing = vec![pr(
            "o/r",
            2,
            "Bob",
            false,
            200,
            vec![user(viewer, true), user("Carol", true)],
        )];
        let from_helper: std::collections::BTreeSet<String> =
            authors_for_people(&authored, &reviewing, viewer)
                .into_iter()
                .collect();
        let from_group: std::collections::BTreeSet<String> =
            group_by_person(&authored, &reviewing, viewer)
                .into_iter()
                .map(|g| g.login.to_ascii_lowercase())
                .collect();
        assert_eq!(from_helper, from_group);
        // Sanity check: viewer excluded, teams excluded, everyone else in.
        assert!(!from_helper.contains("me"));
        assert!(!from_helper.iter().any(|l| l.starts_with('@')));
        assert!(from_helper.contains("alice"));
        assert!(from_helper.contains("bob"));
        assert!(from_helper.contains("carol"));
    }

    #[test]
    fn merged_fetch_authors_includes_viewer_and_people_set() {
        let viewer = "Me";
        let authored = vec![pr(
            "o/r",
            1,
            viewer,
            false,
            100,
            vec![user("Alice", true), team("@core", true)],
        )];
        let reviewing = vec![pr("o/r", 2, "Bob", false, 200, vec![user("Carol", true)])];
        let out = merged_fetch_authors(viewer, &authored, &reviewing);
        let set: std::collections::BTreeSet<&str> = out.iter().map(String::as_str).collect();
        // Viewer must be present (the People set alone would omit it).
        assert!(set.contains("me"));
        // Reviewing author is present.
        assert!(set.contains("bob"));
        // People-set members not otherwise surfaced come through too.
        assert!(set.contains("alice"));
        assert!(set.contains("carol"));
        // Teams never appear.
        assert!(!set.iter().any(|l| l.starts_with('@')));
        // Everything is lowercased.
        assert!(out.iter().all(|s| s == &s.to_ascii_lowercase()));
    }

    #[test]
    fn human_age_buckets() {
        let now = ts(10_000_000_000);
        let mk = |secs: i64| human_age(ts(10_000_000_000 - secs), now);
        assert_eq!(mk(30), "30s");
        assert_eq!(mk(5 * 60), "5m");
        assert_eq!(mk(3 * 3600), "3h");
        assert_eq!(mk(2 * 86_400), "2d");
        // Weeks bucket runs [7d, 30d). 3w = 21d lands inside it.
        assert_eq!(mk(3 * 7 * 86_400), "3w");
        assert_eq!(mk(4 * 30 * 86_400), "4mo");
        assert_eq!(mk(3 * 365 * 86_400), "3y");
    }

    #[test]
    fn human_age_clamps_future() {
        let now = ts(1_000);
        assert_eq!(human_age(ts(2_000), now), "0s");
    }

    #[test]
    fn group_by_person_skips_non_requested_reviewers() {
        let viewer = "me";
        // carol already reviewed (requested=false) and doesn't author anything
        // => must NOT appear as a top-level person.
        let authored = vec![pr(
            "o/r",
            1,
            viewer,
            false,
            100,
            vec![user("carol", false), user("dave", true)],
        )];
        let reviewing = vec![];
        let groups = group_by_person(&authored, &reviewing, viewer);
        let logins: Vec<&str> = groups.iter().map(|g| g.login.as_str()).collect();
        assert!(!logins.contains(&"carol"));
        assert!(logins.contains(&"dave"));
    }

    /// Flatten a tree to `(pr_number, depth)` pairs in pre-order.
    fn flatten(nodes: &[PrTreeNode], depth: usize, out: &mut Vec<(u64, usize)>) {
        for n in nodes {
            out.push((n.pr.number, depth));
            flatten(&n.children, depth + 1, out);
        }
    }

    #[test]
    fn authored_tree_linear_stack() {
        // C targets B's branch, B targets A's branch, A targets main.
        let a = pr_refs("o/r", 1, "main", "a", false, 100);
        let b = pr_refs("o/r", 2, "a", "b", false, 100);
        let c = pr_refs("o/r", 3, "b", "c", false, 100);
        // Input order is arbitrary; the tree must still nest A→B→C.
        let prs: Vec<&Pr> = vec![&c, &a, &b];
        let tree = authored_tree(&prs);
        // One root (A), then a single child chain via `children`.
        assert_eq!(tree.len(), 1);
        let mut flat = Vec::new();
        flatten(&tree, 0, &mut flat);
        assert_eq!(flat, vec![(1, 0), (2, 1), (3, 2)]);
    }

    #[test]
    fn authored_tree_roots_target_main() {
        let a = pr_refs("o/r", 1, "main", "a", false, 200);
        let b = pr_refs("o/r", 2, "main", "b", false, 100);
        let prs: Vec<&Pr> = vec![&a, &b];
        let tree = authored_tree(&prs);
        // Two roots, no children, input order preserved.
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].pr.number, 1);
        assert_eq!(tree[1].pr.number, 2);
        assert!(tree.iter().all(|n| n.children.is_empty()));
    }

    #[test]
    fn authored_tree_sibling_order() {
        // Parent A with two children (both target A's branch). Sibling order
        // follows input order.
        let a = pr_refs("o/r", 1, "main", "a", false, 300);
        let c1 = pr_refs("o/r", 2, "a", "c1", false, 200);
        let c2 = pr_refs("o/r", 3, "a", "c2", false, 100);
        let prs: Vec<&Pr> = vec![&a, &c1, &c2];
        let tree = authored_tree(&prs);
        assert_eq!(tree.len(), 1);
        let root = &tree[0];
        assert_eq!(root.pr.number, 1);
        let child_nums: Vec<u64> = root.children.iter().map(|n| n.pr.number).collect();
        assert_eq!(child_nums, vec![2, 3]);
        assert!(root.children.iter().all(|n| n.children.is_empty()));
    }

    #[test]
    fn authored_tree_cycle_guard() {
        // A targets B's branch and B targets A's branch: a 2-cycle with no
        // root. Both must still appear exactly once and the call must return.
        let a = pr_refs("o/r", 1, "b", "a", false, 200);
        let b = pr_refs("o/r", 2, "a", "b", false, 100);
        let prs: Vec<&Pr> = vec![&a, &b];
        let tree = authored_tree(&prs);
        let mut flat = Vec::new();
        flatten(&tree, 0, &mut flat);
        let mut nums: Vec<u64> = flat.iter().map(|(n, _)| *n).collect();
        nums.sort_unstable();
        assert_eq!(nums, vec![1, 2]);
    }

    #[test]
    fn authored_tree_self_parent_is_root() {
        // A PR whose base and head are the same branch must not parent itself.
        let a = pr_refs("o/r", 1, "x", "x", false, 100);
        let prs: Vec<&Pr> = vec![&a];
        let tree = authored_tree(&prs);
        assert_eq!(tree.len(), 1);
        assert!(tree[0].children.is_empty());
    }
}
