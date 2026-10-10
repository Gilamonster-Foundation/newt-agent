use super::*;

/// **The cap is a ROUND limit, and the trailer must say so** (#1965).
///
/// It read "the tool-call limit of 40 rounds" — which names one unit and
/// measures in another. A round may issue several tool calls, so the evidenced
/// session showed 65 tool calls under a 40-round cap, and an operator reading
/// the trailer had every reason to think the cap was 40 calls and that they
/// had been given 25 extra. The number was always the EFFECTIVE cap; the unit
/// was the lie.
///
/// Pins every operator-facing cap-exit surface at once, because the phrase was
/// duplicated across three of them and one site in `mod.rs` already spelled it
/// "tool-round limit" — the codebase was inconsistent with itself.
#[test]
fn no_cap_exit_surface_calls_a_round_limit_a_tool_call_limit() {
    let surfaces = [
        cap_exit_nudge(40, None, &[]),
        cap_exit_fallback(40, None, 0, None),
        cap_exit_progress_handoff(40, None, "done", false, None),
    ];
    for text in &surfaces {
        assert!(
            !text.contains("tool-call limit"),
            "a round limit announced as a call limit: {text}"
        );
        assert!(
            text.contains("tool-round limit"),
            "…and it must still name the limit it hit: {text}"
        );
        assert!(
            text.contains("40 rounds"),
            "…with the EFFECTIVE cap and its unit: {text}"
        );
    }
}

#[test]
fn cap_exit_nudge_names_the_limit_and_folds_in_progress() {
    let nudge = cap_exit_nudge(5, None, &[]);
    assert!(nudge.contains("5 rounds"), "got: {nudge}");
    assert!(nudge.contains("Do NOT call any more tools"));
    assert!(
        nudge.contains("progress update"),
        "the cap turn asks for a handoff, not a fake final answer: {nudge}"
    );
    assert!(
        nudge.contains("tool loop has stopped at the cap")
            && nudge.contains("operator's objective is complete or what remains"),
        "the model should distinguish a stopped loop from objective completion: {nudge}"
    );
    // #867: the grounding constraint — the trim just deleted the evidence,
    // so the nudge must forbid reconstructing paths from memory.
    assert!(
        nudge.contains("Cite only file paths that appear verbatim"),
        "got: {nudge}"
    );
    assert!(nudge.contains("say so plainly"), "got: {nudge}");
    assert!(
        !nudge.contains("progress so far"),
        "no block when None: {nudge}"
    );
    assert!(
        !nudge.contains("actually observed"),
        "no manifest block when the ledger is empty: {nudge}"
    );
    // Step 27.5: the <plan>/<state> progress is folded into the nudge.
    let with = cap_exit_nudge(5, Some("<plan>1. [x] foo</plan>"), &[]);
    assert!(with.contains("Your progress so far"), "got: {with}");
    assert!(with.contains("<plan>1. [x] foo</plan>"), "got: {with}");
}

/// #867 Part A: the observed-paths manifest survives the trim and is
/// handed to the model as the citable ground truth.
#[test]
fn cap_exit_nudge_folds_in_the_observed_paths_manifest() {
    let observed = vec![
        "newt-tui/src/lib.rs".to_string(),
        "newt-core/src/agentic/mod.rs".to_string(),
    ];
    let nudge = cap_exit_nudge(5, Some("<state>k=v</state>"), &observed);
    assert!(
        nudge.contains("File paths actually observed in tool results"),
        "got: {nudge}"
    );
    assert!(nudge.contains("- newt-tui/src/lib.rs"), "got: {nudge}");
    assert!(
        nudge.contains("- newt-core/src/agentic/mod.rs"),
        "got: {nudge}"
    );
    // Manifest precedes the progress block; both survive together.
    let manifest_at = nudge.find("actually observed").unwrap();
    let progress_at = nudge.find("Your progress so far").unwrap();
    assert!(manifest_at < progress_at, "got: {nudge}");
}

#[test]
fn cap_exit_progress_renders_plan_and_state_or_none() {
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};
    use crate::agentic::scratchpad::{ScratchpadStore, SessionScratchpadStore};
    let ledger = SessionStepLedger::default();
    let pad = SessionScratchpadStore::default();
    // Both empty → nothing to salvage.
    assert!(cap_exit_progress(None, Some(&ledger), Some(&pad)).is_none());
    assert!(cap_exit_progress(None, None, None).is_none());
    // Populated → a combined block naming both.
    ledger.set_plan(&["build it".to_string(), "test it".to_string()]);
    pad.set("cwd", "/work".to_string());
    let p = cap_exit_progress(
        None,
        Some(&ledger as &dyn StepLedger),
        Some(&pad as &dyn ScratchpadStore),
    )
    .expect("non-empty progress");
    assert!(p.contains("build it"), "{p}");
    assert!(p.contains("cwd"), "{p}");
}

#[test]
fn cap_exit_fallback_usage_advice_and_salvage() {
    // wasted_calls < rounds → caller-neutral advice to increase the limit.
    let with = cap_exit_fallback(
        4,
        Some(crate::TokenUsage {
            input_tokens: 12,
            output_tokens: 34,
        }),
        0,
        None,
    );
    assert!(with.contains("12 in / 34 out tokens"), "got: {with}");
    assert!(
        with.contains("increase the tool-round limit"),
        "got: {with}"
    );

    let without = cap_exit_fallback(4, None, 0, None);
    assert!(!without.contains("tokens consumed"), "got: {without}");
    assert!(without.contains("tool-round limit (4"), "got: {without}");

    // Step 27.5: a thrash run (≥ one failed call per round) gets HONEST
    // advice — a tooling problem, not "raise the cap".
    let thrash = cap_exit_fallback(4, None, 6, None);
    assert!(thrash.contains("tool calls that failed"), "got: {thrash}");
    assert!(
        !thrash.contains("raise [tui].max_tool_rounds"),
        "thrash advice must not blame the cap: {thrash}"
    );

    // Step 27.5: progress is salvaged even when the summary failed.
    let salvaged = cap_exit_fallback(4, None, 0, Some("<state>cwd=/x</state>"));
    assert!(salvaged.contains("Progress captured"), "got: {salvaged}");
    assert!(salvaged.contains("Current state:"), "got: {salvaged}");
    assert!(salvaged.contains("cwd=/x"), "got: {salvaged}");
    assert!(!salvaged.contains("<state>"), "got: {salvaged}");
}

#[test]
fn cap_exit_summary_detects_every_pending_action_handoff() {
    let handoff = "I have two issues: duplicate topic_has_rollups and a stray brace. Let me fix both — read around 490 to see what needs removing, then verify with a build check.";
    assert!(cap_exit_summary_is_action_handoff(handoff));
    let plan_update = "Summary\n\nI reached the tool-round limit.\n\nNext Steps Required\n\nTo continue, I would need to remove the duplicate function using edit_file, verify cargo check, then finish the plan.";
    assert!(
        cap_exit_summary_is_action_handoff(plan_update),
        "plan-shaped progress handoffs also contain pending actions"
    );
    assert!(!cap_exit_summary_is_action_handoff(
            "The duplicate helper definitions and stray brace were removed, and the build check passed."
        ));

    let paused = cap_exit_progress_handoff(
        25,
        None,
        plan_update,
        true,
        Some("<plan>1. [ ] remove duplicate helper</plan><state>check=pending</state>"),
    );
    assert!(paused.contains("tool-round limit (25"), "{paused}");
    assert!(
        paused.contains("Next Steps Required"),
        "the model-authored progress update survives: {paused}"
    );
    assert!(
        paused.contains("have not run yet"),
        "pending work is not presented as completed: {paused}"
    );
    assert!(paused.contains("progress handoff"), "{paused}");
    assert!(paused.contains("Captured working state"), "{paused}");
    assert!(paused.contains("remove duplicate helper"), "{paused}");
    assert!(paused.contains("Current state:"), "{paused}");
    assert!(!paused.contains("<plan>"), "{paused}");
}

#[test]
fn cap_exit_model_reply_only_wraps_real_progress_handoffs() {
    let completed = "The duplicate helper was removed and cargo check passed.";
    assert_eq!(cap_exit_model_reply(25, None, completed, None), completed);

    let pending = "Next steps: remove the duplicate helper, then run cargo check.";
    let pending_reply = cap_exit_model_reply(25, None, pending, None);
    assert!(pending_reply.starts_with(pending), "{pending_reply}");
    assert!(
        pending_reply.contains("progress handoff"),
        "{pending_reply}"
    );
    assert!(
        pending_reply.contains("have not run yet"),
        "{pending_reply}"
    );

    let captured = cap_exit_model_reply(
        25,
        None,
        completed,
        Some("<state>cargo check still pending</state>"),
    );
    assert!(captured.contains("Captured working state"), "{captured}");
    assert!(captured.contains("cargo check still pending"), "{captured}");
}

#[test]
fn cap_exit_finalizer_applies_workspace_claim_checks() {
    let workspace = tempfile::TempDir::new().expect("temp workspace");
    let text = finalize_final_text(
        "Updated src/definitely_not_present.rs and verified it.".to_string(),
        &workspace.path().to_string_lossy(),
        &crate::Scope::All,
        &capability_check::Evidence::default(),
        None,
        &claim_check::TurnClaims::capture(
            &workspace.path().to_string_lossy(),
            &crate::Scope::All,
            None,
        ),
        &self_verify::VerificationLedger::default(),
    );
    assert!(
        text.contains("⚠ claim check (#867)"),
        "cap handoffs must use the same path grounding gate on every provider: {text}"
    );
}

/// #2683 round 2 (PR #2688 review, finding 3): a bare git subprocess with
/// only `current_dir` set inherits this process's FULL
/// environment — an operator shell's `GIT_DIR`/`GIT_WORK_TREE` (the dotfiles
/// idiom: `GIT_DIR=~/.dotfiles GIT_WORK_TREE=~`) or a `~/.gitconfig` naming
/// `core.hooksPath` would otherwise redirect the fixture's `init`/`add`/
/// `commit` outside the tempdir, or run a hook. `env_clear()` plus an
/// explicit, minimal env — mirroring `agentic::tools_tests`'s
/// `hermetic_git_env` (the Landlock-confinement proof's own fixture) —
/// closes every ambient git/HOME knob instead of denylisting the ones
/// remembered. Proven isolated by
/// [`hermetic_git_ignores_a_hostile_inherited_git_dir_and_work_tree`]. The
/// author/committer identity is passed as env, not `git config`, so no
/// config write is needed before the first commit.
fn hermetic_git(dir: &std::path::Path, home: &std::path::Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(dir).env_clear().envs([
        ("HOME", home.to_string_lossy().into_owned()),
        ("PATH", std::env::var("PATH").unwrap_or_default()),
        ("GIT_AUTHOR_NAME", "t".to_string()),
        ("GIT_AUTHOR_EMAIL", "t@example.invalid".to_string()),
        ("GIT_COMMITTER_NAME", "t".to_string()),
        ("GIT_COMMITTER_EMAIL", "t@example.invalid".to_string()),
        ("GIT_CONFIG_NOSYSTEM", "1".to_string()),
        ("GIT_CONFIG_GLOBAL", "/dev/null".to_string()),
        ("GIT_TEMPLATE_DIR", "/dev/null".to_string()),
    ]);
    cmd
}

/// Env var whose presence makes a re-execution of this test binary run as
/// the dedicated hostile-environment subprocess of
/// [`hermetic_git_ignores_a_hostile_inherited_git_dir_and_work_tree`] (its
/// value is the fixture dir). Set ONLY on the `Command` that spawns that one
/// child, never on this process — PR #2688 round 2 review, finding 3: a
/// process-global `crate::process_env::set_var` races every OTHER test
/// sharing this binary (the mutex it took only protects callers that also
/// take the same lock; a plain `std::env::var` read elsewhere does not).
/// Same pattern as `tools_tests::helper_git_shell_grant`'s
/// `HOSTILE_ENV_SUBPROCESS_ROOT`.
const HOSTILE_ENV_SUBPROCESS_DIR: &str = "NEWT_2683_HOSTILE_ENV_SUBPROCESS_DIR";

/// #2683 round 2 (PR #2688 review, finding 3): proves [`hermetic_git`] is
/// actually hermetic. A hostile `GIT_DIR`/`GIT_WORK_TREE` — the dotfiles
/// idiom a developer shell exports — must never redirect `git init` into
/// the directory those vars name.
///
/// Round 3 (PR #2688 latest review, finding 3): the hostile vars are set
/// only on the `Command` that re-executes this SAME test as a dedicated
/// child process — this process's own environment is never mutated, so no
/// other test sharing this binary can observe a hostile value. The child's
/// re-entry branch (below) runs [`hermetic_git`] for real, with the hostile
/// vars genuinely present in ITS OWN ambient environment; `env_clear()`
/// means that environment never reaches the grandchild `git` process.
/// Confirmed red against the pre-fix bare git subprocess (only
/// `current_dir` set, no `env_clear`): with the same hostile vars set,
/// `init` created `.git` under the HOSTILE `GIT_DIR` instead of inside `dir`.
#[test]
fn hermetic_git_ignores_a_hostile_inherited_git_dir_and_work_tree() {
    // Re-entry: run as the dedicated hostile subprocess. `dir` already
    // exists (the parent created it); `home` is this child's own, since no
    // state about it needs to flow back to the parent.
    if let Ok(dir) = std::env::var(HOSTILE_ENV_SUBPROCESS_DIR) {
        let home = tempfile::TempDir::new().expect("fixture home");
        let ok = hermetic_git(std::path::Path::new(&dir), home.path())
            .args(["init", "-q"])
            .status()
            .expect("git init")
            .success();
        assert!(ok, "git init must succeed under the hermetic fixture");
        return;
    }

    let hostile = tempfile::TempDir::new().expect("hostile root");
    let hostile_gitdir = hostile.path().join(".git");
    let dir = tempfile::TempDir::new().expect("fixture repo");

    let this_test = format!(
        "{}::hermetic_git_ignores_a_hostile_inherited_git_dir_and_work_tree",
        module_path!().split_once("::").map_or("", |(_, rest)| rest)
    );
    let exe = std::env::current_exe().expect("current_exe must resolve for the re-exec proof");
    let output = std::process::Command::new(&exe)
        .args(["--exact", &this_test, "--nocapture", "--test-threads=1"])
        // Explicit, minimal: this Command's env is what the child inherits.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env(HOSTILE_ENV_SUBPROCESS_DIR, dir.path())
        .env("GIT_DIR", &hostile_gitdir)
        .env("GIT_WORK_TREE", hostile.path())
        .output()
        .expect("spawn hostile-env subprocess");

    assert!(
        output.status.success(),
        "hostile-env subprocess fixture failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        dir.path().join(".git").exists(),
        "init must create .git INSIDE the fixture dir, never the hostile GIT_DIR"
    );
    assert!(
        !hostile_gitdir.exists(),
        "the hostile GIT_DIR must never be touched"
    );
}

/// #2683, through the REAL dispatch path (`finalize_final_text`, real `git`
/// subprocess calls via `claim_check::git_in`), not just the pure
/// `annotate_action_claims` unit tests: a real repo whose HEAD genuinely did
/// not move since `head_before` was captured — the exact retest shape
/// (COMPARE.md, 2026-10-02) where the model reported "committed locally" on
/// a change that was only staged. Round 3: the annotation is a neutral facts
/// note, not a verdict.
#[test]
fn cap_exit_finalizer_adds_a_facts_note_when_head_did_not_move_in_a_real_repo() {
    let workspace = tempfile::TempDir::new().expect("temp workspace");
    let home = tempfile::TempDir::new().expect("temp HOME");
    let git = |args: &[&str]| {
        assert!(
            hermetic_git(workspace.path(), home.path())
                .args(args)
                .output()
                .expect("git")
                .status
                .success(),
            "git {args:?}"
        );
    };
    git(&["init", "-q"]);
    std::fs::write(workspace.path().join("a.txt"), "one\n").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-q", "-m", "init"]);
    let head_before = claim_check::TurnClaims::capture(
        &workspace.path().to_string_lossy(),
        &crate::Scope::All,
        None,
    );
    // The turn only staged a further edit — exactly the measured bug — so
    // HEAD at finalize time is IDENTICAL to `head_before`.
    std::fs::write(workspace.path().join("a.txt"), "one\ntwo\n").unwrap();
    git(&["add", "a.txt"]);

    let text = finalize_final_text(
        "I committed the change locally.".to_string(),
        &workspace.path().to_string_lossy(),
        &crate::Scope::All,
        &capability_check::Evidence::default(),
        None,
        &head_before,
        &self_verify::VerificationLedger::default(),
    );
    assert!(
        text.contains("⚠ claim check (#2683)"),
        "a staged-not-committed claim must get a facts note through the real dispatch path: {text}"
    );
    assert!(text.contains("HEAD did not move"), "got: {text}");
}

/// #2683 round 3 (PR #2688 review round 2, finding 1), through the REAL
/// dispatch path: the claim names a specific file (`src/a.txt`) that is
/// still dirty while HEAD moved for an UNRELATED reason (`other.txt`) in the
/// same turn — exactly the shape a bare HEAD-moved check cannot distinguish
/// from the claimed file actually being committed. The facts note reports
/// `src/a.txt` as still uncommitted, as a FACT read off `git status
/// --porcelain`, never as a per-claim verdict (round 2's per-file matching
/// is gone entirely; see `claim_check::commit_facts_note`).
#[test]
fn cap_exit_finalizer_facts_note_names_a_file_still_dirty_when_head_moved_in_a_real_repo() {
    let workspace = tempfile::TempDir::new().expect("temp workspace");
    let home = tempfile::TempDir::new().expect("temp HOME");
    let git = |args: &[&str]| {
        assert!(
            hermetic_git(workspace.path(), home.path())
                .args(args)
                .output()
                .expect("git")
                .status
                .success(),
            "git {args:?}"
        );
    };
    git(&["init", "-q"]);
    std::fs::create_dir(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/a.txt"), "one\n").unwrap();
    git(&["add", "src/a.txt"]);
    git(&["commit", "-q", "-m", "init"]);
    let head_before = claim_check::TurnClaims::capture(
        &workspace.path().to_string_lossy(),
        &crate::Scope::All,
        None,
    );

    // The turn's actual edit: staged but never committed.
    std::fs::write(workspace.path().join("src/a.txt"), "one\ntwo\n").unwrap();
    git(&["add", "src/a.txt"]);
    // An unrelated commit moves HEAD away from `head_before` — the shape a
    // bare `head_moved` check cannot distinguish from the claimed file
    // actually being committed. Committed by PATHSPEC (`-- other.txt`), not
    // a bare `git commit`, so it does not sweep the already-staged
    // `src/a.txt` edit in with it: that edit must stay in the index,
    // uncommitted, after this.
    std::fs::write(workspace.path().join("other.txt"), "unrelated\n").unwrap();
    git(&["add", "other.txt"]);
    git(&["commit", "-q", "-m", "unrelated", "--", "other.txt"]);

    let text = finalize_final_text(
        "I committed src/a.txt.".to_string(),
        &workspace.path().to_string_lossy(),
        &crate::Scope::All,
        &capability_check::Evidence::default(),
        None,
        &head_before,
        &self_verify::VerificationLedger::default(),
    );
    assert!(
        text.contains("⚠ claim check (#2683)"),
        "a claimed file still staged must get a facts note even though HEAD moved \
         (for an unrelated reason), through the real dispatch path: {text}"
    );
    assert!(text.contains("`src/a.txt`"), "got: {text}");
    assert!(text.contains("moved from"), "HEAD DID move: {text}");
    assert!(
        !text.contains("HEAD did not move"),
        "HEAD DID move in this scenario: {text}"
    );
}

#[test]
fn read_only_tools_classified_correctly() {
    // save_note writes memory, not the workspace: a round that only
    // saved a note must still count toward the read-only write-nudge.
    for name in &[
        "list_dir",
        "read_file",
        "find",
        "search",
        "web_fetch",
        "use_skill",
        "save_note",
        "prompt_read",
    ] {
        assert!(is_read_only_tool(name), "{name} should be read-only");
    }
}

#[test]
fn prompt_read_exact_recovery_is_never_spilled() {
    let store = content_spill::SessionSpillStore::new([7u8; 16]);
    let exact = "x".repeat(content_spill::TOOL_RESULT_SPILL_CAP + 1);
    let output = maybe_offload_tool_result("prompt_read", exact.clone(), true, Some(&store), None);
    assert_eq!(output, exact);
    assert_eq!(content_spill::SpillStore::unique_objects(&store), 0);
}

#[test]
fn memory_fetch_exact_recovery_is_never_spilled() {
    let store = content_spill::SessionSpillStore::new([7u8; 16]);
    let padding = "x".repeat(content_spill::TOOL_RESULT_SPILL_CAP);
    let exact = format!("{padding}\nRECOVERED_MIDDLE_DETAIL\n{padding}");
    let output = maybe_offload_tool_result("memory_fetch", exact.clone(), true, Some(&store), None);
    assert!(
        output.contains("RECOVERED_MIDDLE_DETAIL"),
        "fetching an offloaded body must recover its middle, not another retrieval marker"
    );
    assert_eq!(output, exact);
    assert_eq!(content_spill::SpillStore::unique_objects(&store), 0);
}

#[test]
fn disclosure_chokepoint_redacts_registered_canary_in_every_encoding() {
    // step-6.1a ratchet guard (`disclosure-gate-live-path`): a value registered
    // at session start must not reach the model-facing tool message in ANY
    // encoding — raw, base64, or hex. Proves the single live chokepoint
    // (`maybe_offload_tool_result`) runs the by-value `DisclosureFilter`,
    // including for the early-return tools (`run_command` here) that the
    // offload/spill redaction never touched. Also pins that the `None` path is
    // byte-for-byte unchanged, so the gate is inert until a secret is registered.
    use crate::ocap::DisclosureFilter;
    use base64::Engine as _;

    let canary = "CANARY-9f3a2b1c8d7e6f50";
    let mut filter = DisclosureFilter::new();
    filter.register(canary);

    let b64 = base64::engine::general_purpose::STANDARD.encode(canary.as_bytes());
    let hex: String = canary
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    // A tool output embedding the canary raw + re-encoded, as an exfil would.
    let raw = format!("secret {canary} b64 {b64} hex {hex}\n");

    let gated = maybe_offload_tool_result("run_command", raw.clone(), false, None, Some(&filter));
    assert!(
        !filter.leaks(&gated),
        "no encoding of the registered canary may survive the chokepoint: {gated}"
    );
    assert!(
        gated.contains("[REDACTED]"),
        "the canary is replaced: {gated}"
    );

    // Negative control: no filter → byte-identical (the gate is off = unchanged).
    let ungated = maybe_offload_tool_result("run_command", raw.clone(), false, None, None);
    assert_eq!(
        ungated, raw,
        "a `None` disclosure filter must not alter output"
    );
}

#[test]
fn no_model_ingress_funnel_leaks_a_registered_session_secret() {
    // step-6.6 (`disclosure-gate-live-path` #5): with the session filter
    // installed on this turn's thread (as both live builders do), NO
    // model-ingress funnel may carry a registered secret — even when the
    // explicit `disclosure` param is `None` (the TLS is the uniform backstop).
    // Covers the tool-result chokepoint AND the summary path here; the
    // memory/observation/compaction/spill funnel (`redact_secrets`) is proven
    // by `compress::redaction_tests::redact_secrets_value_filters_a_registered_session_secret`.
    let canary = "NEWT-CANARY-e2e-7f3a9c2b1d";
    let mut f = crate::ocap::DisclosureFilter::new();
    f.register(canary);
    let _g = crate::ocap::scoped_session_disclosure(f);

    // 1. Tool-result chokepoint, explicit param = None → TLS backstop redacts.
    let tool =
        maybe_offload_tool_result("run_command", format!("out: {canary}"), false, None, None);
    assert!(
        !tool.contains(canary),
        "tool-result funnel leaked a registered secret via the TLS backstop: {tool}"
    );
    // 2. Summary funnel, param = None → TLS backstop redacts.
    let summary = redact_model_facing(None, format!("final answer mentions {canary}"));
    assert!(
        !summary.contains(canary),
        "summary funnel leaked a registered secret: {summary}"
    );
}

#[test]
fn write_tools_not_read_only() {
    for name in &["edit_file", "write_file", "run_command"] {
        assert!(!is_read_only_tool(name), "{name} should NOT be read-only");
    }
}

#[test]
fn read_only_call_classifies_simple_shell_probes() {
    assert!(is_read_only_call(
        "git",
        &serde_json::json!({"op": "branch-list"})
    ));
    assert!(!is_read_only_call(
        "git",
        &serde_json::json!({"op": "branch", "name": "new"})
    ));
    assert!(!is_read_only_call("git", &serde_json::json!({})));
    assert!(is_read_only_call(
        "run_command",
        &serde_json::json!({"command": "grep -n 'help_lines' newt-tui/src/lib.rs"})
    ));
    assert!(is_read_only_call(
        "run_command",
        &serde_json::json!({"command": "rg -n format_help newt-tui/src"})
    ));
    assert!(is_read_only_call(
        "run_command",
        &serde_json::json!({"command": "sed -n '1,20p' newt-tui/src/lib.rs"})
    ));

    assert!(!is_read_only_call(
        "run_command",
        &serde_json::json!({"command": "cargo test -p newt-tui"})
    ));
    assert!(!is_read_only_call(
        "run_command",
        &serde_json::json!({"command": "sed -i 's/a/b/' file.txt"})
    ));
    assert!(!is_read_only_call(
        "run_command",
        &serde_json::json!({"command": "grep x file > out.txt"})
    ));
}

#[test]
fn read_only_action_nudge_names_edit_permission_and_blocker_paths() {
    let nudge = read_only_action_nudge(3, Some(4), None, None);
    assert!(nudge.contains("read-only rounds so far"), "{nudge}");
    assert!(nudge.contains("edit_file"), "{nudge}");
    assert!(nudge.contains("write_file"), "{nudge}");
    assert!(nudge.contains("request_permissions"), "{nudge}");
    assert!(nudge.contains("exact blocker"), "{nudge}");
    assert!(nudge.contains("operator declined"), "{nudge}");
}

#[test]
fn read_only_action_nudge_counts_only_fixed_rounds_after_the_current_round() {
    for remaining in [0, 4] {
        let nudge = read_only_action_nudge(3, Some(remaining), None, None);
        assert!(nudge.contains(&format!(
            "After this round, at most {remaining} further tool rounds remain under the configured hard limit."
        )), "{nudge}");
        assert!(!nudge.contains("round(s) left"), "{nudge}");
    }
    let renewable = read_only_action_nudge(3, None, None, None);
    assert!(renewable.contains("read-only rounds so far"), "{renewable}");
    assert!(renewable.contains("edit_file or write_file"), "{renewable}");
    assert!(!renewable.contains("configured hard limit"), "{renewable}");
    assert!(!renewable.contains("round(s) left"), "{renewable}");
}

#[test]
fn workflow_action_nudges_honor_operator_stop_and_report_only_requests() {
    let ledger = SessionStepLedger::default();
    ledger.set_plan(&["finish the original task".into(), "verify it".into()]);
    for nudge in [
        narration_action_nudge(),
        escalated_narration_action_nudge(2, 2, Some(&ledger)),
        read_only_action_nudge(3, Some(2), None, None),
        pending_plan_completion_nudge(Some(&ledger), false, None).unwrap(),
        pending_plan_completion_nudge(Some(&ledger), true, None).unwrap(),
    ] {
        assert!(nudge.contains("latest operator instruction"), "{nudge}");
        assert!(nudge.contains("newer steering"), "{nudge}");
        assert!(nudge.contains("stop or report-only"), "{nudge}");
    }
}

#[test]
fn read_only_action_nudge_mentions_active_plan_when_present() {
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};

    let ledger = SessionStepLedger::default();
    ledger.restore(&PlanSnapshot {
        target: None,
        steps: vec![
            Step {
                description: "inspect".to_string(),
                status: StepStatus::Done,
            },
            Step {
                description: "edit".to_string(),
                status: StepStatus::Active,
            },
        ],
    });
    let nudge = read_only_action_nudge(3, Some(2), Some(&ledger as &dyn StepLedger), None);
    assert!(nudge.contains("active multi-step plan"), "{nudge}");
    assert!(nudge.contains("ACTIVE step"), "{nudge}");
    assert!(nudge.contains("latest operator instruction"), "{nudge}");
    assert!(nudge.contains("update_plan"), "{nudge}");
    assert!(
        !nudge.contains("instead of restarting or re-planning"),
        "{nudge}"
    );
}

/// #<issue>: when a `WorkflowSteerer` match offers a delegate hint (e.g.
/// the built-in `diagnose_failure` workflow, and `crew`/`team` dispatch is
/// available this session), the read-only nudge surfaces it — sustained
/// read-only exploration on that task shape is exactly what delegation is
/// for, not just "stop reading, edit it yourself".
#[test]
fn read_only_action_nudge_includes_a_delegate_hint_when_offered() {
    let nudge = read_only_action_nudge(3, Some(4), None, Some("consider calling crew or team"));
    assert!(nudge.contains("consider calling crew or team"), "{nudge}");
    // Still carries the original inline-action guidance too — delegation
    // is offered ALONGSIDE continuing directly, never in place of it.
    assert!(nudge.contains("edit_file"), "{nudge}");
}

#[test]
fn read_only_action_nudge_omits_delegate_clause_when_none_offered() {
    let nudge = read_only_action_nudge(3, Some(4), None, None);
    assert!(!nudge.contains("crew"), "{nudge}");
    assert!(!nudge.contains("team"), "{nudge}");
}

#[test]
fn pending_plan_completion_nudge_is_state_driven() {
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};

    assert!(pending_plan_completion_nudge(None, false, None).is_none());

    let ledger = SessionStepLedger::default();
    ledger.restore(&PlanSnapshot {
        target: None,
        steps: vec![
            Step {
                description: "already done".to_string(),
                status: StepStatus::Done,
            },
            Step {
                description: "keep working".to_string(),
                status: StepStatus::Active,
            },
        ],
    });
    let nudge = pending_plan_completion_nudge(Some(&ledger as &dyn StepLedger), false, None)
        .expect("open plan produces a nudge");
    assert!(nudge.contains("1/2 unfinished step"), "{nudge}");
    assert!(nudge.contains("Active step 2"), "{nudge}");
    assert!(nudge.contains("update_plan"), "{nudge}");
    assert!(nudge.contains("call the next tool"), "{nudge}");
    assert!(nudge.contains("concrete blocker"), "{nudge}");

    let plan_update_nudge = pending_plan_completion_nudge(
            Some(&ledger as &dyn StepLedger),
            true,
            Some(
                "Configured workflow 'github_pr' is active. Workflow steps:\n- commit_step: Commit the verified step",
            ),
        )
        .expect("open plan produces a plan-update nudge");
    assert!(
        plan_update_nudge.contains("findings/next-steps summary"),
        "{plan_update_nudge}"
    );
    assert!(
        plan_update_nudge.contains("Call update_plan now"),
        "{plan_update_nudge}"
    );
    assert!(
        plan_update_nudge.contains("make the immediate blocker repair the active step"),
        "{plan_update_nudge}"
    );
    assert!(
        plan_update_nudge.contains("Do not repeat the findings summary"),
        "{plan_update_nudge}"
    );
    assert!(
        plan_update_nudge.contains("github_pr"),
        "{plan_update_nudge}"
    );
    assert!(
        plan_update_nudge.contains("commit_step"),
        "{plan_update_nudge}"
    );

    ledger.restore(&PlanSnapshot {
        target: None,
        steps: vec![Step {
            description: "complete".to_string(),
            status: StepStatus::Done,
        }],
    });
    assert!(pending_plan_completion_nudge(Some(&ledger as &dyn StepLedger), false, None).is_none());
}

#[test]
fn workflow_classifier_text_keeps_recent_user_issue_context() {
    let messages = vec![
        serde_json::json!({
            "role": "user",
            "content": "Take a look at https://github.com/Gilamonster-Foundation/newt-agent/issues/548 and get me a PR."
        }),
        serde_json::json!({
            "role": "assistant",
            "content": "I will inspect the issue and repo state."
        }),
    ];
    let text = workflow_classifier_text(
            &messages,
            "Summary of Findings\n\nCurrent Status: the build is broken. Next Steps Required: update the plan.",
        );
    let hint = crate::WorkflowSteerer::builtin()
        .plan_update_hint(&text)
        .expect("GitHub issue context should select the PR workflow");
    assert!(hint.contains("github_pr"), "{hint}");
    assert!(hint.contains("read_issue"), "{hint}");
    assert!(hint.contains("open_pr"), "{hint}");
}

// ---------------------------------------------------------------------
// #1965 — the turn heartbeat
// ---------------------------------------------------------------------

#[test]
fn the_heartbeat_notice_pins_due_output_and_skips_missed_intervals() {
    let mut heartbeat = TurnHeartbeat::default();
    let at = std::time::Duration::from_secs;

    assert!(heartbeat.notice(at(299), 10, 40).is_none());
    assert_eq!(
        heartbeat.notice(at(300), 10, 40).as_deref(),
        Some("still working — 5m elapsed, round 10 of 40")
    );
    assert!(heartbeat.notice(at(301), 11, 40).is_none());
    assert_eq!(
        heartbeat.notice(at(4200), 210, 10_000).as_deref(),
        Some("still working — 70m elapsed, round 210 of 10000")
    );
    assert!(heartbeat.notice(at(4200), 210, 10_000).is_none());
}

/// **Bounded, and pure over a stated elapsed.** No clock is read here and none
/// is slept on: `due` takes the elapsed it should judge, so the whole schedule
/// is table-testable. A wall-clock assertion would be a flake generator on a
/// saturating box, which is exactly why the policy and the clock are separate.
#[test]
fn the_heartbeat_fires_once_per_interval_and_never_catches_up() {
    let five = std::time::Duration::from_secs(300);
    let at = std::time::Duration::from_secs;
    let mut hb = TurnHeartbeat::default();

    assert!(!hb.due(at(0), five), "a turn that just started is not late");
    assert!(
        !hb.due(at(299), five),
        "…nor one just short of the first mark"
    );
    assert!(hb.due(at(300), five), "the first interval fires");
    assert!(!hb.due(at(301), five), "…exactly once");
    assert!(!hb.due(at(599), five));
    assert!(hb.due(at(600), five), "the second interval fires");

    // A turn that blocked for an hour inside ONE tool call emits one line on
    // return, not twelve. Catching up would turn a quiet signal into a wall of
    // text at the moment the operator is trying to read what happened.
    assert!(
        hb.due(at(4200), five),
        "the long gap yields exactly one line"
    );
    assert!(!hb.due(at(4200), five));
    assert!(!hb.due(at(4499), five));
}

/// The anti-vacuous twin: an ordinary turn emits NOTHING. A heartbeat that
/// fired on short turns would be noise, and `due` returning `true` always
/// would satisfy the schedule test above on its own.
#[test]
fn an_ordinary_turn_emits_no_heartbeat_at_all() {
    let five = std::time::Duration::from_secs(300);
    let mut hb = TurnHeartbeat::default();
    for secs in [0, 1, 5, 30, 90, 180, 299] {
        assert!(
            !hb.due(std::time::Duration::from_secs(secs), five),
            "{secs}s into a turn is not heartbeat-worthy"
        );
    }
}

/// A zero interval disables it rather than dividing by zero or firing every
/// round — the zero-is-noop contract this workspace uses elsewhere.
#[test]
fn a_zero_interval_disables_the_heartbeat() {
    let mut hb = TurnHeartbeat::default();
    for secs in [0, 300, 10_000] {
        assert!(!hb.due(
            std::time::Duration::from_secs(secs),
            std::time::Duration::ZERO
        ));
    }
}

/// The line names elapsed minutes and the round against the EFFECTIVE cap, so
/// an escalated turn reads as "round 210 of 10000" rather than looking like it
/// has overrun a limit of 40 — which is the confusion #1965 is about.
#[test]
fn the_heartbeat_line_reports_elapsed_and_the_effective_cap() {
    let line = turn_heartbeat_line(std::time::Duration::from_secs(1954), 210, 10_000);
    assert!(line.contains("32m elapsed"), "{line}");
    assert!(line.contains("round 210 of 10000"), "{line}");
    assert!(!line.contains('\u{1b}'), "no ANSI in the pure line: {line}");
    assert!(!line.contains('\n'), "one line: {line}");
}

#[test]
fn pending_plan_nudge_does_not_promote_agent_claims_to_runtime_facts() {
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};
    let ledger = SessionStepLedger::default();
    let claim = "CI gate BLOCKED: cargo absent; Build declined by the operator (exit 40032)";
    ledger.set_plan(&["inspect".into(), claim.into()]);
    ledger.advance();
    for needs_plan_update in [false, true] {
        let nudge = pending_plan_completion_nudge(Some(&ledger), needs_plan_update, None).unwrap();
        assert!(
            !nudge.contains(claim),
            "agent-authored text is not a host reminder: {nudge}"
        );
        assert!(!nudge.contains("cargo absent"), "{nudge}");
        assert!(!nudge.contains("Build declined"), "{nudge}");
        assert!(nudge.contains("agent-maintained"), "{nudge}");
        assert!(nudge.contains("advisory"), "{nudge}");
        assert!(nudge.contains("1/2 unfinished step"), "{nudge}");
        assert!(nudge.contains("step 2"), "{nudge}");
        assert!(nudge.contains("latest operator instruction"), "{nudge}");
        assert!(nudge.contains("update_plan"), "{nudge}");
    }
    assert!(
        plan_block(&ledger).unwrap().contains(claim),
        "the source plan remains inspectable"
    );
}

/// #2718: the shared normal/cap-exit finalizer must append actual Cargo facts,
/// even with optional verification disabled and after another command succeeds.
#[tokio::test]
async fn finalizer_refutes_cargo_success_with_observed_failure() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().to_str().unwrap();
    let mut ledger = self_verify::VerificationLedger::for_turn("refactor", false);
    ledger
        .observe(
            "run_command",
            &serde_json::json!({"command":"cargo check"}),
            false,
            Some(crate::ExecOutcome::Failed),
            root,
        )
        .await;
    ledger
        .observe(
            "run_command",
            &serde_json::json!({"command":"git status"}),
            true,
            Some(crate::ExecOutcome::Passed),
            root,
        )
        .await;
    let text = finalize_final_text(
        "Cargo check green".into(),
        root,
        &crate::Scope::All,
        &capability_check::Evidence::default(),
        None,
        &claim_check::TurnClaims::capture(root, &crate::Scope::All, None),
        &ledger,
    );
    assert!(text.contains("claim check (#2718)"), "{text}");
    assert!(text.contains("Failed in"), "{text}");
}

/// The captured ledger must stay one step per RENDERED line. The handoff is a
/// non-streamed reply (`cap_exit_model_reply` returns `streamed = false`), so
/// the styled surface renders it whole through `render_markdown`, where a bare
/// newline inside a paragraph is a soft break folded into a space
/// (`markdown/emitter.rs`). The older fixtures here are hand-written
/// `1. [ ] …` strings, not `build_plan_block`'s `☐ 1. …` shape, so they never
/// exercised it: measured live on 2026-10-07, the steps of a capped refactor
/// turn came out as one wrapped paragraph. Under `--no-default-features` the
/// renderer is a passthrough and the raw bytes are already one step per line,
/// so the pin holds there too.
#[test]
fn cap_exit_handoff_keeps_one_plan_step_per_rendered_line() {
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};
    use crate::agentic::scratchpad::{ScratchpadStore, SessionScratchpadStore};
    let ledger = SessionStepLedger::default();
    ledger.set_plan(&[
        "inventory".to_string(),
        "verify".to_string(),
        "extract".to_string(),
    ]);
    // ✓ 1. inventory / → 2. verify / ☐ 3. extract: all three glyphs.
    ledger.advance();
    let pad = SessionScratchpadStore::default();
    pad.set("cwd", "/work".to_string());
    let progress = cap_exit_progress(
        None,
        Some(&ledger as &dyn StepLedger),
        Some(&pad as &dyn ScratchpadStore),
    )
    .expect("non-empty progress");
    let text = cap_exit_progress_handoff(25, None, "Next steps: finish.", true, Some(&progress));
    // Wide on purpose: before the fix the folded paragraph fit on ONE physical
    // line, so every position below was equal and the ordering failed.
    let rendered = crate::agentic::render_markdown(
        &text,
        crate::agentic::RenderOpts {
            color: true,
            cols: 200,
        },
    );
    let lines: Vec<&str> = rendered.lines().collect();
    let line_of = |needle: &str| {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} missing from the rendered handoff: {rendered}"))
    };
    let order = [
        line_of("Captured working state:"),
        line_of("Plan:"),
        line_of("inventory"),
        line_of("verify"),
        line_of("extract"),
        line_of("Current state:"),
        line_of("cwd"),
    ];
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "the label, 'Plan:', each step and the state must each land on their \
         own rendered line, in order; got {order:?} in: {rendered}"
    );
}

/// Stage 1: even a cap fallback must include harness facts, not only model claims.
#[test]
fn observed_report_is_present_at_cap_finalization() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let claims = claim_check::TurnClaims::capture(root, &crate::Scope::All, None);
    let out = finalize_final_text(
        cap_exit_fallback(1, None, 0, None),
        root,
        &crate::Scope::All,
        &capability_check::Evidence::default(),
        None,
        &claims,
        &self_verify::VerificationLedger::default(),
    );
    assert!(out.starts_with("## Observed"), "{out}");
    assert!(out.contains("unavailable"), "{out}");
}

/// Live report regression: copied report envelopes in model prose must not
/// impersonate a second harness report, including copies after ordinary prose.
#[test]
fn observed_report_finalization_removes_model_report_copies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let claims = claim_check::TurnClaims::capture(root, &crate::Scope::All, None);
    let copy = "## Observed\n\nReport `invented-cid` — root `workspace`.\n\nFiles since objective/adoption snapshot (LF counts; absence is 0):\n- invented facts\n\n## Model explanation\n\n";
    let out = finalize_final_text(
        format!("{copy}First explanation.\n\n{copy}Second explanation."),
        root,
        &crate::Scope::All,
        &capability_check::Evidence::default(),
        None,
        &claims,
        &self_verify::VerificationLedger::default(),
    );
    assert_eq!(out.matches("## Observed").count(), 1, "{out}");
    assert_eq!(out.matches("## Model explanation").count(), 1, "{out}");
    assert!(!out.contains("invented-cid"), "{out}");
    assert!(out.contains("First explanation."));
    assert!(out.contains("Second explanation."));
}
