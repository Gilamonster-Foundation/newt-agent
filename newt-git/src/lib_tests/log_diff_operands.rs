use super::*;

/// `log`'s `revision` param starts the walk somewhere other than HEAD.
#[test]
fn log_revision_single_rev_starts_walk_there() {
    let (dir, ids) = repo_with_history();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let log = eng
        .log(&GitCaveats::top(), 10, Some("HEAD~1"), &[])
        .unwrap();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0].id, ids[1], "starts at c2, not HEAD (c3)");
    assert_eq!(log[1].id, ids[0]);
}

/// `log`'s `revision` param also accepts an `A..B` range: first-parent walk
/// from `B`, stopping BEFORE `A` — matching real `git log A..B`'s commit set
/// for a linear first-parent history.
#[test]
fn log_revision_range_excludes_left_bound() {
    let (dir, ids) = repo_with_history();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let log = eng
        .log(&GitCaveats::top(), 10, Some("HEAD~2..HEAD"), &[])
        .unwrap();
    let want: Vec<String> = String::from_utf8(
        git_cmd(dir.path())
            .args(["log", "--first-parent", "--format=%H", "HEAD~2..HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .lines()
    .map(str::to_string)
    .collect();
    let got: Vec<String> = log.iter().map(|c| c.id.clone()).collect();
    assert_eq!(got, want, "matches real git's commit set for the range");
    assert_eq!(got, vec![ids[2].clone(), ids[1].clone()]);
}

/// `log`'s `paths` param keeps only commits whose diff touches a match —
/// `c2` (adds `b.txt`) is excluded when filtering on `a.txt`.
#[test]
fn log_paths_filters_commits_that_did_not_touch_the_pathspec() {
    let (dir, ids) = repo_with_history();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let log = eng
        .log(&GitCaveats::top(), 10, None, &["a.txt".to_string()])
        .unwrap();
    let got: Vec<String> = log.iter().map(|c| c.id.clone()).collect();
    assert_eq!(got, vec![ids[2].clone(), ids[0].clone()], "{log:?}");
}

/// `diff` with `DiffSpec::Rev` diffs a commit's tree against the worktree.
#[test]
fn diff_rev_vs_worktree() {
    let (dir, ids) = repo_with_history();
    std::fs::write(dir.path().join("a.txt"), "worktree edit\n").unwrap();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let d = eng
        .diff(
            &GitCaveats::top(),
            DiffSpec::Rev(ids[0].clone()),
            &[],
            false,
        )
        .unwrap();
    // Against c1 (only a.txt existed), the worktree now also carries b.txt
    // (added in c2) and a.txt has a further uncommitted edit.
    let paths: Vec<&str> = d.files.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.contains(&"a.txt"), "{d:?}");
    assert!(paths.contains(&"b.txt"), "{d:?}");
}

/// `diff` with `DiffSpec::RevRange` diffs two commits' trees.
#[test]
fn diff_rev_range_matches_real_git_diff_stat_shape() {
    let (dir, ids) = repo_with_history();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let d = eng
        .diff(
            &GitCaveats::top(),
            DiffSpec::RevRange(ids[0].clone(), ids[2].clone()),
            &[],
            false,
        )
        .unwrap();
    let mut got: Vec<&str> = d.files.iter().map(|f| f.path.as_str()).collect();
    got.sort_unstable();
    assert_eq!(got, vec!["a.txt", "b.txt"], "{d:?}");
}

/// `DiffSpec::Rev` also accepts an `A..B` spec, equivalent to `RevRange`.
#[test]
fn diff_rev_accepts_an_embedded_range() {
    let (dir, ids) = repo_with_history();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let range = eng
        .diff(
            &GitCaveats::top(),
            DiffSpec::Rev(format!("{}..{}", ids[0], ids[2])),
            &[],
            false,
        )
        .unwrap();
    let explicit = eng
        .diff(
            &GitCaveats::top(),
            DiffSpec::RevRange(ids[0].clone(), ids[2].clone()),
            &[],
            false,
        )
        .unwrap();
    assert_eq!(range.files, explicit.files);
}

/// `diff`'s `paths` param filters the result to matching pathspecs.
#[test]
fn diff_paths_filters_to_the_pathspec() {
    let (dir, ids) = repo_with_history();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let d = eng
        .diff(
            &GitCaveats::top(),
            DiffSpec::RevRange(ids[0].clone(), ids[2].clone()),
            &["b.txt".to_string()],
            false,
        )
        .unwrap();
    assert_eq!(d.files.len(), 1, "{d:?}");
    assert_eq!(d.files[0].path, "b.txt");
}

/// `diff`'s `stat` flag reports insertion/deletion counts matching real
/// `git diff --shortstat`'s totals for the same range.
#[test]
fn diff_stat_matches_real_git_diff_numstat_totals() {
    let (dir, ids) = repo_with_history();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let d = eng
        .diff(
            &GitCaveats::top(),
            DiffSpec::RevRange(ids[0].clone(), ids[2].clone()),
            &[],
            true,
        )
        .unwrap();
    let stat = d.stat.expect("stat requested");
    let got_insertions: usize = stat.iter().map(|s| s.insertions).sum();
    let got_deletions: usize = stat.iter().map(|s| s.deletions).sum();

    let numstat = String::from_utf8(
        git_cmd(dir.path())
            .args(["diff", "--numstat", &ids[0], &ids[2]])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let (mut want_insertions, mut want_deletions) = (0usize, 0usize);
    for line in numstat.lines() {
        let mut cols = line.split_whitespace();
        want_insertions += cols.next().unwrap().parse::<usize>().unwrap();
        want_deletions += cols.next().unwrap().parse::<usize>().unwrap();
    }
    assert_eq!(got_insertions, want_insertions);
    assert_eq!(got_deletions, want_deletions);
}

/// Without `stat: true`, `DiffReport::stat` stays `None` — the field is
/// opt-in, not a default cost on every diff call.
#[test]
fn diff_stat_defaults_to_none() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let d = eng
        .diff(&GitCaveats::top(), DiffSpec::Worktree, &[], false)
        .unwrap();
    assert!(d.stat.is_none());
}

/// `A..B` is reachability, not a first-parent-chain scan: fork a branch from
/// `main`, advance `main` past the fork, then commit on the branch. Real
/// `git log main..HEAD` (the most common shape #2520 exists for) must not
/// walk past the fork into main's post-fork history.
#[test]
fn log_revision_range_is_reachability_not_first_parent_scan() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    std::fs::write(p.join("a.txt"), "1\n").unwrap();
    git(p, &["add", "a.txt"]);
    git(p, &["commit", "-q", "-m", "root"]);
    git(p, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(p.join("b.txt"), "topic\n").unwrap();
    git(p, &["add", "b.txt"]);
    git(p, &["commit", "-q", "-m", "topic commit"]);
    let topic_head = rev_parse(p, "HEAD");
    git(p, &["checkout", "-q", "main"]);
    std::fs::write(p.join("c.txt"), "main advanced\n").unwrap();
    git(p, &["add", "c.txt"]);
    git(p, &["commit", "-q", "-m", "main advances past the fork"]);
    git(p, &["checkout", "-q", "topic"]);

    let want: Vec<String> = String::from_utf8(
        git_cmd(p)
            .args(["log", "--format=%H", "main..HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .lines()
    .map(str::to_string)
    .collect();
    assert_eq!(
        want,
        vec![topic_head],
        "fixture sanity: main..HEAD is exactly the topic commit"
    );

    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let log = eng
        .log(&GitCaveats::top(), 10, Some("main..HEAD"), &[])
        .unwrap();
    let got: Vec<String> = log.iter().map(|c| c.id.clone()).collect();
    assert_eq!(got, want, "must not walk past the fork into main's history");
}

/// `A...B` (symmetric diff) is refused by name, not answered with
/// `"could not resolve commit"` (there is no such single revision).
#[test]
fn log_symmetric_range_is_refused_by_name_not_a_resolve_failure() {
    let (dir, _ids) = repo_with_history();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let err = eng
        .log(&GitCaveats::top(), 10, Some("HEAD~1...HEAD"), &[])
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("symmetric ranges are not supported"), "{msg}");
}

/// Same refusal for `diff A...B`.
#[test]
fn diff_symmetric_range_is_refused_by_name() {
    let (dir, ids) = repo_with_history();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let err = eng
        .diff(
            &GitCaveats::top(),
            DiffSpec::Rev(format!("{}...{}", ids[0], ids[2])),
            &[],
            false,
        )
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("symmetric ranges are not supported"));
}

/// A bare revision token that is ALSO an existing worktree path is ambiguous
/// — real git refuses rather than silently picking the revision reading.
#[test]
fn log_revision_that_is_also_an_existing_path_is_refused_as_ambiguous() {
    let (dir, _ids) = repo_with_history();
    // A branch literally named after the on-disk path `a.txt`.
    git(dir.path(), &["branch", "a.txt"]);
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let err = eng
        .log(&GitCaveats::top(), 10, Some("a.txt"), &[])
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("ambiguous"), "{msg}");
}

/// A revision token that resolves but has no same-named worktree path is
/// unaffected by the ambiguity check.
#[test]
fn log_revision_that_only_resolves_is_not_flagged_ambiguous() {
    let (dir, ids) = repo_with_history();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let log = eng
        .log(&GitCaveats::top(), 10, Some(ids[1].as_str()), &[])
        .unwrap();
    assert_eq!(log[0].id, ids[1]);
}
