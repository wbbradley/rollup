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
    /// `Some(_)` iff the PR is merged. Open PRs leave this `None`.
    pub merged_at: Option<chrono::DateTime<chrono::Utc>>,
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
}
