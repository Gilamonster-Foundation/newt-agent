use super::*;

#[test]
fn commit_carries_the_coauthor_trailer_in_the_message() {
    let dir = repo_with_commit();
    std::fs::write(dir.path().join("c.txt"), "x\n").unwrap();
    let t = tool(dir.path());
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["c.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "add c"}),
        &GitCaveats::top(),
    )
    .unwrap();
    // Inspect the real commit message via system git.
    let log = Command::new("git")
        .current_dir(dir.path())
        .args(["log", "-1", "--pretty=%B"])
        .output()
        .unwrap();
    let body = String::from_utf8_lossy(&log.stdout);
    assert!(body.contains("add c"), "subject present: {body}");
    // The canonical harness-managed trailer + provenance, rendered from the
    // typed CommitAttribution (real package version, the configured email).
    let version = newt_core::build_info::PACKAGE_VERSION;
    // The build revision is part of the contributor identity, so it
    // renders in the qualifier alongside the version.
    let build = newt_core::build_info::SOURCE_ID;
    assert!(
        body.contains(&format!(
            "Co-authored-by: qwen3:30b (newt-agent v{version} {build}) <noreply@newt-agent.com>"
        )),
        "canonical model trailer present: {body}"
    );
    assert!(
        body.contains("Harness: newt-agent v") && body.contains(" | Model: qwen3:30b | Operator: "),
        "canonical provenance line present: {body}"
    );
}
/// #1709 acceptance condition: a model may supply a bare subject —
/// "fix the parser" — with ZERO attribution text, and the resulting
/// first-class Newt commit still carries correct harness-managed
/// attribution (the canonical model trailer + provenance, rendered from
/// the typed `CommitAttribution` through the one shared finalizer
/// boundary). Grounds the mocked `finalize_commit_message` test against a
/// real commit read back via system git.
#[test]
fn bare_model_subject_still_gets_harness_managed_attribution() {
    let dir = repo_with_commit();
    std::fs::write(dir.path().join("p.txt"), "x\n").unwrap();
    let t = tool(dir.path());
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["p.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    // Bare subject, no attribution text whatsoever from the model.
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "fix the parser"}),
        &GitCaveats::top(),
    )
    .unwrap();
    let body = head_message(dir.path());
    assert!(body.contains("fix the parser"), "subject preserved: {body}");
    let version = newt_core::build_info::PACKAGE_VERSION;
    // The build revision is part of the contributor identity, so it
    // renders in the qualifier alongside the version.
    let build = newt_core::build_info::SOURCE_ID;
    assert!(
        body.contains(&format!(
            "Co-authored-by: qwen3:30b (newt-agent v{version} {build}) <noreply@newt-agent.com>"
        )),
        "canonical model trailer added by the harness: {body}"
    );
    assert!(
        body.contains(" | Model: qwen3:30b | Operator: "),
        "canonical provenance line added by the harness: {body}"
    );
}
/// #551 regression at the commit boundary: a `/model` switch between two
/// commits must attribute EACH commit to the model actually driving it.
/// The second commit carries model B — not a stale model A frozen earlier
/// — and the first commit is NOT retroactively rewritten. This crosses the
/// `/model` → `session_git_tool.attribution` → `finalize_commit_message`
/// boundary at the REAL commit level: the unit-tier
/// `fresh_construction_reflects_a_model_switch` proves the construction
/// half (a fresh `CommitAttribution` sees the new model); this proves the
/// WIRED commit half. It mirrors the per-loop-iteration refresh in
/// `newt-tui::chat` (`tool.attribution = from_identity(&inf_model, …)`)
/// by refreshing `tool.attribution` between commits, then reads each
/// commit back via system git. Real-resource (real git) → grounds the
/// mocked `finalize_commit_message` tests against actual history.
#[test]
fn model_switch_between_commits_attributes_each_to_the_live_model() {
    let dir = repo_with_commit();
    let p = dir.path();
    // Session boots under model A.
    let mut t = tool(p);
    t.attribution = Some(newt_core::attribution::CommitAttribution::from_runtime(
        "model-a",
        None,
        "noreply@newt-agent.com",
    ));

    // Commit C1 under model A.
    std::fs::write(p.join("c1.txt"), "x\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["c1.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "c1 under model A"}),
        &GitCaveats::top(),
    )
    .unwrap();
    let body_c1 = head_message(p);
    assert!(
        body_c1.contains(" | Model: model-a | "),
        "C1 → model A: {body_c1}"
    );
    assert!(!body_c1.contains("model-b"), "C1 has no model B: {body_c1}");

    // `/model model-b`: refresh the tool's attribution, exactly as the chat
    // loop does at the top of the next iteration before the turn's ChatCtx.
    t.attribution = Some(newt_core::attribution::CommitAttribution::from_runtime(
        "model-b",
        None,
        "noreply@newt-agent.com",
    ));
    // Commit C2 under model B.
    std::fs::write(p.join("c2.txt"), "x\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["c2.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "c2 under model B"}),
        &GitCaveats::top(),
    )
    .unwrap();
    let body_c2 = head_message(p);
    assert!(
        body_c2.contains(" | Model: model-b | "),
        "C2 → the LIVE model B at commit time: {body_c2}"
    );
    assert!(
        !body_c2.contains("model-a"),
        "model A does NOT survive as stale Newt attribution on C2 (#551): {body_c2}"
    );

    // The switch did not retroactively rewrite C1 — model A is still there.
    let c1_again = Command::new("git")
        .current_dir(p)
        .args(["log", "--pretty=%B", "--skip=1", "-1"])
        .output()
        .unwrap();
    let body_c1_still = String::from_utf8_lossy(&c1_again.stdout).to_string();
    assert!(
        body_c1_still.contains(" | Model: model-a | "),
        "C1 unchanged after the switch (no backward leakage): {body_c1_still}"
    );
    assert!(
        !body_c1_still.contains("model-b"),
        "C1 not corrupted with model B: {body_c1_still}"
    );

    // `/model model-a` back to a previous model (req 7): switching back works.
    t.attribution = Some(newt_core::attribution::CommitAttribution::from_runtime(
        "model-a",
        None,
        "noreply@newt-agent.com",
    ));
    std::fs::write(p.join("c3.txt"), "x\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["c3.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "c3 back under model A"}),
        &GitCaveats::top(),
    )
    .unwrap();
    let body_c3 = head_message(p);
    assert!(
        body_c3.contains(" | Model: model-a | "),
        "C3 → model A after switching back to a previous model: {body_c3}"
    );
}
/// #551 for the amend path (req 8): amending after a `/model` switch
/// re-signs the commit with the LIVE model's attribution, not the stale
/// model that authored the original commit. The amend arm calls
/// `finalize_commit_message` with the current `tool.attribution`, so the
/// switched model is what lands. Real-resource (real git).
#[test]
fn amend_after_a_model_switch_resigns_with_the_live_model() {
    let dir = repo_with_commit();
    let p = dir.path();
    let mut t = tool(p);
    t.attribution = Some(newt_core::attribution::CommitAttribution::from_runtime(
        "model-a",
        None,
        "noreply@newt-agent.com",
    ));
    std::fs::write(p.join("c1.txt"), "x\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["c1.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "orig under model A"}),
        &GitCaveats::top(),
    )
    .unwrap();
    assert!(head_message(p).contains(" | Model: model-a | "));

    // `/model model-b`, then amend the commit.
    t.attribution = Some(newt_core::attribution::CommitAttribution::from_runtime(
        "model-b",
        None,
        "noreply@newt-agent.com",
    ));
    std::fs::write(p.join("c2.txt"), "x\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["c2.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch(
        "amend",
        &serde_json::json!({"message": "reworded under model B"}),
        &GitCaveats::top(),
    )
    .unwrap();
    let body = head_message(p);
    assert!(
        body.contains(" | Model: model-b | "),
        "amended commit → the live model B: {body}"
    );
    assert!(
        !body.contains("model-a"),
        "stale model A did NOT survive the amend (#551): {body}"
    );
    assert!(
        body.contains("reworded under model B"),
        "amend subject preserved: {body}"
    );
}
/// #1709 req 8: `amend` with NO new message still REFRESHES attribution.
/// The amend arm reads HEAD's existing full message and runs it through the
/// canonical finalizer before creating the amended commit, so a `/model`
/// switch since the original commit replaces the stale Newt model trailers
/// and provenance (the user subject/body and third-party trailers are
/// preserved).
///
/// Real-resource (real git).
#[test]
fn amend_with_no_message_refreshes_attribution_after_a_model_switch() {
    let dir = repo_with_commit();
    let p = dir.path();
    let mut t = tool(p);
    t.attribution = Some(newt_core::attribution::CommitAttribution::from_runtime(
        "model-a",
        None,
        "noreply@newt-agent.com",
    ));
    std::fs::write(p.join("c1.txt"), "x\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["c1.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "orig under model A"}),
        &GitCaveats::top(),
    )
    .unwrap();
    let before = head_message(p);
    assert!(
        before.contains(" | Model: model-a | "),
        "C1 → model A: {before}"
    );
    assert!(before.contains("orig under model A"));

    // `/model model-b`, stage more work, then amend with NO message.
    t.attribution = Some(newt_core::attribution::CommitAttribution::from_runtime(
        "model-b",
        None,
        "noreply@newt-agent.com",
    ));
    std::fs::write(p.join("c2.txt"), "x\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["c2.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch("amend", &serde_json::json!({}), &GitCaveats::top())
        .unwrap();
    let body = head_message(p);
    assert!(
        body.contains(" | Model: model-b | "),
        "amend(no message) → the LIVE model B: {body}"
    );
    assert!(
        !body.contains("model-a"),
        "stale model A did NOT survive amend(no message): {body}"
    );
    // The original subject/body is preserved (not erased by re-finalization).
    assert!(
        body.contains("orig under model A"),
        "amend(no message) preserved the user subject/body: {body}"
    );
}
#[test]
fn local_git_tool_status_renders_readable_text() {
    let dir = repo_with_commit();
    let t = tool(dir.path());
    let out = t
        .dispatch("status", &serde_json::json!({}), &GitCaveats::top())
        .unwrap();
    assert!(out.contains("on branch main"), "got: {out}");
    assert!(out.contains("working tree clean"), "got: {out}");
}
