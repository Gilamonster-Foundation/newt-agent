use super::*;

#[test]
fn roadmap_commands_parse_expected_actions() {
    use newt_core::plan::NodeKind;
    assert_eq!(
        parse_roadmap_command("/roadmap").unwrap(),
        RoadmapCommand::Show(None)
    );
    assert_eq!(
        parse_roadmap_command("/roadmap list").unwrap(),
        RoadmapCommand::List
    );
    assert_eq!(
        parse_roadmap_command("/roadmap show rm-1").unwrap(),
        RoadmapCommand::Show(Some("rm-1".into()))
    );
    assert_eq!(
        parse_roadmap_command("/roadmap new Mermaid in Rust").unwrap(),
        RoadmapCommand::New("Mermaid in Rust".into())
    );
    assert_eq!(
        parse_roadmap_command("/roadmap use rm-1").unwrap(),
        RoadmapCommand::Use("rm-1".into())
    );
    assert_eq!(
        parse_roadmap_command("/roadmap add phase Build the parser").unwrap(),
        RoadmapCommand::Add {
            kind: NodeKind::Phase,
            title: "Build the parser".into(),
            under: None
        }
    );
    assert_eq!(
        parse_roadmap_command("/roadmap add plan Implement it under phase-1").unwrap(),
        RoadmapCommand::Add {
            kind: NodeKind::Plan,
            title: "Implement it".into(),
            under: Some("phase-1".into())
        }
    );
    assert!(parse_roadmap_command("/roadmap new").is_err());
    assert!(parse_roadmap_command("/roadmap add nonsense x").is_err());
}

#[test]
fn render_roadmap_tree_outlines_nodes_by_depth() {
    let toml = "\
[[subtask]]
id = \"road\"
instruction = \"the roadmap\"
kind = \"roadmap\"

[[subtask]]
id = \"phase-1\"
instruction = \"phase one\"
kind = \"phase\"
parent = \"road\"
";
    let tree = newt_core::plan::Plan::from_toml_str(toml).unwrap();
    let rm = newt_core::Roadmap {
        id: "rm-123456789012".into(),
        title: "Demo".into(),
        tree,
    };
    let out = render_roadmap_tree(&rm);
    assert!(out.contains("Roadmap: Demo"));
    assert!(out.contains("roadmap [road]"));
    assert!(out.contains("phase [phase-1]"));
    // The child (phase-1) is indented deeper than its parent (road).
    let road_indent = out
        .lines()
        .find(|l| l.contains("[road]"))
        .unwrap()
        .find('○');
    let phase_indent = out
        .lines()
        .find(|l| l.contains("[phase-1]"))
        .unwrap()
        .find('○');
    assert!(phase_indent > road_indent, "child indented deeper: {out}");
}

#[test]
fn empty_roadmap_renders_a_hint() {
    let rm = newt_core::Roadmap {
        id: "rm-1".into(),
        title: "Empty".into(),
        tree: newt_core::plan::Plan::default(),
    };
    assert!(render_roadmap_tree(&rm).contains("no nodes yet"));
}

#[test]
fn next_roadmap_node_id_avoids_collisions() {
    let mut tree = newt_core::plan::Plan::default();
    assert_eq!(next_roadmap_node_id(&tree), "node-1");
    tree.subtasks.push(newt_core::plan::Subtask::node(
        "node-1",
        "x",
        newt_core::plan::NodeKind::Task,
        None,
    ));
    assert_eq!(next_roadmap_node_id(&tree), "node-2");
}

#[test]
fn roadmap_drive_subcommands_parse() {
    assert_eq!(
        parse_roadmap_command("/roadmap next").unwrap(),
        RoadmapCommand::Next
    );
    assert_eq!(
        parse_roadmap_command("/roadmap work").unwrap(),
        RoadmapCommand::Next
    );
    assert_eq!(
        parse_roadmap_command("/roadmap bind").unwrap(),
        RoadmapCommand::Bind(None)
    );
    assert_eq!(
        parse_roadmap_command("/roadmap bind node-3").unwrap(),
        RoadmapCommand::Bind(Some("node-3".into()))
    );
    assert_eq!(
        parse_roadmap_command("/roadmap done").unwrap(),
        RoadmapCommand::Done(None)
    );
    assert_eq!(
        parse_roadmap_command("/roadmap done node-3").unwrap(),
        RoadmapCommand::Done(Some("node-3".into()))
    );
    assert_eq!(
        parse_roadmap_command("/roadmap eval").unwrap(),
        RoadmapCommand::Eval(None)
    );
    assert_eq!(
        parse_roadmap_command("/roadmap eval node-3").unwrap(),
        RoadmapCommand::Eval(Some("node-3".into()))
    );
    assert_eq!(
        parse_roadmap_command("/roadmap drive").unwrap(),
        RoadmapCommand::Drive
    );
    // #1062: task <node> commit [sha] — HEAD when the sha is omitted.
    assert_eq!(
        parse_roadmap_command("/roadmap task node-4 commit").unwrap(),
        RoadmapCommand::TaskCommit {
            node: "node-4".into(),
            sha: None
        }
    );
    assert_eq!(
        parse_roadmap_command("/roadmap task node-4 commit b56fefa").unwrap(),
        RoadmapCommand::TaskCommit {
            node: "node-4".into(),
            sha: Some("b56fefa".into())
        }
    );
    assert!(
        parse_roadmap_command("/roadmap task node-4").is_err(),
        "task without `commit` is a usage error"
    );
    assert!(
        parse_roadmap_command("/roadmap task").is_err(),
        "task without a node is a usage error"
    );
}

/// #1062 auto-capture decision (pure): a new commit in a bound Plan's turn
/// targets that Plan's next uncaptured Task; no commit / unbound / no ready
/// task → nothing captured.
#[test]
fn autocapture_target_picks_the_bound_plans_next_task_on_a_new_commit() {
    let toml = r#"
[[subtask]]
id = "pl"
instruction = "plan"
kind = "plan"
conversation_id = "conv-1"

[[subtask]]
id = "t1"
instruction = "task 1"
kind = "task"
parent = "pl"
"#;
    let mut tree = newt_core::plan::Plan::from_toml_str(toml).unwrap();
    // No new commit → nothing.
    assert_eq!(
        autocapture_target(&tree, "conv-1", Some("abc"), "abc"),
        None
    );
    // New commit + bound Plan with a pending Task → that Task.
    assert_eq!(
        autocapture_target(&tree, "conv-1", Some("abc"), "def"),
        Some("t1".into())
    );
    // A first commit from an unborn HEAD (None before) still counts.
    assert_eq!(
        autocapture_target(&tree, "conv-1", None, "def"),
        Some("t1".into())
    );
    // A conversation NOT bound to any Plan → nothing.
    assert_eq!(autocapture_target(&tree, "other-conv", None, "def"), None);
    // Once the Plan's only Task is captured, a later commit finds no target.
    tree.set_artifact_commit("t1", "def", None);
    assert_eq!(
        autocapture_target(&tree, "conv-1", Some("abc"), "ghi"),
        None
    );
}

#[test]
fn render_marks_the_next_ready_node_with_the_cursor() {
    // road (branch, pending) → task-1 (leaf, pending). next_ready_node = task-1.
    let toml = "\
[[subtask]]
id = \"road\"
instruction = \"the roadmap\"
kind = \"roadmap\"

[[subtask]]
id = \"task-1\"
instruction = \"do it\"
kind = \"task\"
parent = \"road\"
";
    let tree = newt_core::plan::Plan::from_toml_str(toml).unwrap();
    let rm = newt_core::Roadmap {
        id: "rm-1".into(),
        title: "Demo".into(),
        tree,
    };
    let out = render_roadmap_tree(&rm);
    // The cursor ▶ sits on task-1 (the next-ready node), not on the branch.
    let task_line = out.lines().find(|l| l.contains("[task-1]")).unwrap();
    assert!(task_line.contains('▶'), "cursor on next-ready node: {out}");
    let road_line = out.lines().find(|l| l.contains("[road]")).unwrap();
    assert!(!road_line.contains('▶'), "branch is not the cursor: {out}");
}

#[test]
fn roadmap_bind_eval_and_done_drive_a_node_through_its_status() {
    let (_state, ws_dir, store) = recall_test_store();
    let ws = ws_dir.path().to_str().unwrap();
    let conv = "1781000000000000000-abcd"; // a stand-in active conversation id
    let mut active_roadmap: Option<String> = None;

    // Author a roadmap with one Plan node.
    handle_roadmap_command(
        "/roadmap new Build it",
        &store,
        &mut active_roadmap,
        conv,
        ws,
    )
    .unwrap();
    handle_roadmap_command(
        "/roadmap add plan Parser",
        &store,
        &mut active_roadmap,
        conv,
        ws,
    )
    .unwrap();
    let rm_id = active_roadmap.clone().unwrap();

    // /roadmap next reports the plan node needs a conversation (unbound).
    let next =
        handle_roadmap_command("/roadmap next", &store, &mut active_roadmap, conv, ws).unwrap();
    assert!(
        next.message.contains("Bind"),
        "unbound plan: {}",
        next.message
    );
    assert!(next.switch_to.is_none());

    // Bind THIS conversation to it → node goes Running and gets the conv id.
    handle_roadmap_command("/roadmap bind", &store, &mut active_roadmap, conv, ws).unwrap();
    let node = store.load_roadmap(&rm_id).unwrap().unwrap().tree.subtasks[0].clone();
    assert_eq!(node.status, newt_core::plan::SubtaskStatus::Running);
    assert_eq!(node.conversation_id.as_deref(), Some(conv));

    // /roadmap next now resumes-to-cursor: it hands back the bound conversation.
    let next2 =
        handle_roadmap_command("/roadmap next", &store, &mut active_roadmap, conv, ws).unwrap();
    assert_eq!(next2.switch_to.as_deref(), Some(conv));

    // /roadmap eval on the (childless) Plan node evaluates NOT done — no
    // objective evidence (no child tasks) — so it is not marked Done.
    let eval =
        handle_roadmap_command("/roadmap eval", &store, &mut active_roadmap, conv, ws).unwrap();
    assert!(
        eval.message.contains("not done yet"),
        "eval: {}",
        eval.message
    );
    assert_ne!(
        store.load_roadmap(&rm_id).unwrap().unwrap().tree.subtasks[0].status,
        newt_core::plan::SubtaskStatus::Done
    );

    // /roadmap done (defaulting to the bound node) marks it Done manually.
    handle_roadmap_command("/roadmap done", &store, &mut active_roadmap, conv, ws).unwrap();
    let done = store.load_roadmap(&rm_id).unwrap().unwrap().tree.subtasks[0].status;
    assert_eq!(done, newt_core::plan::SubtaskStatus::Done);
}

// ── #1082 roadmap-as-code: /roadmap export + import ─────────────────────
// The file edge is injected (in-memory closures), so these stay in the
// fully-mocked unit tier — no fs I/O beyond the store the shared
// `recall_test_store()` helper already provides.

#[test]
fn roadmap_export_import_commands_parse() {
    assert_eq!(
        parse_roadmap_command("/roadmap export").unwrap(),
        RoadmapCommand::Export(None)
    );
    assert_eq!(
        parse_roadmap_command("/roadmap export plans/r.toml").unwrap(),
        RoadmapCommand::Export(Some("plans/r.toml".into()))
    );
    assert_eq!(
        parse_roadmap_command("/roadmap import").unwrap(),
        RoadmapCommand::Import(None)
    );
    assert_eq!(
        parse_roadmap_command("/roadmap import /tmp/r.toml").unwrap(),
        RoadmapCommand::Import(Some("/tmp/r.toml".into()))
    );
}

// ── #1083: /roadmap issue — bind a node to the forge issue it realizes ──

#[test]
fn roadmap_issue_command_parses_plain_and_hash_numbers() {
    assert_eq!(
        parse_roadmap_command("/roadmap issue node-1 39").unwrap(),
        RoadmapCommand::IssueSet {
            node: "node-1".into(),
            number: 39
        }
    );
    // The friendly `#39` form binds the same.
    assert_eq!(
        parse_roadmap_command("/roadmap issue node-1 #39").unwrap(),
        RoadmapCommand::IssueSet {
            node: "node-1".into(),
            number: 39
        }
    );
    assert!(parse_roadmap_command("/roadmap issue node-1").is_err());
    assert!(parse_roadmap_command("/roadmap issue node-1 nope").is_err());
    assert!(parse_roadmap_command("/roadmap issue").is_err());
}

#[test]
fn roadmap_issue_binds_the_ref_on_any_node_and_rejects_unknown_ids() {
    let (_state, ws_dir, store) = recall_test_store();
    let ws = ws_dir.path().to_str().unwrap();
    let conv = "1781000000000000000-abcd";
    let mut active: Option<String> = None;
    handle_roadmap_command("/roadmap new Gated", &store, &mut active, conv, ws).unwrap();
    handle_roadmap_command("/roadmap add phase P1", &store, &mut active, conv, ws).unwrap();
    let rm_id = active.clone().unwrap();

    // Binds on a PHASE (the gate is kind-agnostic), persists to the store.
    let out =
        handle_roadmap_command("/roadmap issue node-1 #39", &store, &mut active, conv, ws).unwrap();
    assert!(out.message.contains("issue #39"), "{}", out.message);
    let node = store.load_roadmap(&rm_id).unwrap().unwrap().tree.subtasks[0].clone();
    assert_eq!(node.artifact_ref.as_ref().and_then(|a| a.issue), Some(39));

    // Unknown node id fails loud, store unchanged.
    let err = handle_roadmap_command("/roadmap issue ghost 1", &store, &mut active, conv, ws)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no node `ghost`"), "{err}");
}

#[test]
fn roadmap_file_path_resolves_default_relative_and_absolute() {
    let default = roadmap_file_path("/ws", None);
    assert_eq!(
        default,
        std::path::Path::new("/ws").join(newt_core::roadmap_file::DEFAULT_ROADMAP_FILE)
    );
    // Relative args are workspace-relative — the file belongs to the repo.
    assert_eq!(
        roadmap_file_path("/ws", Some("plans/r.toml")),
        std::path::PathBuf::from("/ws/plans/r.toml")
    );
    assert_eq!(
        roadmap_file_path("/ws", Some("/abs/r.toml")),
        std::path::PathBuf::from("/abs/r.toml")
    );
}

#[test]
fn roadmap_export_then_import_round_trips_and_upserts_by_id() {
    let (_state, ws_dir, store) = recall_test_store();
    let ws = ws_dir.path().to_str().unwrap();
    let conv = "1781000000000000000-abcd";
    let mut active: Option<String> = None;

    // Author a two-node roadmap, then export it through a fake fs.
    handle_roadmap_command("/roadmap new Chartered", &store, &mut active, conv, ws).unwrap();
    handle_roadmap_command("/roadmap add phase P1", &store, &mut active, conv, ws).unwrap();
    handle_roadmap_command(
        "/roadmap add plan Body under node-1",
        &store,
        &mut active,
        conv,
        ws,
    )
    .unwrap();
    let rm_id = active.clone().unwrap();

    let written: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    let out = export_roadmap_to(
        &store,
        &rm_id,
        std::path::Path::new("/repo/.newt/roadmap.toml"),
        &|_, text| {
            *written.borrow_mut() = Some(text.to_string());
            Ok(())
        },
    )
    .unwrap();
    assert!(out.message.contains("2 nodes"), "{}", out.message);
    let exported = written.borrow().clone().unwrap();

    // Drift the working copy past the export…
    handle_roadmap_command("/roadmap add phase Stray", &store, &mut active, conv, ws).unwrap();
    assert_eq!(
        store
            .load_roadmap(&rm_id)
            .unwrap()
            .unwrap()
            .tree
            .subtasks
            .len(),
        3
    );

    // …then import restores the repo authority IN PLACE (same id, updated).
    let mut fresh_active: Option<String> = None;
    let out = import_roadmap_from(
        &store,
        &mut fresh_active,
        std::path::Path::new("/repo/.newt/roadmap.toml"),
        &|_| Ok(exported.clone()),
    )
    .unwrap();
    assert!(out.message.contains("updated existing"), "{}", out.message);
    assert_eq!(fresh_active.as_deref(), Some(rm_id.as_str()));
    let restored = store.load_roadmap(&rm_id).unwrap().unwrap();
    assert_eq!(restored.tree.subtasks.len(), 2);
    assert_eq!(restored.title, "Chartered");

    // Round-trip is byte-identical: re-export matches the imported text.
    let rewritten: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    export_roadmap_to(&store, &rm_id, std::path::Path::new("/x"), &|_, text| {
        *rewritten.borrow_mut() = Some(text.to_string());
        Ok(())
    })
    .unwrap();
    assert_eq!(rewritten.borrow().clone().unwrap(), exported);
}

#[test]
fn roadmap_import_into_empty_workspace_creates_and_activates() {
    let (_state, _ws, store) = recall_test_store();
    let text = newt_core::roadmap_file::RoadmapFile::new(
        "rm-fresh",
        "Bootstrapped",
        newt_core::plan::Plan::default(),
    )
    .to_toml_string()
    .unwrap();
    let mut active: Option<String> = None;
    let out = import_roadmap_from(
        &store,
        &mut active,
        std::path::Path::new("/repo/.newt/roadmap.toml"),
        &|_| Ok(text.clone()),
    )
    .unwrap();
    assert!(out.message.contains("created new"), "{}", out.message);
    assert_eq!(active.as_deref(), Some("rm-fresh"));
    assert!(store.load_roadmap("rm-fresh").unwrap().is_some());
}

#[test]
fn roadmap_import_corrupt_file_fails_loud_and_leaves_store_untouched() {
    let (_state, ws_dir, store) = recall_test_store();
    let ws = ws_dir.path().to_str().unwrap();
    let conv = "1781000000000000000-abcd";
    let mut active: Option<String> = None;
    handle_roadmap_command("/roadmap new Keep me", &store, &mut active, conv, ws).unwrap();
    let rm_id = active.clone().unwrap();

    // Corrupt file: parse fails BEFORE any store write; active id keeps.
    let err = import_roadmap_from(
        &store,
        &mut active,
        std::path::Path::new("/repo/.newt/roadmap.toml"),
        &|_| Ok("not = [toml".to_string()),
    )
    .unwrap_err();
    assert!(!err.to_string().is_empty());
    assert_eq!(active.as_deref(), Some(rm_id.as_str()));
    assert_eq!(store.list_roadmaps().unwrap().len(), 1);

    // Missing file: a friendly error naming the path and the bootstrap hint.
    let err = import_roadmap_from(
        &store,
        &mut active,
        std::path::Path::new("/repo/.newt/roadmap.toml"),
        &|_| Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone")),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains(".newt/roadmap.toml"), "{err}");
    assert!(err.contains("/roadmap export"), "{err}");
}

#[test]
fn roadmap_export_without_active_roadmap_is_a_friendly_error() {
    let (_state, ws_dir, store) = recall_test_store();
    let ws = ws_dir.path().to_str().unwrap();
    let mut active: Option<String> = None;
    let err = handle_roadmap_command(
        "/roadmap export",
        &store,
        &mut active,
        "1781000000000000000-abcd",
        ws,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("no active roadmap"), "{err}");
}
