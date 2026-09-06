use super::*;

#[test]
fn conversation_commands_parse_expected_actions() {
    assert_eq!(
        parse_conversation_command("/conversation list").unwrap(),
        ConversationCommand::List
    );
    assert_eq!(
        parse_conversation_command("/conversation show abc").unwrap(),
        ConversationCommand::Show("abc".into())
    );
    assert_eq!(
        parse_conversation_command("/conversation restore abc").unwrap(),
        ConversationCommand::Restore("abc".into())
    );
    assert_eq!(
        parse_conversation_command("/conversation rename abc A better title").unwrap(),
        ConversationCommand::Rename {
            id: "abc".into(),
            title: "A better title".into()
        }
    );
    assert_eq!(
        parse_conversation_command("/conversation delete abc").unwrap(),
        ConversationCommand::Delete("abc".into())
    );
    assert_eq!(
        parse_conversation_command("/conversation rm abc").unwrap(),
        ConversationCommand::Delete("abc".into())
    );
}

// -- /recall (Step 17.4, #246) ------------------------------------------

/// **`/tree` and `/plan` retire under the same per-subcommand rule** (#2009
/// PR12).
///
/// `/roadmap` reads AND mutates, so retiring its two aliases wholesale would
/// either break `/tree` on a pipe (§3.3 protects reads) or let `/plan done`
/// mutate through a shim that is supposed to be dying. The door is decided per
/// subcommand, exactly as `/conversation`'s was.
#[test]
fn the_roadmap_read_subcommands_are_the_ones_a_retired_door_may_serve() {
    use super::roadmap_subcommand_reads as reads;

    // Reads: `/tree` and a bare `/plan` land here, and they keep working.
    assert!(reads(""), "bare /roadmap renders");
    assert!(reads("show"));
    assert!(reads("tree"), "the word /tree retired into");
    assert!(reads("show 3"), "an argument does not make it a write");
    assert!(reads("list"));

    // Mutators: reachable through `/roadmap`, never through a retired door.
    for sub in [
        "new x",
        "use 3",
        "add task x",
        "next",
        "bind",
        "done",
        "drive",
        "import p",
    ] {
        assert!(!reads(sub), "`{sub}` mutates");
    }
    // `export` writes a file, so it is not a read for this purpose either.
    assert!(!reads("export json"));
}

/// `tree` is a REAL subcommand, not just a pointer.
///
/// The retirement message names `/roadmap tree`, and a pointer to something
/// unparseable is worse than no pointer — it is confident. This is the half
/// that would have been missed: the doc said `/roadmap tree` while the code
/// mapped `/tree` to `/roadmap show`.
#[test]
fn roadmap_tree_parses_as_the_view_it_names() {
    let show = super::parse_roadmap_command("/roadmap show").expect("show parses");
    let tree = super::parse_roadmap_command("/roadmap tree").expect("tree parses");
    assert_eq!(show, tree, "tree IS show — one view, two names");
}

/// **A retired READ still reads; a retired MUTATOR redirects** (#2009 PR6b).
///
/// `/conversation` is both, so the rule is applied per SUBCOMMAND. Retiring
/// the whole verb would break `/conversation list` on a pipe (§3.3); letting
/// the whole verb keep working would mean the shim never dies.
#[test]
fn the_retired_conversation_verb_reads_but_does_not_mutate() {
    use super::{conversation_op_plan, ConversationOpPlan};

    // Reads: performed, exactly as before.
    assert_eq!(
        conversation_op_plan("/conversation list").unwrap(),
        ConversationOpPlan::Run
    );
    assert_eq!(
        conversation_op_plan("/conversation show abc").unwrap(),
        ConversationOpPlan::Run
    );

    // Mutators: redirected, and the message names the replacement AND says
    // nothing happened — the two things a habitual typist needs to know.
    for (line, expect) in [
        ("/conversation restore abc", "/resume restore abc"),
        (
            "/conversation rename abc New Title",
            "/resume rename abc New Title",
        ),
        ("/conversation delete abc", "/resume delete abc"),
    ] {
        match conversation_op_plan(line).unwrap() {
            ConversationOpPlan::Redirect(msg) => {
                assert!(msg.contains(expect), "{line}: {msg}");
                assert!(msg.contains("nothing changed"), "{line}: {msg}");
            }
            other => panic!("`{line}` must not mutate through the retired verb: {other:?}"),
        }
    }
}

/// **Every delete asks first, at either door.** The operator's standing rule
/// for this sweep; `/resume delete` is not exempt for being the new spelling.
#[test]
fn deleting_a_conversation_asks_first_and_names_what_is_lost() {
    use super::{conversation_op_plan, ConversationOpPlan};

    match conversation_op_plan("/resume delete abc").unwrap() {
        ConversationOpPlan::Confirm { prompt, id } => {
            assert_eq!(id, "abc");
            assert!(prompt.contains("abc"), "{prompt}");
            assert!(
                prompt.contains("every turn"),
                "names what is lost: {prompt}"
            );
            assert!(prompt.contains("cannot be undone"), "{prompt}");
        }
        other => panic!("a delete must ask: {other:?}"),
    }
    // `rm` is the same act under another name and gets the same question.
    assert!(matches!(
        conversation_op_plan("/resume rm abc").unwrap(),
        ConversationOpPlan::Confirm { .. }
    ));
    // ...and the non-destructive ops are NOT gated behind a question.
    assert_eq!(
        conversation_op_plan("/resume rename abc Title").unwrap(),
        ConversationOpPlan::Run
    );
}

/// Anything but an explicit yes deletes nothing — including a surface that
/// cannot ask at all.
#[test]
fn a_declined_or_unaskable_delete_removes_nothing() {
    use newt_core::interaction_surface::SurfaceInteraction;
    use newt_core::HumanQuestionOutcome;

    let answering =
        |a: &'static str| move |_: &SurfaceInteraction| HumanQuestionOutcome::Answer(a.to_string());
    assert!(super::confirm_conversation_delete(&answering("y"), "q"));
    assert!(super::confirm_conversation_delete(&answering("yes"), "q"));

    assert!(!super::confirm_conversation_delete(&answering("n"), "q"));
    assert!(!super::confirm_conversation_delete(&answering(""), "q"));
    assert!(
        !super::confirm_conversation_delete(&answering("delete it please"), "q"),
        "an unresolvable answer is a no, not a yes"
    );

    // A surface with no way to ask — a pipe, EOF, Esc — declines. §3.3: those
    // are outcomes, never an implied answer.
    let cancelled = |_: &SurfaceInteraction| HumanQuestionOutcome::Cancelled;
    assert!(!super::confirm_conversation_delete(&cancelled, "q"));
}

/// Only the five named subcommands reach the conversation ops; everything else
/// after `/resume` is still a search.
#[test]
fn resume_routes_only_its_subcommands_to_the_conversation_ops() {
    use super::resume_conversation_subcommand as sub;
    for word in ["list", "show", "restore", "rename", "delete", "rm"] {
        assert!(sub(&format!("resume {word} x")), "{word}");
    }
    assert!(sub("resume list"));
    // A query that merely STARTS like one is a search, not an op.
    assert!(!sub("resume restored"), "restored is a search term");
    assert!(!sub("resume listing"), "listing is a search term");
    assert!(!sub("resume"), "bare /resume is browse");
    assert!(!sub("resume tokio panic"));
    assert!(!sub("resumex list"));
}

/// **The retired `/recall` parses as `/resume find`** (#2009 PR6).
///
/// Every case the old `parse_recall_command` pinned, now asserted against the
/// parser that replaced it — the point of the fold is that these are the SAME
/// behaviour, so the evidence has to be the same cases.
#[test]
fn recall_commands_parse_as_the_find_they_retired_into() {
    assert_eq!(
        parse_resume_command("/recall"),
        ResumeCommand::Find(String::new())
    );
    assert_eq!(
        parse_resume_command("/recall   "),
        ResumeCommand::Find(String::new())
    );
    assert_eq!(
        parse_resume_command("/recall tokio panic"),
        ResumeCommand::Find("tokio panic".into())
    );
    // `/recallx` is some other (unknown) command, not `/recall x`. The old
    // parser returned an error; this one declines to claim the line, which is
    // the same refusal in the shape the resume parser speaks.
    assert_ne!(
        parse_resume_command("/recallx"),
        ResumeCommand::Find("x".into())
    );
    assert_ne!(
        parse_resume_command("/conversation list"),
        ResumeCommand::Find("list".into())
    );
}

/// `find` is search that never reopens — the half of `/recall` that `/resume
/// <token>` could not express, because a token resolving as an id reopens it.
#[test]
fn resume_find_searches_without_reopening() {
    assert_eq!(
        parse_resume_command("/resume find"),
        ResumeCommand::Find(String::new())
    );
    assert_eq!(
        parse_resume_command("/resume find tokio panic"),
        ResumeCommand::Find("tokio panic".into())
    );
    // An id-shaped token under `find` STAYS a search: that is the distinction
    // the subcommand exists to make.
    assert_eq!(
        parse_resume_command("/resume find 175200000000"),
        ResumeCommand::Find("175200000000".into())
    );
    assert_eq!(
        parse_resume_command("/resume 175200000000"),
        ResumeCommand::Query("175200000000".into())
    );
    // `/resume findings` searches for "findings", not an empty find.
    assert_eq!(
        parse_resume_command("/resume findings"),
        ResumeCommand::Query("findings".into())
    );
}

#[test]
fn resume_commands_parse_expected_actions() {
    assert_eq!(parse_resume_command("/resume"), ResumeCommand::Browse);
    assert_eq!(parse_resume_command("/resume   "), ResumeCommand::Browse);
    assert_eq!(parse_resume_command("/resume 3"), ResumeCommand::Select(3));
    assert_eq!(
        parse_resume_command("/resume tokio panic"),
        ResumeCommand::Query("tokio panic".into())
    );
    // #1030: a big all-digits token (the displayed short id is ~19 nanos
    // digits) is an id PREFIX, not a row number — routed to Query/resolve_id
    // so the short id the UI prints is typeable.
    assert_eq!(
        parse_resume_command("/resume 175200000000"),
        ResumeCommand::Query("175200000000".into())
    );
}

#[test]
fn resume_browse_numbers_rows_and_marks_liveness() {
    let (_state, _ws, mut store) = recall_test_store();
    store.set_owner_for_test("host", "boot", 1);
    let a = store.create("Alpha", None).unwrap();
    let b = store.create("Bravo", None).unwrap();
    // Bravo is held by a live owner -> ● ; Alpha is unclaimed but is the
    // "active" id passed below -> ▶.
    store.set_liveness_for_test(|_, _| true);
    store.claim(&b).unwrap();
    let (msg, ids) = resume_browse_message(&store, &a).unwrap();
    // list() is MRU, so Bravo (created last) is row 1, Alpha row 2.
    assert_eq!(ids, vec![b.clone(), a.clone()]);
    assert!(msg.contains("1. ●"), "held conversation marked live: {msg}");
    assert!(msg.contains("2. ▶"), "the active id marked current: {msg}");
    assert!(msg.contains("Alpha") && msg.contains("Bravo"));
}

#[test]
fn resume_search_lists_one_row_per_conversation() {
    let (_state, _ws, store) = recall_test_store();
    let id = store.create("Parser work", None).unwrap();
    store
        .append_turn(&id, "fix the parser tokens", "done")
        .unwrap();
    store.append_turn(&id, "more parser tokens", "ok").unwrap();
    let (msg, ids) = resume_search_message(&store, "parser", "other-active").unwrap();
    // Two matching turns in ONE conversation -> a single numbered row.
    assert_eq!(ids, vec![id]);
    assert!(msg.contains("1. "), "numbered: {msg}");
    assert!(msg.contains("Parser work"));
}
