use super::*;

// --- LocalGitTool (the injected GitTool seam) ---------------------------
#[test]
fn dispatch_init_creates_a_repo_in_a_non_repo_dir_then_commit_works() {
    // Regression: before `op=init`, the embedded git tool was only advertised
    // inside an existing repo and had NO way to create one — an agent in a
    // fresh dir saw no git tool and gave up committing. `init` makes the tool
    // useful there. (Would previously fail: "unknown git op 'init'".)
    let dir = tempfile::tempdir().unwrap();
    assert!(
        GitEngine::open(dir.path()).is_err(),
        "precondition: not a repo yet"
    );
    let t = tool(dir.path());
    let out = t
        .dispatch("init", &serde_json::json!({}), &GitCaveats::top())
        .unwrap();
    assert!(out.contains("initialized"), "got: {out}");
    assert!(
        GitEngine::open(dir.path()).is_ok(),
        "init created a real, openable repo"
    );
    // ...and the rest of the tool now works against the fresh repo.
    std::fs::write(dir.path().join("f.txt"), "x\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["f.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    let c = t
        .dispatch(
            "commit",
            &serde_json::json!({"message": "first"}),
            &GitCaveats::top(),
        )
        .unwrap();
    assert!(c.contains("committed"), "got: {c}");
}
/// #1709 family (req 1): the contributor ledger must clear ONLY after an
/// explicitly confirmed successful commit, never merely because `HEAD`
/// changed. [`LocalGitTool::drain_commit_success`] is that explicit signal
/// — it returns the number of Newt commits that ACTUALLY landed since the
/// last drain (and resets it), so the session loop clears the ledger off
/// THIS, not a `HEAD` diff. This grounds the chat.rs clear-site change: a
/// successful `commit`/`amend`/`rebase` increments the counter; a failed
/// commit, a denied commit, or a no-op leaves it at zero. Real-resource
/// (tempdir + real git) because "did a commit land" is a property of the
/// real git engine, not a mock.
#[test]
fn drain_commit_success_signals_only_a_confirmed_landing() {
    let dir = tempfile::tempdir().unwrap();
    let t = tool(dir.path());
    t.dispatch("init", &serde_json::json!({}), &GitCaveats::top())
        .unwrap();
    // No commit yet → drain is zero (a bare `HEAD` move from init does NOT
    // count as a contributor-consuming commit).
    assert_eq!(t.drain_commit_success(), 0);
    std::fs::write(dir.path().join("f.txt"), "x\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["f.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    // A FAILED commit (empty message is rejected) does NOT signal.
    let bad = t.dispatch(
        "commit",
        &serde_json::json!({"message": "   "}),
        &GitCaveats::top(),
    );
    assert!(bad.is_err(), "empty message must be rejected");
    assert_eq!(
        t.drain_commit_success(),
        0,
        "a failed commit does not signal"
    );
    // A CONFIRMED successful commit signals exactly one.
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "first"}),
        &GitCaveats::top(),
    )
    .unwrap();
    assert_eq!(t.drain_commit_success(), 1, "one confirmed commit");
    // Draining resets the counter — a second drain reads zero.
    assert_eq!(t.drain_commit_success(), 0, "drain resets the counter");
    // Amend is also a commit creation → signals.
    t.dispatch(
        "amend",
        &serde_json::json!({"message": "first (reworded)"}),
        &GitCaveats::top(),
    )
    .unwrap();
    assert_eq!(t.drain_commit_success(), 1, "amend signals a commit");
}
/// #1709 family: the contributor snapshot is CONSUMED at the actual
/// successful commit boundary, not deferred to the end-of-turn drain.
/// Within ONE tool/turn lifecycle (one frozen envelope), commit C1
/// credits the envelope's accumulated contributors, then "more work" →
/// commit C2 in the SAME lifecycle must NOT re-credit C1's contributors:
/// C1's success advanced the consumption cursor past them, so C2's
/// contributor slice is empty and it credits only the active model.
///
/// Real git (tempdir + real commits) because "the trailer landed on C1
/// and NOT on C2" is a property of the real commit objects, not a mock —
/// this grounds the cursor logic in `finalize_commit_message` /
/// `consume_contributors`. The active model (`qwen3:30b`) deliberately
/// differs from the accumulated contributor (`model-a`) so the
/// distinction is visible: C1 credits BOTH, C2 credits ONLY the active
/// model.
#[test]
fn contributor_snapshot_consumed_at_commit_boundary_not_turn_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = tool(dir.path());
    // Inject one accumulated contributor (model-a) into the envelope —
    // the session loop would snapshot this from the ledger at loop-top.
    if let Some(a) = t.attribution.as_mut() {
        a.contributors.push(
            newt_core::attribution::Attribution::new(
                "model-a",
                "newt-agent",
                newt_core::build_info::PACKAGE_VERSION,
                "noreply@newt-agent.com",
            )
            // Mirrors production: the session ledger stamps this build on
            // every contribution it records.
            .with_build(newt_core::build_info::SOURCE_ID),
        );
    }
    t.dispatch("init", &serde_json::json!({}), &GitCaveats::top())
        .unwrap();

    // C1: credits model-a (contributor) + qwen3:30b (active).
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["a.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "C1"}),
        &GitCaveats::top(),
    )
    .unwrap();
    let c1 = head_message(dir.path());
    let version = newt_core::build_info::PACKAGE_VERSION;
    // The build revision is part of the contributor identity, so it
    // renders in the qualifier alongside the version.
    let build = newt_core::build_info::SOURCE_ID;
    assert!(
        c1.contains(&format!(
            "Co-authored-by: model-a (newt-agent v{version} {build}) <noreply@newt-agent.com>"
        )),
        "C1 credits the accumulated contributor A: {c1}"
    );
    assert!(
        c1.contains(&format!(
            "Co-authored-by: qwen3:30b (newt-agent v{version} {build}) <noreply@newt-agent.com>"
        )),
        "C1 credits the active model B: {c1}"
    );

    // "more work" then C2 in the SAME lifecycle (same frozen envelope;
    // C1's success advanced the cursor past model-a).
    std::fs::write(dir.path().join("b.txt"), "y\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["b.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "C2"}),
        &GitCaveats::top(),
    )
    .unwrap();
    let c2 = head_message(dir.path());
    assert!(
        !c2.contains("model-a"),
        "C2 must NOT re-credit A — its snapshot was consumed at C1's boundary: {c2}"
    );
    assert!(
        c2.contains(&format!(
            "Co-authored-by: qwen3:30b (newt-agent v{version} {build}) <noreply@newt-agent.com>"
        )),
        "C2 still credits the active model B: {c2}"
    );
    // Two distinct commits landed in the one lifecycle.
    assert_eq!(commit_count(dir.path()), 2);
    // The cursor now sits at the (frozen) contributor count; a THIRD
    // commit in this lifecycle would also credit only the active model.
    std::fs::write(dir.path().join("c.txt"), "z\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["c.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "C3"}),
        &GitCaveats::top(),
    )
    .unwrap();
    let c3 = head_message(dir.path());
    assert!(
        !c3.contains("model-a"),
        "C3 still does not re-credit A: {c3}"
    );
    assert_eq!(commit_count(dir.path()), 3);
}
/// #1709 family: a FAILED commit must NOT consume the contributor
/// snapshot — `consume_contributors` runs only AFTER `eng.commit` succeeds
/// (the `?` returns early on failure), so a denied/failed commit leaves
/// the cursor at 0 and the contributors remain available for the next
/// attempt. Real git (tempdir) because "the cursor did not advance" is a
/// property of the real dispatch path through `eng.commit`, not a mock.
#[test]
fn failed_commit_does_not_consume_contributors() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = tool(dir.path());
    // One accumulated contributor on the frozen envelope.
    if let Some(a) = t.attribution.as_mut() {
        a.contributors.push(
            newt_core::attribution::Attribution::new(
                "model-a",
                "newt-agent",
                newt_core::build_info::PACKAGE_VERSION,
                "noreply@newt-agent.com",
            )
            // Mirrors production: the session ledger stamps this build on
            // every contribution it records.
            .with_build(newt_core::build_info::SOURCE_ID),
        );
    }
    t.dispatch("init", &serde_json::json!({}), &GitCaveats::top())
        .unwrap();
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["a.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();

    // A commit DENIED by capability fails inside `eng.commit`; the `?`
    // returns before `consume_contributors`, so the cursor stays 0.
    let denied = t.dispatch(
        "commit",
        &serde_json::json!({"message": "denied"}),
        &GitCaveats::read_only(),
    );
    assert!(denied.is_err(), "read-only caps deny the commit");
    let cursor = t
        .contributors_consumed
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        cursor, 0,
        "a failed commit must NOT consume contributors (cursor still 0): {cursor}"
    );
    // No commit landed, so the success counter is also untouched.
    assert_eq!(t.drain_commit_success(), 0);

    // The retry, with commit authority, succeeds and DOES credit the
    // contributor that the failed attempt left intact.
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "C1"}),
        &GitCaveats::top(),
    )
    .unwrap();
    let c1 = head_message(dir.path());
    let version = newt_core::build_info::PACKAGE_VERSION;
    // The build revision is part of the contributor identity, so it
    // renders in the qualifier alongside the version.
    let build = newt_core::build_info::SOURCE_ID;
    assert!(
        c1.contains(&format!(
            "Co-authored-by: model-a (newt-agent v{version} {build}) <noreply@newt-agent.com>"
        )),
        "the contributor survived the failed commit and is credited on the retry: {c1}"
    );
    assert_eq!(commit_count(dir.path()), 1);
    assert_eq!(t.drain_commit_success(), 1);
}
#[test]
fn dispatch_init_is_idempotent_on_an_existing_repo() {
    let dir = repo_with_commit();
    let out = tool(dir.path())
        .dispatch("init", &serde_json::json!({}), &GitCaveats::top())
        .unwrap();
    assert!(out.contains("already a repository"), "got: {out}");
}
#[test]
fn dispatch_init_is_denied_without_write_permission() {
    let dir = tempfile::tempdir().unwrap();
    let res = tool(dir.path()).dispatch("init", &serde_json::json!({}), &GitCaveats::read_only());
    assert!(res.is_err(), "read-only session must not create a repo");
    assert!(
        GitEngine::open(dir.path()).is_err(),
        "a denied init created nothing"
    );
}
#[test]
fn dispatch_checkout_creates_branch_and_branch_delete_removes_it() {
    let dir = repo_with_commit();
    let t = tool(dir.path());
    // checkout defaults create=true → `checkout -b`.
    let out = t
        .dispatch(
            "checkout",
            &serde_json::json!({"name": "feat/dispatch"}),
            &GitCaveats::top(),
        )
        .unwrap();
    assert!(out.contains("created and switched"), "{out}");
    // Switch back to main (same commit), then delete the scratch branch.
    t.dispatch(
        "checkout",
        &serde_json::json!({"name": "main", "create": false}),
        &GitCaveats::top(),
    )
    .unwrap();
    let del = t
        .dispatch(
            "branch-delete",
            &serde_json::json!({"name": "feat/dispatch"}),
            &GitCaveats::top(),
        )
        .unwrap();
    assert_eq!(del, "deleted branch 'feat/dispatch'");
}
#[test]
fn dispatch_unknown_op_lists_the_supported_ops() {
    let dir = repo_with_commit();
    let t = tool(dir.path());
    // 'pull' is no longer advertised or implemented (local-only).
    let err = t
        .dispatch("pull", &serde_json::json!({}), &GitCaveats::top())
        .unwrap_err();
    assert!(err.contains("unknown git op 'pull'"), "{err}");
    assert!(err.contains("checkout"), "{err}");
    assert!(err.contains("branch-delete"), "{err}");
}
/// #1709 integration: the tool's commit-message attribution now flows
/// through ONE boundary — [`LocalGitTool::finalize_commit_message`] →
/// [`CommitAttribution::finalize_message`] — not the removed
/// `sign_message`/`attribution_block` pair. The model may supply a bare
/// subject with zero attribution text; the harness owns the trailer +
/// provenance, deterministically (no wall clock) and idempotently.
///
/// [`CommitAttribution`]: newt_core::attribution::CommitAttribution
#[test]
fn finalize_commit_message_owns_attribution_deterministically() {
    let t = tool(Path::new(".")); // root unused by finalize_commit_message
                                  // Bare subject, zero attribution text → canonical trailer + provenance.
    let out = t.finalize_commit_message("fix the parser");
    let version = newt_core::build_info::PACKAGE_VERSION;
    // The build revision is part of the contributor identity, so it
    // renders in the qualifier alongside the version.
    let build = newt_core::build_info::SOURCE_ID;
    assert!(
        out.contains(&format!(
            "Co-authored-by: qwen3:30b (newt-agent v{version} {build}) <noreply@newt-agent.com>"
        )),
        "canonical model trailer rendered from the typed value: {out}"
    );
    assert!(
        out.contains("Harness: newt-agent v") && out.contains(" | Model: qwen3:30b | Operator: "),
        "canonical provenance line rendered from the same value: {out}"
    );
    assert!(
        !out.contains("Time:"),
        "no wall-clock field (deterministic): {out}"
    );
    assert!(
        out.starts_with("fix the parser\n\n"),
        "subject preserved verbatim: {out}"
    );
    // A legitimate third-party co-author is preserved verbatim.
    let with_third = "feat: x\n\nCo-authored-by: someone <a@b.c>";
    let out2 = t.finalize_commit_message(with_third);
    assert!(
        out2.contains("Co-authored-by: someone <a@b.c>"),
        "third-party kept: {out2}"
    );
    assert!(
        out2.contains("Co-authored-by: qwen3:30b"),
        "newt model trailer added: {out2}"
    );
    // Idempotent: re-finalizing the finalized message yields the same bytes.
    assert_eq!(
        t.finalize_commit_message(&out),
        out,
        "idempotent re-finalization"
    );
    // No attribution configured → message unchanged (test opt-out path).
    let mut t2 = t;
    t2.attribution = None;
    assert_eq!(t2.finalize_commit_message("m"), "m");
}
