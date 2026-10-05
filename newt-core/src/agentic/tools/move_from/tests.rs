use super::transaction::Files;
use super::*;

/// #2724: never interpret text or a masked/nonzero exit as a checked move.
#[test]
fn check_receipt_requires_real_zero_and_confinement() {
    let mut output = ConfinedOutput {
        success: true,
        code: Some(0),
        stdout: Vec::new(),
        stderr: Vec::new(),
        sandbox_kind: agent_bridle::SandboxKind::None,
        timed_out: false,
    };
    assert!(checked_output(&output).is_err());
    output.sandbox_kind = agent_bridle::SandboxKind::Landlock;
    assert!(checked_output(&output).is_ok());
    output.success = false;
    output.code = Some(101);
    output.stderr = vec![b'x'; 10_000];
    let diagnostic = checked_output(&output).unwrap_err();
    assert!(diagnostic.contains("101"));
    assert!(diagnostic.len() < 6200);
    output.success = true;
    output.code = None;
    assert!(checked_output(&output).is_err());
    output.code = Some(0);
    output.timed_out = true;
    assert!(checked_output(&output).is_err());
}

/// #2724: the familiar write_file schema must disclose extraction and its keys.
#[test]
fn catalog_advertises_move() {
    let catalog = super::super::catalog::tool_definitions();
    let write = catalog
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["function"]["name"] == "write_file")
        .unwrap();
    assert!(write["function"]["description"]
        .as_str()
        .unwrap()
        .contains("move_from"));
    assert_eq!(
        write["function"]["parameters"]["properties"]["move_from"]["required"],
        serde_json::json!(["path", "items"])
    );
}

/// #2724/#2723: sibling grants select the build root, while missing grants
/// return the shared actionable refusal before any approval or mutation.
#[test]
fn sibling_move_requires_existing_write_authority() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let session = root.join("session");
    let sibling = root.join("sibling");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::create_dir_all(sibling.join("src")).unwrap();
    std::fs::write(
        sibling.join("Cargo.toml"),
        "[package]\nname = \"sibling\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let source = sibling.join("src/lib.rs");
    let child = sibling.join("src/helpers.rs");
    let before = "fn helper() {}\n";
    std::fs::write(&source, before).unwrap();
    let args = serde_json::json!({"path":child, "content":"", "move_from":{"path":source, "items":["helper"]}});
    let mut caveats = Caveats {
        fs_read: crate::Scope::only([root.to_string_lossy().into_owned()]),
        fs_write: crate::Scope::only([session.to_string_lossy().into_owned()]),
        exec: crate::Scope::none(),
        net: crate::Scope::none(),
        ..Caveats::top()
    };
    let denied = execute(&args, session.to_str().unwrap(), &caveats, None, &mut None)
        .err()
        .unwrap();
    assert!(
        denied.contains("grant the worktree with --write <worktree>"),
        "{denied}"
    );
    for writes in [
        crate::Scope::only([sibling.to_string_lossy().into_owned()]),
        crate::Scope::All,
    ] {
        caveats.fs_write = writes;
        let admitted = execute(&args, session.to_str().unwrap(), &caveats, None, &mut None)
            .err()
            .unwrap();
        assert!(
            admitted.contains("explicit confined build authority"),
            "{admitted}"
        );
    }
    assert_eq!(std::fs::read_to_string(source).unwrap(), before);
    assert!(!child.exists());
}

/// CI-only real-resource proof grounding the mocked AST/transaction/check seam:
/// a tiny dependency-free library is checked before/after extraction, then an
/// actual compiler error restores both files. No kernel skip counts as a pass.
/// Run the explicit --ignored command in docs/design/write-file-move-from.md.
#[test]
#[ignore = "CI-only: real confined cargo check; needs installed Rust and kernel build sandbox"]
fn real_move_from_fixture_compiles_and_failed_check_restores() {
    let fixture = tempfile::tempdir().unwrap();
    let base = fixture.path().canonicalize().unwrap();
    let session = base.join("session");
    let root = base.join("sibling");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    // Checking the session instead of the authorized sibling must fail.
    std::fs::write(session.join("Cargo.toml"), "invalid manifest").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"move_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n[lib]\npath = \"lib.rs\"\n").unwrap();
    let source = root.join("lib.rs");
    let child = root.join("helpers.rs");
    let before = "const N: u8 = 7;\n/// Exact docs\n#[inline]\nfn helper() -> u8 { N }\npub fn result() -> u8 { helper() }\n";
    std::fs::write(&source, before).unwrap();
    let files = DiskFiles::open(&root, &source, &child).unwrap();
    let moved = extract::extract(before, &["helper".into()], "helpers").unwrap();
    let request = check_request(
        &root,
        &root,
        "move_fixture",
        &Caveats {
            net: crate::caveats::Scope::none(),
            ..Caveats::top()
        },
    );
    let caveats = Caveats {
        fs_read: crate::caveats::Scope::only([root.to_string_lossy().into_owned()]),
        fs_write: crate::caveats::Scope::only([root.to_string_lossy().into_owned()]),
        exec: crate::caveats::Scope::none(),
        net: crate::caveats::Scope::none(),
        ..Caveats::top()
    };
    let args = serde_json::json!({"path":child, "content":"", "move_from":{"path":source, "items":["helper"]}});
    let denied = execute(&args, session.to_str().unwrap(), &caveats, None, &mut None);
    assert!(denied
        .err()
        .unwrap()
        .contains("explicit confined build authority"));
    assert_eq!(std::fs::read_to_string(&source).unwrap(), before);
    assert!(!child.exists());
    struct ApproveBuild(usize);
    impl PermissionGate for ApproveBuild {
        fn ask_question(
            &mut self,
            _: &str,
        ) -> super::super::super::permissions::HumanQuestionOutcome {
            panic!("scoped fs_write needs only the Build authority prompt");
        }
        fn ask(
            &mut self,
            requests: &[super::super::super::permissions::PermissionRequest],
        ) -> PermissionDecision {
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].kind,
                super::super::super::permissions::DenialKind::Build
            );
            self.0 += 1;
            PermissionDecision::Allow(Caveats::top())
        }
    }
    let mut gate = ApproveBuild(0);
    execute(
        &args,
        session.to_str().unwrap(),
        &caveats,
        None,
        &mut Some(&mut gate),
    )
    .unwrap();
    assert_eq!(gate.0, 1);

    assert_eq!(std::fs::read_to_string(&source).unwrap(), moved.source);
    assert_eq!(std::fs::read_to_string(&child).unwrap(), moved.child);
    files
        .change(transaction::File::Source, Some(&moved.source), Some(before))
        .unwrap();
    files
        .change(transaction::File::Child, Some(&moved.child), None)
        .unwrap();
    let broken = extract::Extracted {
        source: moved.source,
        child: "not Rust".into(),
    };
    let failure = transaction::run(&files, before, &broken, || check(&request)).unwrap_err();
    assert!(failure.contains("original files restored"), "{failure}");
    assert_eq!(std::fs::read_to_string(source).unwrap(), before);
    assert!(!child.exists());
}
