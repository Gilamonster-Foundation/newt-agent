//! #2836: lifecycle inspects retained source only after adoption is validated.
use crate::agentic::{
    display::ToolDisplay,
    tools::{
        disable_ocap_tests::{env_lock, EnvVar},
        ToolCollaborators,
    },
    NoMcp, PromptDisposition,
};
use crate::worktree_adoption::{
    tests::{fixture, link},
    WorktreeSession,
};
use crate::{Caveats, ExecOutcome, Scope};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::OnceLock,
};

#[tokio::test]
async fn lifecycle_retains_source_when_configuration_changes_after_resolution() {
    let _lock = env_lock().await;
    let _confined = EnvVar::set("NEWT_DISABLE_OCAP", "0");
    for (action, initial, replacement, expected) in [
        (
            "run",
            "echo retained-lifecycle-source",
            "git worktree prune --expire now",
            ExecOutcome::Passed,
        ),
        (
            "run",
            "git worktree prune --expire now",
            "echo replacement-source",
            ExecOutcome::Denied,
        ),
        (
            "build",
            "git worktree prune --expire now",
            "echo replacement-source",
            ExecOutcome::Denied,
        ),
    ] {
        let (temp, policy, _) = fixture(false);
        link(&policy);
        let session = WorktreeSession::default();
        session.adopt(policy.clone());
        let caveats = Caveats {
            fs_write: Scope::only([temp.path().to_string_lossy().into_owned()]),
            ..Caveats::top()
        };
        let configuration = Rc::new(RefCell::new(initial.to_string()));
        let reads = Rc::new(Cell::new(0));
        let _resolver = crate::tooling::resolution_fixture::Override::install({
            let configuration = configuration.clone();
            let reads = reads.clone();
            move || {
                reads.set(reads.get() + 1);
                let snapshot = configuration.borrow().clone();
                // Mutate immediately after taking the snapshot, without sleeps.
                // A second read would execute different bytes. Conversely, a later
                // safe value must not authorize a retained destructive source.
                *configuration.borrow_mut() = replacement.into();
                vec![snapshot]
            }
        });
        let outcome = OnceLock::new();
        let mut display = ToolDisplay::new(Vec::new(), false, 80, 20, false);
        let out = super::super::execute(
            &mut display,
            "lifecycle",
            &serde_json::json!({"phase":"test", "action":action}),
            temp.path().join("main").to_str().unwrap(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            ToolCollaborators {
                worktree_session: Some(&session),
                execution: Some(&outcome),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
        )
        .await;
        // #2836 Windows CI: held, symlink-safe metadata reads are Unix-only.
        // An injected adoption must fail closed before even resolving source on
        // unsupported platforms (the same contract as #2733's adoption tests).
        if cfg!(not(unix)) {
            assert_eq!(reads.get(), 0, "invalid adoption resolved source: {out}");
            assert_eq!(&*configuration.borrow(), initial);
            assert!(out.contains(&policy.notice()), "{out}");
            assert_eq!(outcome.get(), Some(&ExecOutcome::Denied), "{out}");
            continue;
        }
        assert_eq!(reads.get(), 1, "configuration was resolved again: {out}");
        assert_eq!(&*configuration.borrow(), replacement);
        if expected == ExecOutcome::Passed {
            assert!(out.contains("retained-lifecycle-source"), "{out}");
        } else {
            assert!(out.contains(super::NOTICE), "{out}");
        }
        assert_eq!(outcome.get(), Some(&expected), "{action}: {out}");
    }
}
