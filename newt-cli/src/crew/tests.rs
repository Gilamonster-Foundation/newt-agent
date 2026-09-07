use super::*;

#[test]
fn plan_author_prompt_steers_to_minimal_action_only_subtasks() {
    // Regression: the planner must author the FEWEST, code-changing subtasks and
    // NOT separate inspect/verify/test steps. Those steps land nothing and (after
    // the crew-honesty fix) are correctly marked Failed — leaving a plan whose fix
    // DID land falsely reported "incomplete". Steering the prompt is the root fix.
    assert!(PLAN_AUTHOR_SYSTEM.contains("FEWEST"));
    assert!(PLAN_AUTHOR_SYSTEM.contains("MUST change code"));
    assert!(PLAN_AUTHOR_SYSTEM.contains("Do NOT create separate"));
    assert!(PLAN_AUTHOR_SYSTEM.contains("verifies EVERY subtask"));
}

#[test]
fn plan_author_prompt_forbids_rephrased_verify_subtasks_and_gives_a_worked_example() {
    // Regression (autopsy 2026-07-02-pr802-baseline, T2-humanize-duration x
    // qwen3-coder:30b / qwen2.5-coder:32b x2 / deepseek-r1:70b, secondary tag
    // planner-over-decomposition): the literal-verb prohibition above was not
    // enough — planners kept the anti-pattern alive by REPHRASING the forbidden
    // verbs. tmp.zfbIvGkXKX emitted 6 dependent leaves for a one-line bug fix
    // (fix-seconds-component, handle-minutes-and-seconds,
    // format-minutes-and-seconds, test-edge-cases, clean-up-code,
    // update-comments) — "Add ... tests", "Refactor ... readability", "Update
    // comments" never say the literal word "test"/"verify" — then stalled with
    // 4 leaves unreached after leaf 2 broke scope (.plan.log:79-80, "plan
    // incomplete"). A worked one-shot example is a stronger anti-pattern signal
    // than a word blocklist alone.
    assert!(PLAN_AUTHOR_SYSTEM.contains("REPHRASINGS"));
    assert!(PLAN_AUTHOR_SYSTEM.contains("is ONE subtask, not six"));
    assert!(PLAN_AUTHOR_SYSTEM.contains("fix-duration-format"));
}

#[test]
fn github_refs_parses_issue_and_pr_urls_in_prose() {
    // The exact prompt shape from the #548 exercise: a URL in surrounding prose.
    let refs = github_refs(
        "https://github.com/Gilamonster-Foundation/newt-agent/issues/548 <- take a look",
    );
    assert_eq!(
        refs,
        vec![(
            "Gilamonster-Foundation".to_string(),
            "newt-agent".to_string(),
            "issues".to_string(),
            "548".to_string(),
        )]
    );
    // PR URLs (kind = pull) and trailing punctuation on the number.
    let refs = github_refs("see https://github.com/o/r/pull/12).");
    assert_eq!(refs[0].2, "pull");
    assert_eq!(refs[0].3, "12");
    // No GitHub URL ⇒ nothing (authoring just uses the goal text).
    assert!(github_refs("implement the thing").is_empty());
}

#[test]
fn one_shot_authority_grants_fs_keeps_exec_and_net_denied() {
    use newt_core::role_profile::{ScopeKeyword, ScopeSpec};
    let denied = ScopeSpec::Keyword(ScopeKeyword::None);
    let all = ScopeSpec::Keyword(ScopeKeyword::All);
    let mut plan = newt_core::plan::Plan::from_toml_str(
        "goal = \"g\"\n[[subtask]]\nid = \"a\"\ninstruction = \"do a thing\"\n",
    )
    .unwrap();
    // Authored default is fully denied on every axis.
    assert_eq!(plan.subtasks[0].caveat_policy.fs_write, denied);
    grant_one_shot_authority(&mut plan);
    let pol = &plan.subtasks[0].caveat_policy;
    assert_eq!(pol.fs_read, all, "fs_read granted");
    assert_eq!(pol.fs_write, all, "fs_write granted");
    // No shell or network authority is handed out.
    assert_eq!(pol.exec, denied, "exec stays denied");
    assert_eq!(pol.net, denied, "net stays denied");
}

#[test]
fn plan_preview_lists_leaves_and_their_deps() {
    // Pure (in-memory plan, no fs): the preview shows the goal, the subtask
    // count, and the LEAVES (dispatch units) — a branch is not listed.
    let plan = newt_core::plan::Plan::from_toml_str(
        "goal = \"ship it\"\n\
         [[subtask]]\nid=\"epic\"\ninstruction=\"big\"\n\
         [[subtask]]\nid=\"a\"\ninstruction=\"do a\"\nparent=\"epic\"\n\
         [[subtask]]\nid=\"b\"\ninstruction=\"do b\"\nparent=\"epic\"\ndeps=[\"a\"]\n",
    )
    .unwrap();
    let preview = render_plan_preview(&plan);
    assert!(preview.contains("goal: ship it"), "{preview}");
    assert!(preview.contains("3 subtask(s); 2 leaf"), "{preview}");
    assert!(preview.contains("• a — do a"), "{preview}");
    assert!(preview.contains("• b — do b  (after a)"), "{preview}");
    assert!(
        !preview.contains("• epic"),
        "a branch is not a dispatch unit"
    );
    assert!(!preview.contains("will stall"), "a-leaf dep is satisfiable");
}

#[test]
fn plan_preview_flags_deps_that_will_stall() {
    // A leaf depending on a BRANCH (epic, never dispatched) or an ABSENT id
    // can never reach Done → flag it so the preview reveals an unrunnable plan.
    let plan = newt_core::plan::Plan::from_toml_str(
        "[[subtask]]\nid=\"epic\"\ninstruction=\"branch\"\n\
         [[subtask]]\nid=\"a\"\ninstruction=\"do a\"\nparent=\"epic\"\ndeps=[\"epic\"]\n\
         [[subtask]]\nid=\"b\"\ninstruction=\"do b\"\nparent=\"epic\"\ndeps=[\"ghost\"]\n",
    )
    .unwrap();
    let preview = render_plan_preview(&plan);
    assert!(
        preview.contains("epic [will stall]"),
        "branch dep: {preview}"
    );
    assert!(
        preview.contains("ghost [will stall]"),
        "absent dep: {preview}"
    );
}

#[test]
fn worktree_path_guard_refuses_absolute_and_dotdot() {
    // The structural worktree boundary (Path::join discards the base for an
    // absolute path, so fs_write=All is not the boundary — this guard is).
    assert!(is_safe_worktree_path("src/lib.rs"));
    assert!(is_safe_worktree_path("a/b/c.txt"));
    assert!(!is_safe_worktree_path("/etc/passwd"), "absolute escapes");
    assert!(
        !is_safe_worktree_path("../../../etc/cron.d/x"),
        ".. escapes"
    );
    assert!(!is_safe_worktree_path("a/../../b"), "embedded .. escapes");
}

#[test]
fn worktree_read_is_confined_to_the_worktree() {
    // #521: a crew read is fenced to the worktree — an absolute / `..` path
    // can't escape to the host, while a legitimate relative read still works.
    let repo = git_repo();
    let ws = WorktreeWorkspace::create(repo.path(), &worktree_id(), "HEAD", "true".into()).unwrap();
    assert_eq!(ws.read("hello.txt").as_deref(), Some("world\n"));
    assert!(ws.read("/etc/hostname").is_none(), "absolute read refused");
    assert!(
        ws.read("../../../../etc/hostname").is_none(),
        ".. read refused"
    );
}

#[test]
fn parse_authored_plan_maps_json_to_a_default_deny_plan() {
    // Tolerates fences/prose; maps deps/verify; authority is NOT model-granted.
    let raw = "Sure! ```json\n{\"goal\":\"g\",\"subtasks\":[\
        {\"id\":\"a\",\"instruction\":\"do a\",\"verify\":\"just check\"},\
        {\"id\":\"b\",\"instruction\":\"do b\",\"deps\":[\"a\"]}]}\n``` (done)";
    let plan = parse_authored_plan(raw).expect("parsed a plan");
    assert_eq!(plan.goal.as_deref(), Some("g"));
    assert_eq!(plan.subtasks.len(), 2);
    assert_eq!(plan.subtasks[1].deps, vec!["a"]);
    assert_eq!(plan.subtasks[0].verify.as_deref(), Some("just check"));
    assert!(plan.subtasks[0].parent.is_none());
    // default-DENY: the model proposes work, never authority.
    assert_eq!(
        plan.subtasks[0].caveat_policy,
        newt_core::plan::CaveatPolicy::default()
    );
}

#[test]
fn parse_authored_plan_rejects_empty_or_unparseable() {
    assert!(parse_authored_plan("no json at all").is_none());
    assert!(
        parse_authored_plan("{\"goal\":\"g\",\"subtasks\":[]}").is_none(),
        "empty subtasks → not a usable plan"
    );
    assert!(parse_authored_plan("{not json}").is_none());
    // DUPLICATE ids → rejected (they would desync the execute-time cursor into
    // an unbounded re-dispatch loop).
    assert!(
        parse_authored_plan(
            "{\"subtasks\":[{\"id\":\"a\",\"instruction\":\"x\"},{\"id\":\"a\",\"instruction\":\"y\"}]}"
        )
        .is_none(),
        "duplicate ids"
    );
    // Empty / whitespace id or instruction → rejected.
    assert!(
        parse_authored_plan("{\"subtasks\":[{\"id\":\"  \",\"instruction\":\"x\"}]}").is_none(),
        "blank id"
    );
    assert!(
        parse_authored_plan("{\"subtasks\":[{\"id\":\"a\",\"instruction\":\"\"}]}").is_none(),
        "empty instruction"
    );
}

#[tokio::test]
async fn author_plan_decomposes_a_goal_via_the_model() {
    struct PlanMock;
    #[async_trait::async_trait]
    impl Dispatcher for PlanMock {
        async fn dispatch(
            &self,
            _b: &newt_scheduler::PoolBackend,
            _m: &str,
            _r: newt_scheduler::ChatRequest,
        ) -> anyhow::Result<newt_scheduler::ChatReply> {
            Ok(newt_scheduler::ChatReply {
                content: "```json\n{\"goal\":\"ship\",\"subtasks\":[\
                    {\"id\":\"a\",\"instruction\":\"do a\",\"verify\":\"cargo test\"},\
                    {\"id\":\"b\",\"instruction\":\"do b\",\"deps\":[\"a\"]}]}\n```"
                    .to_string(),
                model_id: "m".into(),
                usage: None,
            })
        }
    }
    let cfg: Config = toml::from_str(
        "[[backends]]\nname=\"x\"\nendpoint=\"http://x:11434\"\nmodel=\"m\"\ntiers=[]\n",
    )
    .unwrap();
    let pool = BackendPool::from_source(&StaticSource::from_configs(cfg.backends.iter()));
    let plan = author_plan(&pool, &PlanMock, "m", "ship the thing", 8, None)
        .await
        .expect("authored a plan");
    assert_eq!(plan.goal.as_deref(), Some("ship"));
    assert_eq!(plan.subtasks.len(), 2);
    assert_eq!(plan.subtasks[1].deps, vec!["a"]);
    assert_eq!(
        plan.subtasks[0].caveat_policy,
        newt_core::plan::CaveatPolicy::default()
    );
}

#[test]
fn effective_markers_composes_remove_then_add() {
    // #819: the [plan.prune] override composes with the compiled lexicon —
    // removals first (and removals beat additions), then case-normalized
    // deduped additions.
    let cfg = newt_core::PlanPruneConfig {
        disabled: false,
        add_inspect: vec!["Scrutinize".into(), "  ".into(), "verify".into()],
        add_gate: vec!["smoke".into()],
        remove: vec!["REVIEW".into(), "verify".into()],
    };
    let m = effective_markers(Some(&cfg));
    assert!(
        marker_kind_in(&m, "Review the code for style").is_none(),
        "removed builtin no longer marks"
    );
    assert!(
        marker_kind_in(&m, "verify the output").is_none(),
        "a removed verb cannot be re-added"
    );
    assert!(matches!(
        marker_kind_in(&m, "Scrutinize the module"),
        Some(MarkerKind::Inspect)
    ));
    assert!(matches!(
        marker_kind_in(&m, "smoke the build"),
        Some(MarkerKind::Gate)
    ));
    // Untouched builtins survive.
    assert!(matches!(
        marker_kind_in(&m, "inspect the parser"),
        Some(MarkerKind::Inspect)
    ));
    // None = exactly the compiled defaults.
    let d = effective_markers(None);
    assert_eq!(d.len(), ACTION_MARKERS.len());
}

#[test]
fn prune_respects_the_config_lexicon() {
    // #819: a custom Inspect verb prunes under the composed lexicon and
    // survives under the defaults — the override is config, not code.
    let cfg = newt_core::PlanPruneConfig {
        add_inspect: vec!["scrutinize".into()],
        ..Default::default()
    };
    let mut plan = marker_plan(vec![
        marker_sub("pad", "Scrutinize the module layout", &[]),
        marker_sub("work", "Add the parser", &["pad"]),
    ]);
    prune_non_actionable_subtasks_in(&mut plan, &effective_markers(Some(&cfg)));
    let ids: Vec<&str> = plan.subtasks.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["work"], "custom inspect verb pruned, dep rewired");
    assert!(plan.subtasks[0].deps.is_empty());

    let mut plan2 = marker_plan(vec![
        marker_sub("pad", "Scrutinize the module layout", &[]),
        marker_sub("work", "Add the parser", &["pad"]),
    ]);
    prune_non_actionable_subtasks_in(&mut plan2, &effective_markers(None));
    assert_eq!(plan2.subtasks.len(), 2, "defaults do not know the verb");
}

#[tokio::test]
async fn author_plan_with_disabled_prune_keeps_padding_leaves() {
    // #819: [plan.prune] disabled = true restores the pre-#803 behavior —
    // the padded inspect leaf survives authoring; with the default config
    // it is pruned. Same mock, only the config differs.
    struct PaddedPlanMock;
    #[async_trait::async_trait]
    impl Dispatcher for PaddedPlanMock {
        async fn dispatch(
            &self,
            _b: &newt_scheduler::PoolBackend,
            _m: &str,
            _r: newt_scheduler::ChatRequest,
        ) -> anyhow::Result<newt_scheduler::ChatReply> {
            Ok(newt_scheduler::ChatReply {
                content: "{\"goal\":\"g\",\"subtasks\":[\
                    {\"id\":\"look\",\"instruction\":\"Inspect the module\"},\
                    {\"id\":\"work\",\"instruction\":\"Add the fix\",\"deps\":[\"look\"]}]}"
                    .to_string(),
                model_id: "m".into(),
                usage: None,
            })
        }
    }
    let cfg: Config = toml::from_str(
        "[[backends]]\nname=\"x\"\nendpoint=\"http://x:11434\"\nmodel=\"m\"\ntiers=[]\n",
    )
    .unwrap();
    let pool = BackendPool::from_source(&StaticSource::from_configs(cfg.backends.iter()));
    let disabled = newt_core::PlanPruneConfig {
        disabled: true,
        ..Default::default()
    };
    let kept = author_plan(&pool, &PaddedPlanMock, "m", "g", 8, Some(&disabled))
        .await
        .expect("authored");
    assert_eq!(kept.subtasks.len(), 2, "disabled ⇒ padding survives");
    let pruned = author_plan(&pool, &PaddedPlanMock, "m", "g", 8, None)
        .await
        .expect("authored");
    assert_eq!(pruned.subtasks.len(), 1, "default ⇒ padding pruned");
    assert_eq!(pruned.subtasks[0].id, "work");
    assert!(pruned.subtasks[0].deps.is_empty(), "dep rewired");
}

// ── #801: prune non-actionable (inspect / terminal-gate) subtasks ──────────
fn marker_sub(id: &str, instruction: &str, deps: &[&str]) -> newt_core::plan::Subtask {
    newt_core::plan::Subtask {
        id: id.into(),
        instruction: instruction.into(),
        deps: deps.iter().map(|s| (*s).to_string()).collect(),
        parallel_ok: false,
        context: vec![],
        verify: None,
        status: newt_core::plan::SubtaskStatus::Pending,
        result: None,
        parent: None,
        kind: newt_core::plan::NodeKind::Task,
        conversation_id: None,
        artifact_ref: None,
        caveat_policy: newt_core::plan::CaveatPolicy::default(),
    }
}
fn marker_plan(subs: Vec<newt_core::plan::Subtask>) -> newt_core::plan::Plan {
    newt_core::plan::Plan {
        goal: None,
        aggregation: newt_core::plan::Aggregation::default(),
        subtasks: subs,
    }
}

#[test]
fn prune_drops_inspect_and_terminal_gate_rewiring_deps() {
    // The over-decomposed plan the DGX sweep caught (30b × T2): a read-only
    // inspect leaf, the real fix, and a trailing validate gate. WOULD FAIL before
    // #801 — all three subtasks survived and the inspect/gate tripped
    // nothing-to-land.
    let mut plan = marker_plan(vec![
        marker_sub(
            "inspect-test",
            "Inspect the existing test to understand the failure",
            &[],
        ),
        marker_sub(
            "fix",
            "Modify humanize_duration to return \"1m 30s\"",
            &["inspect-test"],
        ),
        marker_sub(
            "validate",
            "Verify the tests pass with cargo test",
            &["fix"],
        ),
    ]);
    prune_non_actionable_subtasks_in(&mut plan, &effective_markers(None));
    let ids: Vec<&str> = plan.subtasks.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["fix"],
        "inspect + terminal gate pruned, fix survives"
    );
    // the fix's dep on the removed inspect (which had no deps) is rewired away,
    // so it is immediately dispatchable instead of stalled on an absent dep.
    assert!(plan.subtasks[0].deps.is_empty(), "dangling dep removed");
}

#[test]
fn prune_keeps_a_gate_a_real_leaf_depends_on() {
    // A NON-terminal gate (a survivor still depends on it) is kept — the prune
    // bites only terminal gates.
    let mut plan = marker_plan(vec![
        marker_sub("extract", "Extract the sum into a helper", &[]),
        marker_sub("check", "Validate the helper compiles", &["extract"]),
        marker_sub("use", "Rewrite summarize to call the helper", &["check"]),
    ]);
    prune_non_actionable_subtasks_in(&mut plan, &effective_markers(None));
    let ids: Vec<&str> = plan.subtasks.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["extract", "check", "use"],
        "mid-plan gate retained"
    );
}

#[test]
fn prune_leaves_an_all_marker_plan_untouched() {
    // Every subtask is a marker → zero actionable work → the empty-guard leaves
    // the plan intact for plan_sanity / re-author, never half-pruned.
    let mut plan = marker_plan(vec![
        marker_sub("a", "Inspect the module", &[]),
        marker_sub("b", "Verify the build", &["a"]),
    ]);
    prune_non_actionable_subtasks_in(&mut plan, &effective_markers(None));
    assert_eq!(plan.subtasks.len(), 2, "all-marker plan untouched");
}

#[test]
fn prune_is_a_noop_on_a_plan_of_real_work() {
    // Non-marker leading verbs ("Add", "Rename") survive unchanged — the existing
    // behaviour for a well-formed multi-leaf plan, deps intact.
    let mut plan = marker_plan(vec![
        marker_sub("a", "Add error handling to parse_port", &[]),
        marker_sub("b", "Rename the helper", &["a"]),
    ]);
    prune_non_actionable_subtasks_in(&mut plan, &effective_markers(None));
    let ids: Vec<&str> = plan.subtasks.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b"]);
    assert_eq!(plan.subtasks[1].deps, vec!["a"]);
}

#[test]
fn ground_subtask_scopes_unions_def_sites_with_declared_files() {
    // #812 (§4b: augmentation, not replacement): the derived def-site
    // LEADS the scope, and the model's declared files are APPENDED — a
    // companion target grounding cannot see (a new test file) must stay
    // in the lane, and a mis-declaring model still cannot aim the fence
    // away from the real seam. Where derivation finds nothing, the
    // declaration alone survives.
    let mut plan = marker_plan(vec![
        marker_sub("fix", "Fix `humanize_duration` in the util module", &[]),
        marker_sub("new", "Create the brand-new reporting module", &[]),
    ]);
    plan.subtasks[0].context = vec!["tests/util_test.rs".to_string(), "src/util.rs".to_string()];
    plan.subtasks[1].context = vec!["src/report.rs".to_string()];
    let def_sites = |sym: &str| -> Vec<String> {
        if sym == "humanize_duration" {
            vec!["src/util.rs:2:pub fn humanize_duration".to_string()]
        } else {
            Vec::new()
        }
    };
    ground_subtask_scopes(&mut plan, &def_sites);
    assert_eq!(
        plan.subtasks[0].context,
        vec!["src/util.rs".to_string(), "tests/util_test.rs".to_string()],
        "derived seam leads; declared companion appended; dup deduped"
    );
    assert_eq!(
        plan.subtasks[1].context,
        vec!["src/report.rs".to_string()],
        "no def-site found → the declared file survives alone"
    );
}

#[test]
fn parse_authored_plan_reads_declared_files_into_context() {
    // #812: the authored JSON's `files` array lands in Subtask.context
    // (as the untrusted fallback the def-site derivation may override);
    // absent/empty `files` still parses with an empty context.
    let raw = r#"{"goal":"g","subtasks":[
        {"id":"a","instruction":"Fix util","files":["src/util.rs","  "],"deps":[]},
        {"id":"b","instruction":"Add docs","deps":["a"]}
    ]}"#;
    let plan = parse_authored_plan(raw).expect("parses");
    assert_eq!(plan.subtasks[0].context, vec!["src/util.rs".to_string()]);
    assert!(plan.subtasks[1].context.is_empty());
}

#[test]
fn crew_shared_target_is_a_single_absolute_per_root_dir() {
    // #697: every leaf derives the SAME absolute target under the crew root, so
    // sequential leaves share it (incremental builds) and a leaf's own cwd
    // can't relativize it. The root must be platform-absolute — a bare
    // `/tmp/...` is NOT absolute on Windows (paths need a drive prefix), which
    // failed the Windows CI job.
    #[cfg(windows)]
    let root = Path::new(r"C:\tmp\throw");
    #[cfg(not(windows))]
    let root = Path::new("/tmp/throw");
    let a = crew_shared_target_dir(root);
    assert!(a.ends_with(".scratch/crew-target"), "{a:?}");
    assert!(a.is_absolute(), "must be absolute: {a:?}");
    assert_eq!(crew_shared_target_dir(root), a);
}

#[test]
fn plan_sanity_flags_dangling_deps_and_passes_clean_plans() {
    // Clean: b depends on a, both defined.
    let ok = newt_core::plan::Plan::from_toml_str(
        "goal = \"g\"\n[[subtask]]\nid = \"a\"\ninstruction = \"do a\"\n\
         [[subtask]]\nid = \"b\"\ninstruction = \"do b\"\ndeps = [\"a\"]\n",
    )
    .unwrap();
    assert!(plan_sanity(&ok).is_empty(), "{:?}", plan_sanity(&ok));
    // Dangling: b depends on a `ghost` no subtask defines.
    let bad = newt_core::plan::Plan::from_toml_str(
        "goal = \"g\"\n[[subtask]]\nid = \"b\"\ninstruction = \"do b\"\ndeps = [\"ghost\"]\n",
    )
    .unwrap();
    let probs = plan_sanity(&bad);
    assert!(probs.iter().any(|p| p.contains("ghost")), "{probs:?}");
}

// -- #691: claim-check backstop (refute a subtask targeting a symbol's wrong file) --

#[test]
fn claim_check_refutes_a_subtask_targeting_the_wrong_file() {
    // The #548 shape: plan says edit `help_lines` in newt-cli/src/crew.rs, but
    // it's defined in newt-tui/src/lib.rs.
    let plan = newt_core::plan::Plan::from_toml_str(
        "goal = \"g\"\n[[subtask]]\nid = \"a\"\n\
         instruction = \"In newt-cli/src/crew.rs, modify the `help_lines` function\"\n",
    )
    .unwrap();
    let def_sites = |sym: &str| -> Vec<String> {
        if sym == "help_lines" {
            vec!["newt-tui/src/lib.rs:8273:fn help_lines() {".to_string()]
        } else {
            vec![]
        }
    };
    let c = plan_grounding_contradictions(&plan, def_sites);
    assert!(
        c.iter()
            .any(|p| p.contains("help_lines") && p.contains("newt-tui/src/lib.rs")),
        "must cite the real def site: {c:?}"
    );
}

#[test]
fn claim_check_refutes_an_unquoted_symbol_in_the_wrong_file() {
    // #696: the warm #548 retest's subtask named the symbol UNQUOTED ("the
    // help_lines function"), so C's old backtick-only recall missed it.
    let plan = newt_core::plan::Plan::from_toml_str(
        "goal = \"g\"\n[[subtask]]\nid = \"a\"\n\
         instruction = \"Refactor the help_lines function in newt-cli/src/crew.rs\"\n",
    )
    .unwrap();
    let def_sites = |sym: &str| -> Vec<String> {
        if sym == "help_lines" {
            vec!["newt-tui/src/lib.rs:8273:fn help_lines() {".to_string()]
        } else {
            vec![]
        }
    };
    let c = plan_grounding_contradictions(&plan, def_sites);
    assert!(
        c.iter()
            .any(|p| p.contains("help_lines") && p.contains("newt-tui")),
        "unquoted symbol must now be refuted: {c:?}"
    );
}

#[test]
fn claim_check_passes_when_the_claimed_file_matches_the_def() {
    let plan = newt_core::plan::Plan::from_toml_str(
        "goal = \"g\"\n[[subtask]]\nid = \"a\"\n\
         instruction = \"In newt-tui/src/lib.rs, modify `help_lines`\"\n",
    )
    .unwrap();
    let def_sites = |_: &str| vec!["newt-tui/src/lib.rs:8273:fn help_lines() {".to_string()];
    assert!(plan_grounding_contradictions(&plan, def_sites).is_empty());
}

#[test]
fn claim_check_never_refutes_a_new_symbol_or_pathless_step() {
    // defined nowhere (to be created) → no refutation
    let p1 = newt_core::plan::Plan::from_toml_str(
        "goal = \"g\"\n[[subtask]]\nid = \"a\"\ninstruction = \"create `new_thing` in src/new.rs\"\n",
    )
    .unwrap();
    assert!(plan_grounding_contradictions(&p1, |_| vec![]).is_empty());
    // no path claimed → nothing to check
    let p2 = newt_core::plan::Plan::from_toml_str(
        "goal = \"g\"\n[[subtask]]\nid = \"a\"\ninstruction = \"refactor `help_lines`\"\n",
    )
    .unwrap();
    let def = |_: &str| vec!["newt-tui/src/lib.rs:8273:fn help_lines() {".to_string()];
    assert!(plan_grounding_contradictions(&p2, def).is_empty());
}

#[test]
fn path_tokens_extracts_file_paths_only() {
    assert_eq!(
        path_tokens("edit newt-cli/src/crew.rs and call `help_lines`, not foo"),
        vec!["newt-cli/src/crew.rs".to_string()]
    );
    assert!(path_tokens("just a sentence with no paths").is_empty());
}

#[test]
fn repo_context_detects_language_layout_and_skips_non_repo() {
    let repo = git_repo();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    std::fs::write(repo.path().join("main.rs"), "fn main() {}\n").unwrap();
    git(repo.path(), &["add", "-A"]).unwrap();
    git(repo.path(), &["commit", "-qm", "add cargo"]).unwrap();
    let ctx = fetch_repo_context(repo.path());
    assert!(ctx.contains("Rust"), "detects Rust: {ctx}");
    assert!(
        ctx.contains("cargo test"),
        "infers the build command: {ctx}"
    );
    assert!(ctx.contains("Cargo.toml"), "lists top-level entries: {ctx}");
    // A non-repo dir contributes nothing (authoring uses the goal text alone).
    let empty = tempfile::tempdir().unwrap();
    assert!(fetch_repo_context(empty.path()).is_empty());
}

#[test]
fn git_failure_retains_the_process_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let error = git(dir.path(), &["newt-invalid-test-operation"])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("status="),
        "stderr alone cannot explain an unsuccessful Git subprocess: {error}"
    );
}

fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@t"],
        vec!["config", "user.name", "t"],
        // Keep line endings verbatim so checked-out content matches on
        // Windows runners (default autocrlf would turn "\n" into "\r\n").
        vec!["config", "core.autocrlf", "false"],
    ] {
        git(p, &args).unwrap();
    }
    std::fs::write(p.join("hello.txt"), "world\n").unwrap();
    git(p, &["add", "-A"]).unwrap();
    git(p, &["commit", "-qm", "init"]).unwrap();
    dir
}

#[test]
fn infer_test_command_priority() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(infer_test_command(dir.path()), None, "no markers → None");
    std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
    assert_eq!(infer_test_command(dir.path()).as_deref(), Some("pytest -x"));
    std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
    assert_eq!(
        infer_test_command(dir.path()).as_deref(),
        Some("cargo test")
    );
    std::fs::write(dir.path().join("justfile"), "").unwrap();
    assert_eq!(
        infer_test_command(dir.path()).as_deref(),
        Some("just check")
    );
}

#[test]
fn crew_normalize_commands_come_from_tooling_packs() {
    // #880: the toolchain→formatter mapping is DATA (newt_core::tooling), so
    // a Cargo repo resolves `cargo fmt` and an unmarked dir resolves nothing.
    // (The pack detection/merge/multiple-toolchain logic is tested in
    // newt_core::tooling; here we confirm the crew wires through to it.)
    let dir = tempfile::tempdir().unwrap();
    assert!(crew_normalize_commands(dir.path()).is_empty());
    std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
    assert!(crew_normalize_commands(dir.path()).contains(&"cargo fmt".to_string()));
}

#[test]
fn commit_to_branch_lands_work_visible_to_base() {
    let repo = git_repo();
    let mut ws = WorktreeWorkspace::create(repo.path(), "land1", "HEAD", "true".into()).unwrap();
    ws.apply(&[Edit {
        path: "added.rs".into(),
        new_content: "pub fn f() {}\n".into(),
    }]);
    let (branch, sha) = ws
        .commit_to_branch("crew/land1", "newt", "newt@bot", "land it")
        .unwrap();
    assert_eq!(branch, "crew/land1");
    assert!(!sha.is_empty());
    // The branch lives in the SHARED object store → the base repo sees it and
    // it carries the work, even after the worktree is dropped.
    drop(ws);
    let files = git(repo.path(), &["ls-tree", "-r", "--name-only", "crew/land1"]).unwrap();
    assert!(
        files.lines().any(|l| l == "added.rs"),
        "branch carries the work: {files}"
    );
    // Base working tree is untouched until a human merges the branch.
    assert!(
        !repo.path().join("added.rs").exists(),
        "base tree untouched until merge"
    );
}

#[test]
fn leaf_chains_off_prior_landed_tip() {
    // Leaf composition (#646): leaf B forks off leaf A's LANDED tip, so it
    // SEES A's work and its branch consolidates both — the property that
    // makes a multi-leaf one-shot run produce one coherent change instead of
    // N scattered single-step branches. (Real-fs tier; migrate per #514.)
    let repo = git_repo();

    // Leaf A: edit a.txt off HEAD, land crew/a.
    let mut wsa = WorktreeWorkspace::create(repo.path(), "a", "HEAD", "true".into()).unwrap();
    wsa.apply(&[Edit {
        path: "a.txt".into(),
        new_content: "A\n".into(),
    }]);
    let (_a, sha_a) = wsa.commit_to_branch("crew/a", "n", "n@b", "a").unwrap();
    drop(wsa);

    // Leaf B forks off A's landed sha — it MUST see a.txt (the chain).
    let mut wsb = WorktreeWorkspace::create(repo.path(), "b", &sha_a, "true".into()).unwrap();
    assert!(
        wsb.read("a.txt").is_some(),
        "leaf B forked off A's tip sees A's file"
    );
    wsb.apply(&[Edit {
        path: "b.txt".into(),
        new_content: "B\n".into(),
    }]);
    wsb.commit_to_branch("crew/b", "n", "n@b", "b").unwrap();
    drop(wsb);

    // crew/b is the single CONSOLIDATED tip — it carries BOTH leaves' work.
    let files = git(repo.path(), &["ls-tree", "-r", "--name-only", "crew/b"]).unwrap();
    assert!(
        files.lines().any(|l| l == "a.txt"),
        "consolidated tip has a.txt: {files}"
    );
    assert!(
        files.lines().any(|l| l == "b.txt"),
        "consolidated tip has b.txt: {files}"
    );
}

#[test]
fn same_head_siblings_consolidate_into_one_tip_containing_both() {
    let repo = git_repo();
    let mut a = WorktreeWorkspace::create(repo.path(), "sibling-a", "HEAD", "true".into()).unwrap();
    a.apply(&[Edit {
        path: "a.txt".into(),
        new_content: "A\n".into(),
    }]);
    let (branch_a, _) = a
        .commit_to_branch("crew/sibling-a", "n", "n@b", "a")
        .unwrap();
    drop(a);

    let mut b = WorktreeWorkspace::create(repo.path(), "sibling-b", "HEAD", "true".into()).unwrap();
    b.apply(&[Edit {
        path: "b.txt".into(),
        new_content: "B\n".into(),
    }]);
    let (branch_b, _) = b
        .commit_to_branch("crew/sibling-b", "n", "n@b", "b")
        .unwrap();
    drop(b);

    let (consolidated, _) = WorktreeWorkspace::consolidate_branches(
        repo.path(),
        "sibling-result",
        "HEAD",
        &[branch_a, branch_b],
        "n",
        "n@b",
    )
    .unwrap();
    let files = git(
        repo.path(),
        &["ls-tree", "-r", "--name-only", &consolidated],
    )
    .unwrap();
    assert!(files.lines().any(|line| line == "a.txt"), "{files}");
    assert!(files.lines().any(|line| line == "b.txt"), "{files}");
}

#[test]
fn commit_to_branch_errs_with_no_changes() {
    let repo = git_repo();
    let ws = WorktreeWorkspace::create(repo.path(), "land2", "HEAD", "true".into()).unwrap();
    assert!(
        ws.commit_to_branch("crew/land2", "n", "n@b", "noop")
            .is_err(),
        "no changes → nothing to land"
    );
}

#[test]
fn worktree_isolates_reads_and_writes() {
    let repo = git_repo();
    let mut ws = WorktreeWorkspace::create(repo.path(), "t1", "HEAD", "true".into()).unwrap();

    // files() lists tracked files; read() reads them (line-ending-tolerant).
    assert!(ws.files().iter().any(|f| f == "hello.txt"));
    assert_eq!(
        ws.read("hello.txt").as_deref().map(str::trim_end),
        Some("world")
    );
    assert_eq!(ws.read("nope.txt"), None);

    // apply() writes into the WORKTREE, not the live tree.
    let written = ws.apply(&[Edit {
        path: "src/new.rs".into(),
        new_content: "fn main() {}\n".into(),
    }]);
    assert_eq!(written, vec!["src/new.rs".to_string()]);
    assert_eq!(ws.read("src/new.rs").as_deref(), Some("fn main() {}\n"));
    assert!(
        !repo.path().join("src/new.rs").exists(),
        "edit must NOT touch the live tree"
    );
}

// run_test shells the platform shell; the assertions below use POSIX
// commands, so they're Unix-only (the deploy targets are macOS + Linux).
#[cfg(unix)]
#[test]
fn run_test_passes_and_reports_failure() {
    // run_test now runs CONFINED (P4). Where the kernel fence is available the
    // verify runs under it; where it is not, it fails closed (never runs the
    // repo-controlled command unconfined).
    // Confined-and-runs requires BOTH the kernel fs fence AND a resolvable
    // DenyAll net guard; where the guard is absent (a `-p newt-agent` build
    // that does not compile `newt-net-guard`, run from a non-`newt` harness)
    // the confined verify fails closed — secure, but not the "pass" outcome.
    let confinable = newt_core::confined_exec::kernel_fs_fence_available()
        && newt_core::confined_exec::net_guard_available();
    let repo = git_repo();
    let ok =
        WorktreeWorkspace::create(repo.path(), "t2a", "HEAD", "test -f hello.txt".into()).unwrap();
    let bad = WorktreeWorkspace::create(repo.path(), "t2b", "HEAD", "exit 3".into()).unwrap();
    if confinable {
        assert!(ok.run_test().0, "committed file present → pass (confined)");
        assert!(!bad.run_test().0, "non-zero exit → fail");
    } else {
        // Off the normative Linux+Landlock platform the confined outcome is
        // backend-dependent (macOS Seatbelt may confine-and-run, or the spawn
        // fails closed) — both secure. Assert the invariant that holds either
        // way: a non-zero-exit verify is NEVER reported as passing.
        assert!(
            !bad.run_test().0,
            "a failing verify must never be reported as passing"
        );
    }
}

#[cfg(unix)]
#[test]
fn set_test_command_changes_the_command_that_runs() {
    // Production regression: Workspace's default setter used to discard
    // the per-leaf verify command, so the runner silently repeated its
    // inferred command instead. Observe the command's exit outcome rather
    // than the setter call itself.
    if !(newt_core::confined_exec::kernel_fs_fence_available()
        && newt_core::confined_exec::net_guard_available())
    {
        return;
    }
    let repo = git_repo();
    let mut ws =
        WorktreeWorkspace::create(repo.path(), "verify-setter", "HEAD", "true".into()).unwrap();
    assert!(ws.run_test().0, "the original command passes");

    ws.set_test_command("false");

    assert!(
        !ws.run_test().0,
        "the installed per-leaf command must run and fail"
    );
}

#[test]
fn cleanup_removes_the_worktree() {
    let repo = git_repo();
    let path = {
        let ws = WorktreeWorkspace::create(repo.path(), "t3", "HEAD", "true".into()).unwrap();
        let p = ws.path().to_path_buf();
        assert!(p.exists());
        p
        // ws dropped here → cleanup()
    };
    assert!(!path.exists(), "Drop removed the worktree");
}

// --- B2: the `newt crew` wiring ---------------------------------------

/// An in-memory config with three role backends/loadouts and one crew.
fn crew_cfg() -> Config {
    toml::from_str(
        r#"
        [[backends]]
        name = "p"
        endpoint = "http://p:11434"
        model = "planner-m"
        tiers = []
        [[backends]]
        name = "n"
        endpoint = "http://n:11434"
        model = "nav-m"
        tiers = []
        [[backends]]
        name = "t"
        endpoint = "http://t:11434"
        model = "triage-m"
        tiers = []
        [loadouts.planner]
        provider = "p"
        [loadouts.navigator]
        provider = "n"
        [loadouts.triage]
        provider = "t"
        [crews.coder]
        planner = "planner"
        navigator = "navigator"
        triage = "triage"
        "#,
    )
    .unwrap()
}

#[test]
fn resolve_crew_name_explicit_single_none_multiple() {
    let cfg = crew_cfg();
    assert_eq!(resolve_crew_name(&cfg, Some("coder")).unwrap(), "coder");
    assert_eq!(resolve_crew_name(&cfg, None).unwrap(), "coder"); // sole crew
    assert!(resolve_crew_name(&cfg, Some("ghost"))
        .unwrap_err()
        .to_string()
        .contains("no crew named 'ghost'"));
    let empty = Config::default();
    assert!(resolve_crew_name(&empty, None)
        .unwrap_err()
        .to_string()
        .contains("no crews defined"));
}

#[test]
fn model_for_role_from_provider_backend_and_missing() {
    let cfg = crew_cfg();
    assert_eq!(model_for_role(&cfg, "planner").unwrap(), "planner-m");
    assert!(model_for_role(&cfg, "ghost").is_err());
}

/// Role-aware mock: returns the canned JSON each role's prompt expects,
/// keyed by the pinned model. The planner emits an edit that creates the
/// file the verification command checks for, so the crew converges.
struct RoleMock;
#[async_trait::async_trait]
impl Dispatcher for RoleMock {
    async fn dispatch(
        &self,
        _backend: &newt_scheduler::PoolBackend,
        model: &str,
        _req: newt_scheduler::ChatRequest,
    ) -> anyhow::Result<newt_scheduler::ChatReply> {
        let content = match model {
            "nav-m" => r#"{"relevant_files": ["marker.txt"]}"#,
            "planner-m" => r#"{"edits": [{"path": "FIXED.txt", "new_content": "ok\n"}]}"#,
            "triage-m" => r#"{"summary": "missing file", "next_action": "create it"}"#,
            _ => "{}",
        };
        Ok(newt_scheduler::ChatReply {
            content: content.to_string(),
            model_id: model.to_string(),
            usage: None,
        })
    }
}

#[cfg(unix)] // the verification command is a POSIX `test -f`
#[tokio::test]
async fn crew_converges_with_a_fixing_planner() {
    // The crew verify runs CONFINED under the DenyAll egress floor. Where the
    // kernel fs fence or the net guard cannot be established the confined
    // verify fails closed (never runs the repo command unconfined) and the
    // crew cannot converge — so the convergence assertion is only meaningful
    // when both are available (in CI's workspace build they are).
    if !newt_core::confined_exec::kernel_fs_fence_available()
        || !newt_core::confined_exec::net_guard_available()
    {
        return;
    }
    let repo = git_repo();
    let cfg = crew_cfg();
    let args = CrewArgs {
        task: "make the check pass".into(),
        crew: Some("coder".into()),
        dir: Some(repo.path().to_path_buf()),
        // fails until the planner creates FIXED.txt
        test: Some("test -f FIXED.txt".into()),
        max_attempts: Some(2),
        dry_run: false,
    };
    let code = run_with(&cfg, args, &RoleMock).await.unwrap();
    assert_eq!(code, 0, "planner's edit creates FIXED.txt → verify passes");
}

#[tokio::test]
async fn crew_dry_run_resolves_without_touching_the_repo() {
    let repo = git_repo();
    let cfg = crew_cfg();
    let args = CrewArgs {
        task: "noop".into(),
        crew: Some("coder".into()),
        dir: Some(repo.path().to_path_buf()),
        test: Some("true".into()),
        max_attempts: None,
        dry_run: true,
    };
    // dry-run never builds a worktree or dispatches.
    let code = run_with(&cfg, args, &RoleMock).await.unwrap();
    assert_eq!(code, 0);
    assert!(!repo.path().join(".scratch/worktrees").exists());
}

// -- #687: grounding surfaces the real definition, not an earlier-sorting decoy --

#[test]
fn grounding_surfaces_the_definition_even_when_decoys_sort_first() {
    // The #548 shape: newt-cli/crew.rs mentions of `help_lines` (decoys) sort
    // before the real `fn help_lines()` in newt-tui — they must NOT bury it.
    let blocks = vec![GroundingBlock {
        term: "help_lines".to_string(),
        defs: vec![
            "newt-tui/src/lib.rs:8273:fn help_lines() -> &'static [&'static str] {".to_string(),
        ],
        mentions: vec![
            "newt-cli/src/crew.rs:100:    // help_lines rolls up the dgx block".to_string(),
            "newt-cli/src/crew.rs:200:    assert!(out.contains(\"help_lines\"));".to_string(),
            "newt-cli/src/crew.rs:300:    let _ = help_lines_marker;".to_string(),
        ],
    }];
    let out = format_grounding_hits(&blocks);
    assert!(
        out.contains("newt-tui/src/lib.rs:8273") && out.contains("[def]"),
        "the real definition must be surfaced and marked: {out}"
    );
}

#[test]
fn grounding_never_drops_a_definition_under_the_budget() {
    // Many mentions across many terms must not crowd a definition out.
    let blocks: Vec<GroundingBlock> = (0..30)
        .map(|i| GroundingBlock {
            term: format!("sym{i}"),
            defs: vec![format!("src/a.rs:{i}:fn sym{i}() {{")],
            mentions: (0..8)
                .map(|j| format!("src/b.rs:{j}:// sym{i} mention {j}"))
                .collect(),
        })
        .collect();
    let out = format_grounding_hits(&blocks);
    assert!(
        out.contains("[def]"),
        "definitions surface under the budget: {out}"
    );
    // the first hit is a definition, never a mention.
    let first = out
        .lines()
        .find(|l| l.trim_start().starts_with("sym"))
        .unwrap();
    assert!(
        first.contains("[def]"),
        "first hit must be a definition: {first}"
    );
}

#[test]
fn empty_blocks_yield_empty_grounding() {
    assert!(format_grounding_hits(&[]).is_empty());
}

#[test]
fn unresolved_symbol_parses_rustc_errors() {
    assert_eq!(
        unresolved_symbol("error[E0425]: cannot find function `help_lines` in this scope"),
        Some("help_lines".to_string())
    );
    assert_eq!(
        unresolved_symbol("error[E0599]: no method named `roll_up` found for struct"),
        Some("roll_up".to_string())
    );
    assert_eq!(unresolved_symbol("error: mismatched types"), None);
}
