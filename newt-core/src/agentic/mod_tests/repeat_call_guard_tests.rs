use super::*;

/// #2637: a confined workspace with one real file, for tests exercising the
/// `ReadRange` memo's content-identity check — it re-verifies against the
/// file's ACTUAL current bytes through the same confined `authorized_read`
/// path a real `read_file` call uses, so a fake/absent file must not be
/// silently accepted as "unchanged". `path` is relative, as `read_file`'s
/// `path` arg is.
fn read_memo_fixture(
    path: &str,
    content: &str,
) -> (tempfile::TempDir, String, crate::caveats::Caveats) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&full, content).unwrap();
    let caveats = crate::confined_exec::workspace_confined_caveats(&root);
    let workspace = root.to_string_lossy().into_owned();
    (dir, workspace, caveats)
}

#[test]
fn clean_build_guidance_appends_one_exact_user_message_on_the_third_call() {
    let original = serde_json::json!({"role": "user", "content": "build the project"});
    let mut messages = vec![original.clone()];
    let mut watch = crate::loop_watch::CleanBuildWatch::default();
    let command = serde_json::json!({"command": "cargo clean && cargo check"});
    for args in [serde_json::json!({}), serde_json::json!({"command": 42})] {
        append_clean_build_warning(&mut messages, &mut watch, &args, None).unwrap();
    }
    for _ in 0..2 {
        append_clean_build_warning(&mut messages, &mut watch, &command, None).unwrap();
        assert_eq!(messages, vec![original.clone()]);
    }
    append_clean_build_warning(&mut messages, &mut watch, &command, None).unwrap();
    let expected = vec![
        original,
        serde_json::json!({
            "role": "user",
            "content": "[loop-guidance] This turn has repeatedly discarded build output and immediately \
        rebuilt it. Your edits already invalidate the build fingerprint, so the \
        clean step buys nothing and costs a full rebuild each time. Drop it and \
        build directly.",
        }),
    ];
    assert_eq!(messages, expected);
    append_clean_build_warning(&mut messages, &mut watch, &command, None).unwrap();
    assert_eq!(messages, expected, "guidance is capped at once per turn");
}

#[test]
fn loop_owned_tool_results_share_one_complete_spill_block() {
    let mut repeated = Vec::new();
    {
        let mut display = display::ToolDisplay::new(&mut repeated, false, 80, 3, false);
        present_synthetic_tool_result(
            &mut display,
            "find",
            &serde_json::json!({"path": ".", "name": "*.rs", "type": "f"}),
            std::path::Path::new("."),
            "a.rs\nb.rs\nc.rs\nd.rs",
        );
    }
    // #1973 declared amendment: this golden MOVED from tail-only
    // (b.rs/c.rs/d.rs) to head+tail (a.rs .. d.rs) — see the module doc on
    // `display::spill_view_lines` for why tail-only is a defect, not a
    // style choice. This test's own property (one complete spill block per
    // result, not split across a repeat-call boundary) is unaffected.
    assert_eq!(
        String::from_utf8(repeated).unwrap(),
        "⚙  find: . (name=*.rs, type=f)\n\
             ▒ a.rs\n\
             ▲ 2 lines hidden  [/spill N raises this view]\n\
             ▓ d.rs\n\
             …\n"
    );

    let mut budget = Vec::new();
    {
        let mut display = display::ToolDisplay::new(&mut budget, false, 80, 3, false);
        present_synthetic_tool_result(
            &mut display,
            "tokens_left",
            &serde_json::json!({}),
            std::path::Path::new("."),
            "context budget: 75% remaining",
        );
    }
    assert_eq!(
        String::from_utf8(budget).unwrap(),
        "⚙  get_context_remaining: \n\
             ▒ context budget: 75% remaining\n\
             …\n"
    );
}

/// disclosure-gate-live-path (#5): the repeat-steer synthetic message is a
/// model-ingress path (re-injected as a `{"role":"tool"}` turn) that
/// interpolates the first line of a FAILED tool result. A registered session
/// secret that lands in that line must be value-filtered before the steer
/// reaches the model — the convergence-audit falsification. This FAILS on the
/// pre-fix code (the raw secret is interpolated verbatim).
#[test]
fn repeat_steer_value_filters_a_registered_session_secret() {
    let secret = "CANARY-repeatsteer-9f3a2b71";
    let mut filter = crate::ocap::DisclosureFilter::new();
    filter.register(secret);
    let _guard = crate::ocap::scoped_session_disclosure(filter);

    let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({"command": "cat /etc/token"});
    // The tool FAILS with the secret echoed in the first line of its result.
    g.record(
        "run_command",
        &args,
        false,
        &format!("error: unexpected token {secret} in response"),
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    // The model repeats the exact call → the steer re-injects the prior line.
    let steer = g
        .repeat_steer("run_command", &args)
        .expect("steers on repeat");
    assert!(
        !steer.contains(secret),
        "the repeat-steer message leaked a registered session secret: {steer}"
    );
    assert!(
        steer.contains("[REDACTED]"),
        "the secret should be redacted in place: {steer}"
    );
    // The steer is still useful (names the tool + the do-not-repeat guidance).
    assert!(steer.contains("already called"), "{steer}");
}

#[test]
fn short_circuits_exact_repeat_without_discouraging_the_tool() {
    let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({"command": "python3 script.py"});
    // First sight of the call → let it run (no steer).
    assert!(g.repeat_steer("run_command", &args).is_none());
    // After a failure, an exact repeat is steered, quoting the prior error.
    g.record(
        "run_command",
        &args,
        false,
        "error: command exited 1",
        Some(crate::ExecOutcome::Failed),
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    let s = g.repeat_steer("run_command", &args).expect("repeat steers");
    assert!(s.contains("already called"), "{s}");
    assert!(s.contains("error: command exited 1"), "{s}");
    // Two ordinary script failures do not establish that the shell is broken.
    g.record(
        "run_command",
        &serde_json::json!({"command": "python3 other.py"}),
        false,
        "error: command exited 1",
        Some(crate::ExecOutcome::Failed),
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    let s2 = g.repeat_steer("run_command", &args).expect("still steers");
    assert!(s2.contains("error: command exited 1"), "{s2}");
    for stale in ["stop using", "embedded tools", "git", "different arguments"] {
        assert!(!s2.contains(stale), "misleading advice {stale:?}: {s2}");
    }
    assert!(s2.contains("correct its cause before retrying"), "{s2}");
    assert_eq!(g.total_failures(), 2, "failure accounting is unchanged");
}

#[tokio::test]
async fn successful_script_edit_releases_the_exact_failed_command() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let script = root.join("script.py");
    std::fs::write(&script, "print('broken')\n").unwrap();
    let caveats = crate::confined_exec::workspace_confined_caveats(&root);
    let command = serde_json::json!({"command": "python3 script.py"});
    let mut guard = RepeatCallGuard::default();
    guard.record(
        "run_command",
        &command,
        false,
        "error: command exited 1",
        Some(crate::ExecOutcome::Failed),
        ReadScope {
            workspace: &root.to_string_lossy(),
            caveats: &caveats,
        },
    );
    assert!(guard.repeat_steer("run_command", &command).is_some());

    // Exercise actual file dispatch, not a success-shaped fake edit receipt.
    // A failed edit cannot release the memo; a completed repair can.
    for (old, succeeds) in [("not present", false), ("broken", true)] {
        let args = serde_json::json!({
            "path": "script.py", "old_string": old, "new_string": "repaired"
        });
        let result = tools::execute_tool_with_collaborators(
            "edit_file",
            &args,
            &root.to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            tools::ToolCollaborators::default(),
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let ok = tools::tool_ok(&result, None);
        assert_eq!(ok, succeeds, "{result}");
        guard.record(
            "edit_file",
            &args,
            ok,
            &result,
            None,
            ReadScope {
                workspace: &root.to_string_lossy(),
                caveats: &caveats,
            },
        );
        assert_eq!(
            guard.repeat_steer("run_command", &command).is_none(),
            succeeds,
            "only the completed repair permits the identical command"
        );
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            if succeeds {
                "print('repaired')\n"
            } else {
                "print('broken')\n"
            }
        );
    }
}

#[test]
fn ignores_successes_and_distinct_calls() {
    // #2637: restored to its pre-#2555 form — a bare successful read_file is
    // NEVER refused on repeat (doctrine: never refuse a successful re-read).
    let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
    let mut g = RepeatCallGuard::default();
    let a = serde_json::json!({"path": "f.rs"});
    g.record(
        "read_file",
        &a,
        true,
        "file contents",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    ); // success → not remembered as a refusal
    assert!(g.repeat_steer("read_file", &a).is_none());
    // A failure under different args does not short-circuit a distinct call.
    let b = serde_json::json!({"path": "g.rs"});
    g.record(
        "read_file",
        &b,
        false,
        "error reading g.rs",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(
        g.repeat_steer("read_file", &a).is_none(),
        "distinct args still run"
    );
    assert!(g.repeat_steer("read_file", &b).is_some());
}

#[test]
fn steers_no_result_repeats_on_second_issuance() {
    // #718: a success-shaped no-result that the model re-issues byte-for-byte
    // is steered on its 2nd call — distinct from a hard failure (no escalation),
    // distinct from a genuine success (which is never steered).
    let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
    let mut g = RepeatCallGuard::default();

    // recall "no matches" — first sight runs; record it; the identical 2nd
    // issuance is steered before re-execution.
    let q = serde_json::json!({"query": "newt-tui PyO3 bindings"});
    assert!(
        g.repeat_steer("recall", &q).is_none(),
        "first recall must run"
    );
    g.record(
        "recall",
        &q,
        true,
        "no matches in past conversations for \"newt-tui PyO3 bindings\" — try different keywords.",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    let s = g
        .repeat_steer("recall", &q)
        .expect("2nd identical recall steers");
    assert!(s.contains("no matches"), "{s}");
    assert!(
        s.contains("resume_context"),
        "recall steer points at resume_context: {s}"
    );
    assert!(
        !s.contains("stop using"),
        "a no-result is not a hard failure — no escalation: {s}"
    );

    // state_get "no such key" — same: 2nd identical probe is steered.
    let k = serde_json::json!({"key": "current_task"});
    assert!(g.repeat_steer("state_get", &k).is_none());
    g.record(
        "state_get",
        &k,
        true,
        "no such key: current_task",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(
        g.repeat_steer("state_get", &k).is_some(),
        "2nd identical state_get steers"
    );

    // plan_get empty ledger — same: the second identical read is steered
    // toward creating the missing plan instead of polling the empty ledger.
    let empty_plan_args = serde_json::json!({});
    assert!(g.repeat_steer("plan_get", &empty_plan_args).is_none());
    g.record(
        "plan_get",
        &empty_plan_args,
        true,
        "no active plan — if this is multi-step work, call update_plan next",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    let plan_steer = g
        .repeat_steer("plan_get", &empty_plan_args)
        .expect("2nd identical empty plan_get steers");
    assert!(plan_steer.contains("update_plan"), "{plan_steer}");

    // #2637: a BARE read_file is still NEVER steered on repeat (covered in
    // depth by the cached-read tests below).
    let f = serde_json::json!({"path": "f.rs"});
    g.record(
        "read_file",
        &f,
        true,
        "file contents",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(g.repeat_steer("read_file", &f).is_none());

    // A no-result under DIFFERENT args is a distinct call — let it run.
    let q2 = serde_json::json!({"query": "something else entirely"});
    assert!(
        g.repeat_steer("recall", &q2).is_none(),
        "distinct recall args still run"
    );
}

#[test]
fn steers_duplicate_successful_web_fetch() {
    let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
    let mut g = RepeatCallGuard::default();
    let issue = serde_json::json!({
        "url": "https://github.com/Gilamonster-Foundation/newt-agent/issues/771"
    });

    assert!(
        g.repeat_steer("web_fetch", &issue).is_none(),
        "first fetch must run"
    );
    g.record(
        "web_fetch",
        &issue,
        true,
        "# Issue\n\nbody",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    let steer = g
        .repeat_steer("web_fetch", &issue)
        .expect("2nd identical successful fetch steers");
    assert!(steer.contains("already observed"), "{steer}");
    assert!(steer.contains("`web_fetch`"), "{steer}");
    assert!(
        steer.contains("https://github.com/Gilamonster-Foundation/newt-agent/issues/771"),
        "{steer}"
    );
    assert!(
        g.repeat_steer(
            "web_fetch",
            &serde_json::json!({"url": "https://github.com/hartsock/scrybe"})
        )
        .is_none(),
        "distinct URLs still run"
    );

    // #2637: a bare read_file is still never steered.
    let file = serde_json::json!({"path": "src/lib.rs"});
    g.record(
        "read_file",
        &file,
        true,
        "file contents",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(
        g.repeat_steer("read_file", &file).is_none(),
        "bare reads are still not steered"
    );
}

#[test]
fn steers_duplicate_successful_read_only_run_command() {
    let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({
        "command": "grep -n 'help_lines' newt-tui/src/lib.rs"
    });

    assert!(
        g.repeat_steer("run_command", &args).is_none(),
        "first grep should run"
    );
    g.record(
        "run_command",
        &args,
        true,
        "9439:fn help_lines() -> &'static [&'static str] {",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );

    let steer = g
        .repeat_steer("run_command", &args)
        .expect("second identical grep should steer");
    assert!(steer.contains("already observed"), "{steer}");
    assert!(steer.contains("read-only shell probe"), "{steer}");
    assert!(steer.contains("`run_command`"), "{steer}");
    assert!(steer.contains("grep -n"), "{steer}");
    assert!(steer.contains("Do NOT repeat"), "{steer}");
}

#[test]
fn does_not_steer_successful_write_capable_run_command() {
    let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({"command": "cargo test -p newt-tui"});

    g.record(
        "run_command",
        &args,
        true,
        "test result: ok",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );

    assert!(
        g.repeat_steer("run_command", &args).is_none(),
        "successful build/test commands are still repeatable"
    );
}

#[test]
fn classifier_leaves_ordinary_successes_repeatable() {
    // #2637: a bare read (no offset/limit) is ALSO an ordinary, non-refusing
    // success now — it is never classified into a steering memo at all
    // (`repeat_calls.cached_read` handles the exact-repeat case separately,
    // ahead of `repeat_steer`). Only truly unclassified shapes belong here.
    let (_dir, workspace, caveats) = read_memo_fixture("src/lib.rs", "file contents");
    let file = serde_json::json!({"path": "src/lib.rs", "offset": 0});
    assert_eq!(
        RepeatCallGuard::classify_repeat_memo("read_file", &file, true, "file contents", None),
        None
    );

    let tests = serde_json::json!({"command": "cargo test -p newt-core"});
    assert_eq!(
        RepeatCallGuard::classify_repeat_memo("run_command", &tests, true, "test result: ok", None),
        None
    );

    let mut g = RepeatCallGuard::default();
    g.record(
        "read_file",
        &file,
        true,
        "file contents",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    g.record(
        "run_command",
        &tests,
        true,
        "test result: ok",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(
        g.repeat_memos.is_empty(),
        "an explicit-range read and an ordinary command stay repeatable"
    );
}

/// #2637: a bare `read_file` (no explicit `offset`/`limit`) IS memoized, but
/// an exact repeat is NEVER refused — `cached_read` re-verifies the file's
/// ACTUAL current bytes (a real confined read, not trusting recorded
/// workspace-changing bookkeeping) and, unchanged, serves the earlier content
/// silently.
#[test]
fn bare_read_file_repeat_is_served_from_cache_not_refused() {
    let (_dir, workspace, caveats) = read_memo_fixture("src/lib.rs", "file contents");
    let file = serde_json::json!({"path": "src/lib.rs"});
    assert!(matches!(
        RepeatCallGuard::classify_repeat_memo("read_file", &file, true, "file contents", None),
        Some(RepeatMemo::ReadRange { .. })
    ));

    let mut g = RepeatCallGuard::default();
    assert!(
        g.cached_read(
            "read_file",
            &file,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats
            }
        )
        .is_none(),
        "first read has no memo yet"
    );
    g.record(
        "read_file",
        &file,
        true,
        "file contents",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    // The doctrine: never refuse. The exact repeat is never steered...
    assert!(
        g.repeat_steer("read_file", &file).is_none(),
        "a bare read_file repeat is never refused"
    );
    // ...it is served silently from the memo instead, with the ORIGINAL content.
    let cached = g
        .cached_read(
            "read_file",
            &file,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats,
            },
        )
        .expect("unchanged file content serves from cache");
    assert_eq!(cached, "file contents");
}

/// #2637: the file changing on disk invalidates the memo — freshness is
/// proven against the file's ACTUAL bytes each time, never inferred from
/// "no recorded workspace-changing tool ran since".
#[test]
fn cached_read_is_dropped_when_the_file_actually_changes() {
    let (_dir, workspace, caveats) = read_memo_fixture("src/lib.rs", "file contents");
    let file = serde_json::json!({"path": "src/lib.rs"});
    let mut g = RepeatCallGuard::default();
    g.record(
        "read_file",
        &file,
        true,
        "file contents",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(g
        .cached_read(
            "read_file",
            &file,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats
            }
        )
        .is_some());

    // An out-of-band edit — no tool this guard observes ran.
    std::fs::write(
        std::path::Path::new(&workspace).join("src/lib.rs"),
        "edited",
    )
    .unwrap();

    assert!(
        g.cached_read(
            "read_file",
            &file,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats
            }
        )
        .is_none(),
        "changed content must never be served as if unchanged"
    );
    // Still never a refusal — the caller falls through to a real read.
    assert!(
        g.repeat_steer("read_file", &file).is_none(),
        "a stale memo is dropped, not turned into a refusal"
    );
}

/// #2637 review P1 regression: the memo's identity must describe exactly the
/// bytes SERVED (`result`), never a separate reread of the path taken at
/// record time. Before the fix, `classify_repeat_memo` called
/// `read_scope.content_id_of(path)` — an independent disk read — to mint the
/// id. That opens a window: the real tool call reads A and returns
/// `result = A`, then (before `record` runs) an out-of-band edit lands B on
/// disk, and the old code stored `(hash(B), A)`. A LATER read, with the file
/// still holding B unchanged, would match `hash(B)` and serve stale `A`.
/// This drives exactly that race and asserts the memo is never confused: the
/// id is bound to `result` (`A`), so it can only ever match a disk read that
/// still shows `A`, and B on disk is correctly seen as a mismatch, not a hit.
#[test]
fn content_id_binds_to_the_served_result_not_a_reread_at_record_time() {
    let (_dir, workspace, caveats) = read_memo_fixture("race.rs", "A");
    let file = serde_json::json!({"path": "race.rs"});
    let mut g = RepeatCallGuard::default();
    // The tool's real read already happened and returned "A" as `result`.
    // Simulate the out-of-band edit landing BEFORE `record` classifies it —
    // the exact window the old reread-based id minting raced.
    std::fs::write(std::path::Path::new(&workspace).join("race.rs"), "B").unwrap();
    g.record(
        "read_file",
        &file,
        true,
        "A",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    // Disk still holds "B" (untouched since the edit above) — this is NOT
    // "unchanged since A", so it must never be served from the memo.
    assert!(
        g.cached_read(
            "read_file",
            &file,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats
            }
        )
        .is_none(),
        "the memo's id must bind to the served result (\"A\"), not a reread \
         taken at record time — serving \"A\" for the current \"B\" would be \
         exactly the stale-content bug this regression guards"
    );
}

/// #2637 review P1: a cache HIT must go through the same disclosure fence a
/// real tool result takes, not `push_tool_resolution` unfiltered — the exact
/// gap the review named (`newt-core/src/agentic/mod.rs:4062/:4474`, pre-fix).
/// The fix passes the cached content through `maybe_offload_tool_result` (the
/// same chokepoint `smart_harness::tool_result` uses) before `push_tool_resolution`;
/// this pins that a session secret embedded in the memoized content is redacted.
#[test]
fn cache_hit_content_passes_through_the_disclosure_fence() {
    let secret = "CANARY-cachehit-4d1e9a02";
    let mut filter = crate::ocap::DisclosureFilter::new();
    filter.register(secret);
    let _guard = crate::ocap::scoped_session_disclosure(filter.clone());

    let secret_content = format!("content with {secret} inside");
    let (_dir, workspace, caveats) = read_memo_fixture("secret_file.txt", &secret_content);
    let file = serde_json::json!({"path": "secret_file.txt"});
    let mut g = RepeatCallGuard::default();
    g.record(
        "read_file",
        &file,
        true,
        &secret_content,
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    let content = g
        .cached_read(
            "read_file",
            &file,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats,
            },
        )
        .expect("unchanged content serves from cache");
    assert!(
        content.contains(secret),
        "sanity: the raw cached content names the secret before filtering: {content}"
    );
    let filtered = maybe_offload_tool_result("read_file", content, false, None, Some(&filter));
    assert!(
        !filtered.contains(secret),
        "a cache hit must pass the live disclosure fence, exactly like \
         a real tool result: {filtered}"
    );
}

/// #2555: ANY workspace change releases the memo (not only an edit of the
/// SAME file) — the model's next read is asking about the tree it just
/// changed, so "nothing has changed since" no longer holds.
#[test]
fn bare_read_file_memo_is_released_by_any_workspace_change() {
    let (_dir, workspace, caveats) = read_memo_fixture("a.rs", "file contents");
    let file = serde_json::json!({"path": "a.rs"});
    let mut g = RepeatCallGuard::default();
    g.record(
        "read_file",
        &file,
        true,
        "file contents",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(g
        .cached_read(
            "read_file",
            &file,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats
            }
        )
        .is_some());
    g.record(
        "write_file",
        &serde_json::json!({"path": "b.rs", "content": "x"}),
        true,
        "wrote b.rs",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(
        g.cached_read(
            "read_file",
            &file,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats
            }
        )
        .is_none(),
        "a workspace write to a DIFFERENT file still releases the memo"
    );
}

/// #2555: the compaction release hook — `release_read_memos` drops every
/// `ReadRange` memo (what `record_compaction_artifact` and the Responses
/// loop's `Compacted` outcome call once a compaction actually commits) but
/// leaves failure/no-result/evidence memos untouched, since those describe
/// an outcome compaction does not invalidate.
#[test]
fn release_read_memos_drops_only_read_range_memos() {
    let (_dir, workspace, caveats) = read_memo_fixture("a.rs", "file contents");
    let mut g = RepeatCallGuard::default();
    let file = serde_json::json!({"path": "a.rs"});
    g.record(
        "read_file",
        &file,
        true,
        "file contents",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    let failing = serde_json::json!({"command": "cargo check"});
    g.record(
        "run_command",
        &failing,
        false,
        "error: boom",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(g
        .cached_read(
            "read_file",
            &file,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats
            }
        )
        .is_some());
    assert!(g.repeat_steer("run_command", &failing).is_some());

    g.release_read_memos();

    assert!(
        g.cached_read(
            "read_file",
            &file,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats
            }
        )
        .is_none(),
        "compaction releases the read-range memo — a post-compaction re-read \
         is legitimate because the working-set card no longer holds the page"
    );
    assert!(
        g.repeat_steer("run_command", &failing).is_some(),
        "compaction must not release an unrelated failure memo"
    );
}

#[test]
fn workflow_error_fingerprint_captures_cargo_location() {
    let output = r#"
error[E0425]: cannot find value `SECTION_PROMPT_TOKENS` in this scope
   --> newt-tui/src/help_sections.rs:523:22
    |
523 |         lines: SECTION_PROMPT_TOKENS,
    |                ^^^^^^^^^^^^^^^^^^^^^ help: a static with a similar name exists: `SECTION_PROMPT`
"#;

    let fp = build_error_fingerprint(output).expect("cargo error should fingerprint");

    assert!(fp.contains("newt-tui/src/help_sections.rs:523:22"), "{fp}");
    assert!(fp.contains("error[E0425]"), "{fp}");
    assert!(fp.contains("SECTION_PROMPT_TOKENS"), "{fp}");
}

#[test]
fn initiative_action_forcing_nudge_fires_at_the_budget_and_resets_on_a_write() {
    // The action-forcing nudge fires once the model has spent the
    // initiative level's budget of consecutive read-only rounds, and a
    // workspace write resets the counter. This is what gives the OpenAI-chat
    // loop (which had no read-only nudge) a push from reading to acting.
    let mut state = WorkflowRuntimeState {
        initiative: crate::initiative::Initiative::Eager, // budget 1
        ..Default::default()
    };
    // Nothing spent yet → no nudge.
    assert!(state.action_forcing_nudge(Some(5), None, None).is_none());
    // One read-only round → at the Eager budget → fires.
    state.record_round_outcome(false, false);
    let nudge = state
        .action_forcing_nudge(Some(5), None, None)
        .expect("eager initiative must force action after one read-only round");
    assert!(nudge.contains("edit_file or write_file"), "{nudge}");
    // Firing resets the counter; a follow-up read-only round re-accumulates.
    assert!(state.action_forcing_nudge(Some(5), None, None).is_none());
    state.record_round_outcome(false, false);
    assert!(state.action_forcing_nudge(Some(5), None, None).is_some());
    // A workspace-write round clears the counter entirely.
    state.record_round_outcome(true, true);
    assert!(
        state.action_forcing_nudge(Some(5), None, None).is_none(),
        "a write must reset the read-only streak"
    );

    // Measured initiative (the default) preserves the historical budget of 3.
    let mut standard = WorkflowRuntimeState::default();
    for _ in 0..2 {
        standard.record_round_outcome(false, false);
    }
    assert!(
        standard.action_forcing_nudge(Some(5), None, None).is_none(),
        "measured must not fire before 3 read-only rounds"
    );
    standard.record_round_outcome(false, false);
    assert!(standard.action_forcing_nudge(Some(5), None, None).is_some());
}

#[test]
fn progress_horizon_override_widens_the_recent_progress_window() {
    for (horizon, expected) in [(None, false), (Some(6), true)] {
        let mut state = WorkflowRuntimeState::default();
        state.set_progress_horizon(horizon);
        state.record_round_outcome(false, true);
        for _ in 0..4 {
            state.record_round_outcome(false, false);
        }
        assert_eq!(state.admit_round(2, &mut 2, 4), expected);
    }
}

#[test]
fn workspace_write_classifier_is_narrow() {
    assert!(is_workspace_write_call("edit_file"));
    assert!(is_workspace_write_call("write_file"));
    assert!(!is_workspace_write_call("run_command"));
    assert!(!is_workspace_write_call("read_file"));
}

/// #2618/F46: `may_change_workspace` gates ELIGIBILITY for a
/// `progress_workspace_state` before/after snapshot, not progress itself — a
/// `run_command` that wrote via shell redirection must be ELIGIBLE for that
/// snapshot; `is_workspace_write_call` (the narrow, name-only classifier used
/// for the headless write-trajectory) never would, by design, which was the
/// bug: the brake's snapshot selector used to gate eligibility on that
/// predicate instead of `may_change_workspace`. Whether the round actually
/// AWARDS progress is a separate question, answered by comparing the
/// before/after `ProgressSnapshot`s in `WorkflowRuntimeState::record_workspace_change`
/// (pinned directly, on the real production snapshot function, by
/// `progress_workspace_state_snapshots_a_real_shell_redirect_as_a_change` in
/// `http_smart_completion.rs`) — not by this predicate alone.
#[test]
fn may_change_workspace_counts_a_run_command_redirect_write() {
    let args = serde_json::json!({"command": "head -n 9309 x | tail -n 2183 > out.rs"});
    assert!(may_change_workspace("run_command", &args));
}

/// A pure read command must still be INELIGIBLE for a workspace snapshot
/// under the same predicate the brake's snapshot selector now gates on.
#[test]
fn may_change_workspace_excludes_a_pure_read_command() {
    let args = serde_json::json!({"command": "grep -n x f"});
    assert!(!may_change_workspace("run_command", &args));
}

#[test]
fn no_result_reason_classifies_and_routes() {
    // recall / state_get no-result prefixes classify…
    assert!(RepeatCallGuard::no_result_reason(
        "recall",
        "no matches in past conversations for \"x\" — try different keywords."
    )
    .is_some_and(|r| r.contains("no matches") && r.contains("resume_context")));
    assert!(
        RepeatCallGuard::no_result_reason("state_get", "no such key: current_task")
            .is_some_and(|r| r.contains("not set"))
    );
    assert!(
        RepeatCallGuard::no_result_reason("plan_get", "no active plan — call update_plan")
            .is_some_and(|r| r.contains("update_plan"))
    );
    // …a real success with content does not.
    assert!(
        RepeatCallGuard::no_result_reason("recall", "3 match(es) in past conversations").is_none()
    );
    assert!(RepeatCallGuard::no_result_reason("read_file", "file contents").is_none());

    // A recall ERROR (ok=false) goes through the FAILURE path, not no-result
    // classification: it lands in repeat_memos as escalation-eligible.
    let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
    let mut g = RepeatCallGuard::default();
    let q = serde_json::json!({"query": "x"});
    g.record(
        "recall",
        &q,
        false,
        "error: index unavailable",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(matches!(
        g.repeat_memos.get(&RepeatCallGuard::key("recall", &q)),
        Some(RepeatMemo::Failure { first_line, .. }) if first_line == "error: index unavailable"
    ));
}

#[test]
fn first_line_caps_and_takes_first() {
    assert_eq!(first_line("one\ntwo\nthree"), "one");
    assert_eq!(first_line(""), "");
    assert_eq!(first_line(&"x".repeat(500)).chars().count(), 200);
}

/// **#1969's second consumer, repaired for free.**
///
/// `RepeatCallGuard` memoizes a `Failure` only for `!ok`, and `ok` came from
/// a prefix test that read every failing compile as a success. So the guard
/// that exists to stop a model re-issuing a dead call was silently disabled
/// for the single most common failing call in a coding session — the build.
///
/// It is not a change to the guard. It is the guard finally being told the
/// truth about the outcome.
#[test]
fn a_failing_build_now_memoizes_and_steers_the_repeat() {
    let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({"command": "cargo check -p thing", "cwd": "/w"});
    // What the shell path renders for a failing compile since #1969.
    let result = "error: command exited 101\nerror[E0308]: mismatched types\n";
    let ok = crate::agentic::tools::tool_result_ok(result);
    assert!(
        !ok,
        "the outcome bit is still wrong, so this proves nothing"
    );

    assert!(g.repeat_steer("run_command", &args).is_none());
    g.record(
        "run_command",
        &args,
        ok,
        result,
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    let steer = g
        .repeat_steer("run_command", &args)
        .expect("a repeated failing build is not steered");
    assert!(steer.contains("already called"), "{steer}");
}

/// The twin: a PASSING build stays repeatable. Builds are re-run constantly
/// and for good reason, so the repair must not memoize success.
#[test]
fn a_passing_build_stays_repeatable() {
    let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({"command": "cargo check -p thing", "cwd": "/w"});
    let result = "    Finished dev [unoptimized] target(s) in 0.04s\n";
    let ok = crate::agentic::tools::tool_result_ok(result);
    assert!(ok, "a successful build is being read as a failure");
    g.record(
        "run_command",
        &args,
        ok,
        result,
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(
        g.repeat_steer("run_command", &args).is_none(),
        "a passing build was memoized; re-running a build is legitimate"
    );
}

/// Wrong-path probes remain failed tool results and cannot earn progress or
/// make a previously observed compiler error count as new evidence again.
#[test]
fn an_fs_producer_failure_does_not_count_as_progress() {
    let compile_error = "error[E0425]: cannot find value `X` in this scope\n --> src/main.rs:1:1\n";
    let enoent_read = "error: reading src/missing.rs: No such file or directory (os error 2)";
    let args = serde_json::json!({"command": "cargo check"});
    let mut state = WorkflowRuntimeState::default();
    assert!(state.record_observation("run_command", &args, compile_error, false, None, None));
    assert!(!state.record_observation(
        "read_file",
        &serde_json::json!({"path": "src/missing.rs"}),
        enoent_read,
        false,
        None,
        None,
    ));
    assert!(!state.record_observation("run_command", &args, compile_error, false, None, None));
}

#[test]
fn an_os_permission_denial_requires_a_failed_probe() {
    assert!(run_command_result_is_denial(
        "run_command",
        false,
        "sh: ./deploy.sh: Permission denied"
    ));
    assert!(!run_command_result_is_denial(
        "run_command",
        true,
        "Permission denied"
    ));
}

#[test]
fn creating_a_plan_invalidates_the_empty_plan_read_memo() {
    let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
    let mut guard = RepeatCallGuard::default();
    let args = serde_json::json!({});
    guard.record(
        "plan_get",
        &args,
        true,
        "no active plan",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(guard.repeat_steer("plan_get", &args).is_some());
    guard.record(
        "update_plan",
        &serde_json::json!({"plan": [{"step": "inspect"}]}),
        true,
        "<plan>inspect</plan>",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(
        guard.repeat_steer("plan_get", &args).is_none(),
        "a fresh plan must be readable in the same turn"
    );
}

/// The actual refusal renderer still returns a failed tool result; a host
/// binary that needs an exec grant continues to ground a denial claim.
#[test]
fn the_carried_userland_refusal_remains_a_failed_tool_result() {
    let envelope = serde_json::json!({
        "exit_code": 127,
        "stdout": "",
        "stderr": "error: command not found: cargo\n",
    });
    let rendered = tools::absent_binary_refusal(&envelope, &crate::caveats::Scope::none())
        .expect("a 127 with no denials renders the refusal");
    assert!(rendered.contains(tools::ABSENT_BINARY_MARKER), "{rendered}");
    assert!(!tools::tool_result_ok(&rendered));
    assert_eq!(
        run_command_result_is_denial("run_command", false, &rendered),
        !rendered.contains(tools::NOT_ON_HOST_MARKER),
    );
}

/// Ground denial accounting against the renderer for a binary outside the
/// filesystem grant, without turning the failure into a prescribed repair.
#[test]
fn a_kernel_refused_binary_remains_a_denied_tool_result() {
    let exe = std::env::current_exe().expect("the running test binary exists");
    let exe = exe.display().to_string();
    let envelope = serde_json::json!({
        "exit_code": 126,
        "stdout": "",
        "stderr": format!("brush: failed to execute command '{exe}': Permission denied (os error 13)\n"),
    });
    let rendered = tools::kernel_refused_binary(&exe, &envelope, &crate::caveats::Scope::none())
        .expect("a 126 outside the read grant renders the refusal");
    assert!(!tools::tool_result_ok(&rendered));
    assert!(run_command_result_is_denial(
        "run_command",
        false,
        &rendered
    ));
}

/// #2374 review A, result-aware mode: only a real workspace change clears a
/// failure memo. A read-only probe or a harness-state call keeps it; a
/// mutating command or a write clears it.
#[test]
fn result_aware_mode_clears_a_failure_memo_only_on_a_workspace_change() {
    let check = serde_json::json!({"command": "cargo test"});
    let wrong = [
        ("run_command", serde_json::json!({"command": "ls"}), false),
        ("state_get", serde_json::json!({"key": "k"}), false),
        ("plan_get", serde_json::json!({}), false),
        ("code_search", serde_json::json!({"query": "retry"}), false),
        (
            "run_command",
            serde_json::json!({"command": "touch fixed"}),
            true,
        ),
        ("write_file", serde_json::json!({"path": "a.txt"}), true),
        ("delete_file", serde_json::json!({"path": "a.txt"}), true),
        // Round three, item 4: an alias is classified as the tool it reaches.
        // Rewrites to inert tools and shell aliases running a read keep the
        // memo; corrective aliases only return coaching text.
        ("get_plan", serde_json::json!({}), false),
        ("show_plan", serde_json::json!({}), false),
        ("read_plan", serde_json::json!({}), false),
        ("find_tool", serde_json::json!({"query": "x"}), false),
        ("list_tools", serde_json::json!({}), false),
        ("resume", serde_json::json!({}), false),
        ("recap", serde_json::json!({}), false),
        ("ask_user", serde_json::json!({"question": "?"}), false),
        ("bash", serde_json::json!({"command": "ls"}), false),
        ("sh", serde_json::json!({"command": "cat a.txt"}), false),
        ("exec", serde_json::json!({"command": "git status"}), false),
        ("str_replace", serde_json::json!({"path": "a.txt"}), false),
        ("apply_patch", serde_json::json!({"path": "a.txt"}), false),
        ("edit", serde_json::json!({"path": "a.txt"}), false),
        ("create_file", serde_json::json!({"path": "a.txt"}), false),
        ("cat", serde_json::json!({"path": "a.txt"}), false),
        ("open_file", serde_json::json!({"path": "a.txt"}), false),
        ("delete", serde_json::json!({"path": "a.txt"}), false),
        ("mkdir", serde_json::json!({"path": "d"}), false),
        ("bash", serde_json::json!({"command": "touch fixed"}), true),
    ]
    .into_iter()
    .filter_map(|(name, args, clears)| {
        let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
        let mut guard = RepeatCallGuard::default();
        guard.record(
            "run_command",
            &check,
            false,
            "error: command exited 101",
            None,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats,
            },
        );
        guard.record(
            name,
            &args,
            true,
            "ok",
            None,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats,
            },
        );
        (guard.repeat_steer("run_command", &check).is_none() != clears)
            .then(|| format!("{name} {args}: clears={}", !clears))
    })
    .collect::<Vec<_>>();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

// ---- U4b: the no-progress brake (state machine; the loop wiring is covered by
// the headless_cli end-to-end tests). Pure: no filesystem, no clock.
//
// The gate runs at the START of every round after the first and counts the
// round that just completed unless it made progress, so a round that `continue`s
// (narration nudge, id-less re-ask) is counted too. `record_round_outcome` only
// reports progress (a write, or a passing lifecycle test/check).

fn no_progress_state(steer_after: usize, stop_after: usize) -> WorkflowRuntimeState {
    WorkflowRuntimeState {
        no_progress: crate::initiative::NoProgressRounds {
            steer_after,
            stop_after,
        },
        ..Default::default()
    }
}

/// The verdict at one round start, as a word.
fn gate(state: &mut WorkflowRuntimeState, steer_allowed: bool) -> &'static str {
    match state.no_progress_verdict(steer_allowed) {
        NoProgress::Continue => "continue",
        NoProgress::Steer(_) => "steer",
        NoProgress::Stop => "stop",
    }
}

#[test]
fn a_write_then_idle_rounds_are_steered_once_then_stopped() {
    let mut state = no_progress_state(2, 3);
    state.record_round_outcome(true, true); // the successful write arms the brake
    assert_eq!(gate(&mut state, true), "continue"); // counts the write round: progress
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "continue"); // 1 idle round
    state.record_round_outcome(false, false);
    let NoProgress::Steer(text) = state.no_progress_verdict(true) else {
        panic!("2 idle rounds must steer");
    };
    assert!(text.contains("2 rounds produced no new evidence"), "{text}");
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "stop"); // 3 idle rounds; the steer was sent once
    assert!(
        state.no_progress_notice().contains("3 consecutive rounds"),
        "{}",
        state.no_progress_notice()
    );
}

#[test]
fn a_repeated_read_only_turn_trips_the_brake() {
    let mut state = no_progress_state(2, 3);
    let seen: Vec<_> = (0..3)
        .map(|_| {
            state.record_round_outcome(false, false);
            gate(&mut state, true)
        })
        .collect();
    assert_eq!(seen, ["continue", "steer", "stop"]);
}

#[test]
fn fresh_evidence_renews_each_window_but_duplicate_cycles_do_not() {
    let mut state = no_progress_state(0, 3);
    let mut limit = 1;
    let args = serde_json::json!({"path":"source.rs"});
    for (round, output) in ["first span", "second span", "third span"]
        .into_iter()
        .enumerate()
    {
        let progress = state.record_observation("read_file", &args, output, true, None, None);
        assert!(progress);
        state.record_round_outcome(false, progress);
        assert!(state.admit_round(round + 1, &mut limit, 1));
        assert_eq!(gate(&mut state, false), "continue");
    }
    for output in ["first span", "second span", "third span"] {
        let progress = state.record_observation("read_file", &args, output, true, None, None);
        assert!(!progress, "cycling through prior evidence buys no renewal");
        state.record_round_outcome(false, progress);
    }
    assert!(!state.admit_round(limit, &mut limit, 1));
}

#[test]
fn repeated_passing_checks_ignore_volatile_output_and_need_changed_tree() {
    let mut state = WorkflowRuntimeState::default();
    let args = serde_json::json!({"phase":"test"});
    let observe = |state: &mut WorkflowRuntimeState, result| {
        state.record_observation(
            "lifecycle",
            &args,
            result,
            true,
            Some(crate::ExecOutcome::Passed),
            None,
        )
    };
    assert!(observe(&mut state, "passed in 1.23s"));
    assert!(!observe(&mut state, "passed in 2.34s"));
    let first = ProgressSnapshot::Tree(content_addressable::ContentId::from_canonical_bytes(&[1]));
    let second = ProgressSnapshot::Tree(content_addressable::ContentId::from_canonical_bytes(&[2]));
    assert!(!state.record_workspace_change(Some(first), Some(first)));
    assert!(!state.record_workspace_change(Some(first), None));
    assert!(!observe(&mut state, "passed in 3.45s"));
    assert!(state.record_workspace_change(Some(first), Some(second)));
    assert!(observe(&mut state, "passed in 4.56s"));
    assert!(
        !state.record_workspace_change(Some(second), Some(first)),
        "A/B mutation cycles are not new progress"
    );
}

#[test]
fn verification_progress_ignores_timeout_but_preserves_command_and_cwd() {
    let mut state = WorkflowRuntimeState::default();
    let args = serde_json::json!({"command":"cargo test -p newt-core"});
    let mut routed = build_exec_routed(&["cargo", "test", "-p", "newt-core"]);
    routed.1["cwd"] = serde_json::json!("workspace");
    routed.1["timeout_secs"] = serde_json::json!(300);
    let observe = |state: &mut WorkflowRuntimeState, routed: &(&'static str, serde_json::Value)| {
        state.record_observation(
            "run_command",
            &args,
            "tests passed",
            true,
            Some(crate::ExecOutcome::Passed),
            Some(routed),
        )
    };
    assert!(observe(&mut state, &routed));
    routed.1["timeout_secs"] = serde_json::json!(301);
    assert!(
        !observe(&mut state, &routed),
        "a larger timeout does not verify anything new"
    );
    routed.1.as_object_mut().unwrap().remove("timeout_secs");
    assert!(
        !observe(&mut state, &routed),
        "the unwrapped command is still the same check"
    );
    routed.1["argv"] = serde_json::json!(["cargo", "test", "-p", "newt-cli"]);
    assert!(
        observe(&mut state, &routed),
        "a different test target counts"
    );
    routed.1["cwd"] = serde_json::json!("another-workspace");
    assert!(
        observe(&mut state, &routed),
        "the working directory matters"
    );
    routed.1["argv"] = serde_json::json!(["just", "test"]);
    assert!(observe(&mut state, &routed), "the executed command matters");
}

#[test]
fn verification_progress_ignores_output_presentation_but_preserves_check() {
    let mut state = WorkflowRuntimeState::default();
    let args = serde_json::json!({"command": "cargo test -p newt-core | tail -30"});
    let mut routed = build_exec_routed(&["cargo", "test", "-p", "newt-core"]);
    routed.1["cwd"] = serde_json::json!("workspace");
    routed.1["trim"] = serde_json::json!({"mode": "tail", "n": 30});
    let observe = |state: &mut WorkflowRuntimeState, routed: &(&'static str, serde_json::Value)| {
        state.record_observation(
            "run_command",
            &args,
            "tests passed",
            true,
            Some(crate::ExecOutcome::Passed),
            Some(routed),
        )
    };
    assert!(observe(&mut state, &routed));
    for trim in [
        Some(serde_json::json!({"mode": "tail", "n": 40})),
        Some(serde_json::json!({"mode": "head", "n": 30})),
        None,
    ] {
        if let Some(trim) = trim {
            routed.1["trim"] = trim;
        } else {
            routed.1.as_object_mut().unwrap().remove("trim");
        }
        for echo in [Some(true), Some(false), None] {
            if let Some(echo) = echo {
                routed.1["echo_dropped"] = serde_json::json!(echo);
            } else {
                routed.1.as_object_mut().unwrap().remove("echo_dropped");
            }
            assert!(
                !observe(&mut state, &routed),
                "presentation changes cannot earn fresh verification credit: {}",
                routed.1
            );
        }
    }
    routed.1["argv"] = serde_json::json!(["cargo", "test", "-p", "newt-core", "--release"]);
    assert!(
        observe(&mut state, &routed),
        "meaningful check flags remain distinct"
    );
    routed.1["argv"] = serde_json::json!(["cargo", "test", "-p", "newt-cli"]);
    assert!(observe(&mut state, &routed), "test targets remain distinct");
    routed.1["cwd"] = serde_json::json!("another-workspace");
    assert!(
        observe(&mut state, &routed),
        "working directories remain distinct"
    );
    let before = ProgressSnapshot::Tree(content_addressable::ContentId::from_canonical_bytes(&[1]));
    let after = ProgressSnapshot::Tree(content_addressable::ContentId::from_canonical_bytes(&[2]));
    assert!(state.record_workspace_change(Some(before), Some(after)));
    assert!(
        observe(&mut state, &routed),
        "a changed tree can be reverified"
    );
}

#[tokio::test]
async fn workspace_progress_uses_real_bytes_without_reading_outside_authority() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().to_str().unwrap();
    let args = serde_json::json!({"command":"printf changed > output.txt"});
    let denied = crate::caveats::Scope::none();
    let allowed = crate::caveats::Scope::only([workspace.to_string()]);
    assert!(
        progress_workspace_state("run_command", &args, workspace, &denied)
            .await
            .is_none()
    );
    assert!(
        progress_workspace_state("read_file", &args, workspace, &allowed)
            .await
            .is_none()
    );
    let before = progress_workspace_state("run_command", &args, workspace, &allowed).await;
    std::fs::write(root.path().join("output.txt"), "changed").unwrap();
    let after = progress_workspace_state("run_command", &args, workspace, &allowed).await;
    let mut state = WorkflowRuntimeState::default();
    assert!(state.record_workspace_change(before, after));
    std::fs::write(root.path().join("output.txt"), "changed").unwrap();
    let unchanged = progress_workspace_state("run_command", &args, workspace, &allowed).await;
    assert!(
        !state.record_workspace_change(after, unchanged),
        "successful no-op write is not progress"
    );
    assert!(self_verify::workspace_tree_state(&root.path().join("missing")).is_none());
}

#[tokio::test]
async fn named_mutation_progress_uses_exact_file_grants_and_absent_postimage() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("source.txt");
    let workspace = root.path().to_str().unwrap();
    let args = serde_json::json!({"path":"source.txt"});
    let scope = crate::caveats::Scope::only([file.to_string_lossy().into_owned()]);
    std::fs::write(&file, "before").unwrap();
    let before = progress_workspace_state("edit_file", &args, workspace, &scope).await;
    assert!(matches!(before, Some(ProgressSnapshot::File(_))));
    std::fs::write(&file, "after").unwrap();
    let after = progress_workspace_state("edit_file", &args, workspace, &scope).await;
    let mut state = WorkflowRuntimeState::default();
    assert!(state.record_workspace_change(before, after));
    std::fs::remove_file(&file).unwrap();
    let absent = progress_workspace_state("delete_file", &args, workspace, &scope).await;
    assert!(matches!(absent, Some(ProgressSnapshot::File(_))));
    assert!(state.record_workspace_change(after, absent));
    let sibling = serde_json::json!({"path":"not-granted.txt"});
    assert!(
        progress_workspace_state("write_file", &sibling, workspace, &scope)
            .await
            .is_none()
    );
}

#[test]
fn a_second_write_resets_the_count_and_rearms_the_steer() {
    let mut state = no_progress_state(2, 4);
    state.record_round_outcome(true, true);
    assert_eq!(gate(&mut state, true), "continue");
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "continue");
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "steer");
    state.record_round_outcome(true, true); // real progress
    assert_eq!(gate(&mut state, true), "continue");
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "continue");
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "steer", "steers again");
}

/// The reset rule: a PASSING `lifecycle` test/check resets the count but stays
/// armed. (Which phases qualify is `only_test_and_check_lifecycle_phases_reset`.)
#[test]
fn a_passing_lifecycle_run_resets_the_count_but_stays_armed() {
    let mut state = no_progress_state(2, 4);
    state.record_round_outcome(true, true);
    assert_eq!(gate(&mut state, true), "continue");
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "continue");
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "steer");
    let args = serde_json::json!({"phase":"test"});
    let progress = state.record_observation(
        "lifecycle",
        &args,
        "passed",
        true,
        Some(crate::ExecOutcome::Passed),
        None,
    );
    assert!(progress);
    state.record_round_outcome(false, progress);
    assert_eq!(gate(&mut state, true), "continue");
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "continue");
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "steer", "still armed, steers again");
}

#[test]
fn zero_disables_each_half_and_default_is_8_12() {
    let mut off = no_progress_state(0, 0);
    off.record_round_outcome(true, true);
    for _ in 0..100 {
        off.record_round_outcome(false, false);
        assert_eq!(gate(&mut off, true), "continue");
    }
    let mut stop_only = no_progress_state(0, 2);
    stop_only.record_round_outcome(true, true);
    assert_eq!(gate(&mut stop_only, true), "continue");
    stop_only.record_round_outcome(false, false);
    assert_eq!(gate(&mut stop_only, true), "continue");
    stop_only.record_round_outcome(false, false);
    assert_eq!(gate(&mut stop_only, true), "stop");
    let d = crate::initiative::NoProgressRounds::default();
    assert_eq!((d.steer_after, d.stop_after), (8, 12));
}

/// Review fix 4: a loop made of rounds that `continue` never calls
/// `record_round_outcome`, but each is a completed model round with no change.
#[test]
fn rounds_that_never_reach_the_outcome_hook_still_count_toward_the_brake() {
    let mut state = no_progress_state(2, 3);
    state.record_round_outcome(true, true);
    let seen: Vec<_> = (0..5).map(|_| gate(&mut state, true)).collect();
    assert_eq!(
        seen,
        ["continue", "continue", "steer", "stop", "stop"],
        "{seen:?}"
    );
}

/// Review fix 3: the STEER is advice (`action_nudges` may be off); the STOP is a
/// bound and must fire regardless.
#[test]
fn the_stop_does_not_depend_on_nudges_being_allowed() {
    let mut state = no_progress_state(2, 3);
    state.record_round_outcome(true, true);
    let seen: Vec<_> = (0..4).map(|_| gate(&mut state, false)).collect();
    assert_eq!(
        seen,
        ["continue", "continue", "continue", "stop"],
        "no steer, but a stop: {seen:?}"
    );
}

/// Build a `routed_to` value shaped exactly as `tools.rs`'s dispatch site
/// records it: `("build_exec", {"argv": […]})`. The test helper for every
/// case below — since #2551 round 2, `is_progress_verification` reads this
/// recorded fact and never re-derives it, so a routed pass is simulated by
/// constructing what dispatch WOULD have recorded, not by driving a real
/// `classify_call` (that only needs proving once, in `routing.rs`'s own
/// tests, for the classification itself).
fn build_exec_routed(argv: &[&str]) -> (&'static str, serde_json::Value) {
    (
        "build_exec",
        serde_json::json!({ "argv": argv.iter().collect::<Vec<_>>() }),
    )
}

/// Review fix 2: only the gate phases are evidence of progress. `format`, `lint`,
/// `clean` and `setup` pass trivially (a looping model can call them forever); a
/// PIPED `run_command` never counts (`| tail` masks the exit status).
#[test]
fn only_test_and_check_lifecycle_phases_reset_the_brake() {
    use crate::ExecOutcome::{Failed, Passed};
    for (phase, expect) in [
        ("test", true),
        ("check", true),
        ("format", false),
        ("lint", false),
        ("clean", false),
        ("setup", false),
        ("bogus", false),
    ] {
        let args = serde_json::json!({ "phase": phase });
        assert_eq!(
            is_progress_verification("lifecycle", &args, Some(Passed), None),
            expect,
            "{phase}"
        );
    }
    let test = serde_json::json!({ "phase": "test" });
    assert!(!is_progress_verification(
        "lifecycle",
        &test,
        Some(Failed),
        None
    ));
    assert!(!is_progress_verification("lifecycle", &test, None, None));
    // #2524 follow-up (F23 "tail-pipe-routes"): a build piped ONLY to
    // `tail`/`head` now DOES route (`build_piped_to_trim_route`) — the
    // build's own exit code decides the outcome, the trim only touches the
    // rendered text, so this is no longer excluded. (Was excluded before
    // that fix landed, on the theory that ANY pipe masks the exit code;
    // measured in 2488-r9 this was exactly backwards for the tail/head-only
    // shape the model actually writes.) `routed_to` carries the trim too —
    // is_progress_verification only reads `argv`, so it is irrelevant here,
    // but the shape matches what dispatch actually records.
    let args = serde_json::json!({ "command": "cargo test -p newt-git | tail -20" });
    let routed = build_exec_routed(&["cargo", "test", "-p", "newt-git"]);
    assert!(is_progress_verification(
        "run_command",
        &args,
        Some(Passed),
        Some(&routed)
    ));
    // A pipe to anything ELSE never routes at dispatch (`classify` refuses
    // any other compound command) — `routed_to` is `None`, so this is
    // excluded structurally, not by re-classifying here.
    let grep_piped = serde_json::json!({ "command": "cargo test -p newt-git | grep FAILED" });
    assert!(!is_progress_verification(
        "run_command",
        &grep_piped,
        Some(Passed),
        None
    ));
}

/// r9 recon item 4 (red first): a `run_command` call that #2533/#2543's
/// routing sends to `build_exec` — a clean, unpiped `cargo test -p newt-git`
/// argv, spawned directly with no shell in between — now counts as a
/// verified pass exactly like a `lifecycle` gate phase does. Measured live in
/// 2483-r9: this exact command routed and passed three times through
/// `build_exec` and never reset the no-progress brake before this fix.
#[test]
fn a_routed_build_exec_pass_resets_the_brake_but_a_failure_does_not() {
    use crate::ExecOutcome::{Failed, Passed};
    let args = serde_json::json!({ "command": "cargo test -p newt-git" });
    let routed = build_exec_routed(&["cargo", "test", "-p", "newt-git"]);
    assert!(is_progress_verification(
        "run_command",
        &args,
        Some(Passed),
        Some(&routed)
    ));
    assert!(!is_progress_verification(
        "run_command",
        &args,
        Some(Failed),
        Some(&routed)
    ));
    assert!(!is_progress_verification(
        "run_command",
        &args,
        None,
        Some(&routed)
    ));
    // A call the routing table would never route (`routed_to` is `None` —
    // dispatch never routed it, `classify_call`'s own rules refused) never
    // counts even when the underlying argv looks build-shaped.
    let not_bare = serde_json::json!({ "command": "cargo test -p newt-git", "cwd": "sub" });
    assert!(!is_progress_verification(
        "run_command",
        &not_bare,
        Some(Passed),
        None
    ));
}

/// #2548 round 2 should-fix (red first): a routed `build_exec` pass counts
/// ONLY when the argv's subcommand/recipe is a gate — mirroring
/// `Phase::is_gate` exactly, the same "test"/"check" vocabulary the
/// `lifecycle` side already uses. `cargo build` (an incremental no-op when
/// already built), `cargo clippy`, `just fmt` and `just clean` all route to
/// `build_exec` too, but pass trivially or say nothing about behaviour —
/// a model alternating `edit_file` with a trivially-passing `just fmt` must
/// not reset the brake, exactly like the `lifecycle` table never counted
/// `lint`/`format`/`clean`/`setup`.
#[test]
fn only_a_routed_build_or_just_gate_recipe_resets_the_brake() {
    use crate::ExecOutcome::Passed;
    for (argv, expect) in [
        (["cargo", "test"].as_slice(), true),
        (["cargo", "check"].as_slice(), true),
        (["cargo", "build"].as_slice(), false),
        (["cargo", "clippy"].as_slice(), false),
        (["just", "test"].as_slice(), true),
        (["just", "check"].as_slice(), true),
        (["just", "fmt"].as_slice(), false),
        (["just", "clean"].as_slice(), false),
    ] {
        let args = serde_json::json!({ "command": argv.join(" ") });
        let routed = build_exec_routed(argv);
        assert_eq!(
            is_progress_verification("run_command", &args, Some(Passed), Some(&routed)),
            expect,
            "{argv:?}"
        );
    }
}

/// #2524 follow-up (red first): `cargo_build_route` keeps a leading
/// `+toolchain` selector in the routed argv (F23's own evidence was `cargo
/// +stable test …`), which pushes the gate subcommand from `argv[1]` to
/// `argv[2]`. Without skipping it here too, a genuine routed pass of
/// exactly that command would silently never reset the brake — the same
/// "measured, never counted" gap r9 recon found for the plain (no
/// toolchain) case.
#[test]
fn a_routed_toolchain_selected_gate_pass_still_resets_the_brake() {
    use crate::ExecOutcome::{Failed, Passed};
    let args = serde_json::json!({ "command": "cargo +stable test -p newt-core" });
    let routed = build_exec_routed(&["cargo", "+stable", "test", "-p", "newt-core"]);
    assert!(is_progress_verification(
        "run_command",
        &args,
        Some(Passed),
        Some(&routed)
    ));
    assert!(!is_progress_verification(
        "run_command",
        &args,
        Some(Failed),
        Some(&routed)
    ));
    // The same non-gate exclusion still applies past the toolchain token.
    let non_gate_routed = build_exec_routed(&["cargo", "+stable", "build", "-p", "newt-core"]);
    assert!(!is_progress_verification(
        "run_command",
        &args,
        Some(Passed),
        Some(&non_gate_routed)
    ));
    // An invalid `+` token never routes at all (routing.rs's own refusal),
    // so dispatch never records a `routed_to` for it either.
    let invalid = serde_json::json!({ "command": "cargo +$X test" });
    assert!(!is_progress_verification(
        "run_command",
        &invalid,
        Some(Passed),
        None
    ));
}

/// F24/PR1: a routed pass through a `cd <dir> &&` prefix counts as
/// verification exactly like the bare command would — #2548's gate check
/// reads the routed `argv`, which no longer carries the folded `cd`. PR1
/// widened the fold from #2550's workspace-root-only case to a real
/// subdirectory too (multi-repo recon rows 1/2/4/6, r10-r12 evidence: `cd
/// repoA && cargo test` never counted before this).
#[test]
fn a_routed_cd_prefixed_gate_pass_still_resets_the_brake() {
    use crate::ExecOutcome::Passed;
    let at_root = serde_json::json!({ "command": "cd /ws/root && cargo test -p newt-core" });
    let routed = build_exec_routed(&["cargo", "test", "-p", "newt-core"]);
    assert!(is_progress_verification(
        "run_command",
        &at_root,
        Some(Passed),
        Some(&routed)
    ));
    // PR1: a real SUBDIRECTORY folding is the SAME recorded shape — this
    // function cannot tell root from subdirectory apart (nor does it need
    // to: dispatch already decided that), reversing #2550's "stays
    // compound, never counts" for exactly this shape, which was the
    // r10-r12 evidence's most common one.
    let at_sub = serde_json::json!({ "command": "cd sub && cargo test -p newt-core" });
    assert!(is_progress_verification(
        "run_command",
        &at_sub,
        Some(Passed),
        Some(&routed)
    ));
}

/// F28 (#2483 evidence): a routed pass through a stripped leading `timeout`
/// wrapper counts as verification exactly like the bare command would —
/// `is_progress_verification` reads the recorded `routed_to` argv, which
/// never carries the dropped `timeout` (only `timeout_secs`, a
/// field this function never looks at), so the gate check is identical
/// whether or not the model wrapped its call in `timeout`.
#[test]
fn a_routed_timeout_wrapped_gate_pass_still_resets_the_brake() {
    use crate::ExecOutcome::Passed;
    let wrapped = serde_json::json!({ "command": "timeout 300 cargo test -p newt-core" });
    let routed = (
        "build_exec",
        serde_json::json!({
            "argv": ["cargo", "test", "-p", "newt-core"],
            "timeout_secs": 300,
        }),
    );
    assert!(is_progress_verification(
        "run_command",
        &wrapped,
        Some(Passed),
        Some(&routed)
    ));
}

/// #2551 round 2 should-fix (red first): `is_progress_verification` must
/// NEVER disagree with what dispatch actually decided, even when the
/// filesystem changed between dispatch and this call. Measured shape: `cd
/// target; cargo test | tail -5` on a clean checkout. At DISPATCH, `target/`
/// does not exist, so routing refuses (`Exec`) and the shell runs `cargo
/// test | tail -5` at the ROOT — which CREATES `target/` and masks the real
/// exit code behind `tail`. A re-classification AFTER the call would now
/// see `target/` and route successfully — exactly the masked-pipe pass
/// #2548 exists to exclude. Proven here by constructing that EXACT
/// filesystem state (a real tempdir with `target/` present) and passing
/// `routed_to: None` (what dispatch actually recorded, before `target/`
/// existed) — `is_progress_verification` takes no `workspace`/`read_scope`
/// at all any more, so it cannot re-derive a different answer no matter
/// what is on disk.
#[test]
fn a_dispatch_time_non_route_never_counts_even_if_the_filesystem_changed_since() {
    use crate::ExecOutcome::Passed;
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    std::fs::create_dir(root.join("target")).expect("mkdir target");
    // Sanity: if this were re-derived now, `target/` genuinely resolves and
    // WOULD route — proving the test exercises the real hazard, not a
    // vacuous one.
    let would_now_route = matches!(
        super::routing::RouteTable::builtin().classify(
            "cd target; cargo test",
            &root,
            &crate::caveats::Scope::only([root.to_string_lossy().into_owned()]),
        ),
        super::routing::RouteDecision::Route {
            tool: "build_exec",
            ..
        }
    );
    assert!(
        would_now_route,
        "fixture sanity: target/ must resolve now, or this test proves nothing"
    );

    let args = serde_json::json!({ "command": "cd target; cargo test | tail -5" });
    assert!(!is_progress_verification(
        "run_command",
        &args,
        Some(Passed),
        None
    ));
}

/// Review round 3: operator steering is new information; it restarts the count
/// and re-arms the steer-once latch, exactly like progress.
#[test]
fn operator_steering_resets_the_count_and_rearms_the_steer() {
    let mut state = no_progress_state(2, 3);
    state.record_round_outcome(true, true);
    assert_eq!(gate(&mut state, true), "continue");
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "continue");
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "steer");
    // The operator types a steer; delivered at the start of the round that
    // would otherwise be the 3rd idle one (the stop).
    state.record_round_outcome(false, false);
    state.note_operator_steering();
    assert_eq!(
        gate(&mut state, true),
        "continue",
        "no stop: the count restarted"
    );
    state.record_round_outcome(false, false);
    assert_eq!(gate(&mut state, true), "continue");
    state.record_round_outcome(false, false);
    assert_eq!(
        gate(&mut state, true),
        "steer",
        "the steer-once latch re-armed"
    );
}

/// F37 regression: the repeat guard refused an identical failed `cargo check`
/// (and `lifecycle`) AFTER an `edit_file` had changed the tree, so the model
/// could never re-verify its own repair. The release on a workspace change
/// must not depend on a verification mode.
#[test]
fn identical_failed_call_reruns_after_a_workspace_edit() {
    let calls = [
        ("run_command", serde_json::json!({"command": "cargo check"})),
        ("lifecycle", serde_json::json!({"phase": "check"})),
    ];
    for (name, args) in &calls {
        let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
        let mut guard = RepeatCallGuard::default();
        guard.record(
            name,
            args,
            false,
            "error: exited 101",
            None,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats,
            },
        );
        assert!(
            guard.repeat_steer(name, args).is_some(),
            "{name}: true repeat"
        );
        guard.record(
            "edit_file",
            &serde_json::json!({"path": "a.rs"}),
            true,
            "ok",
            None,
            ReadScope {
                workspace: &workspace,
                caveats: &caveats,
            },
        );
        assert!(
            guard.repeat_steer(name, args).is_none(),
            "{name} must run after an edit"
        );
    }
}

/// F37: without a change in between, an identical read-only probe stays refused.
#[test]
fn identical_read_only_probe_is_still_refused_without_a_change() {
    let (_dir, workspace, caveats) = read_memo_fixture("unused.txt", "");
    let args = serde_json::json!({"command": "grep x a.txt"});
    let mut guard = RepeatCallGuard::default();
    guard.record(
        "run_command",
        &args,
        true,
        "hello",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    guard.record(
        "read_file",
        &serde_json::json!({"path": "a"}),
        true,
        "x",
        None,
        ReadScope {
            workspace: &workspace,
            caveats: &caveats,
        },
    );
    assert!(guard.repeat_steer("run_command", &args).is_some());
}

/// #2637 / #2555: the compaction hook must release the read memo on COMMIT
/// so the next identical read gets fresh content, while a REJECTED attempt
/// leaves the memo in place (no wasted re-read).
/// Red: comment out `g.release_read_memos()` below → the `.is_none()` assertion fails.
#[test]
fn compaction_committed_forces_fresh_read_rejected_preserves_memo() {
    let (_dir, workspace, caveats) = read_memo_fixture("c.rs", "the source");
    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({"path": "c.rs"});
    let scope = ReadScope {
        workspace: &workspace,
        caveats: &caveats,
    };

    g.record("read_file", &args, true, "the source", None, scope);

    // Before any compaction: memo is live — cache hit.
    assert!(
        g.cached_read("read_file", &args, scope).is_some(),
        "memo must be live before any compaction"
    );

    // Rejected compaction: no release call — memo must survive.
    assert!(
        g.cached_read("read_file", &args, scope).is_some(),
        "a rejected compaction must leave the read memo intact"
    );

    // Committed compaction: release the memos.
    g.release_read_memos(); // comment out → the assertion below fails

    // After committed compaction: memo is gone → next read runs fresh.
    assert!(
        g.cached_read("read_file", &args, scope).is_none(),
        "committed compaction must clear the read memo so the next call reads fresh"
    );
}
