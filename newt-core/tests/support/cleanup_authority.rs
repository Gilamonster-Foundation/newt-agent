//! Native cleanup shares the normal command authority and kernel filesystem fence.

use std::path::Path;

use newt_core::{
    execute_tool, Caveats, DenialKind, HumanQuestionOutcome, NoMcp, PermissionDecision,
    PermissionGate, PermissionRequest, Scope,
};

struct OnceGate {
    allow: bool,
    requests: Vec<(DenialKind, String)>,
}

impl PermissionGate for OnceGate {
    fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
        panic!("native cleanup must preserve its invocation baseline");
    }

    fn ask_with_caveats(
        &mut self,
        baseline: &Caveats,
        requests: &[PermissionRequest],
    ) -> PermissionDecision {
        let grants: Vec<_> = requests
            .iter()
            .map(|r| (r.kind, r.target.clone()))
            .collect();
        self.requests.extend(grants.clone());
        if self.allow {
            PermissionDecision::Allow(newt_core::agentic::widen_caveats(baseline, &grants))
        } else {
            PermissionDecision::Deny
        }
    }

    fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
        HumanQuestionOutcome::Unavailable
    }
}

async fn command(
    workspace: &Path,
    args: serde_json::Value,
    caveats: &Caveats,
    gate: Option<&mut dyn PermissionGate>,
) -> String {
    execute_tool(
        "run_command",
        &args,
        &workspace.to_string_lossy(),
        false,
        100,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        gate,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

fn tree(path: &Path) {
    std::fs::create_dir_all(path.join("nested")).unwrap();
    std::fs::write(path.join("nested/keep.txt"), b"unchanged").unwrap();
}

fn assert_preserved(path: &Path, output: &str) {
    assert_eq!(
        std::fs::read(path.join("nested/keep.txt")).unwrap(),
        b"unchanged",
        "cleanup escaped its granted scope: {output}"
    );
}

pub async fn run(root: &Path) {
    let workspace = root.join("cleanup-workspace");
    let outside = root.join("cleanup-outside");
    std::fs::create_dir(&workspace).unwrap();
    let approved = outside.join("approved tree");
    let sibling = outside.join("unapproved tree");
    tree(&approved);
    tree(&sibling);

    let mut baseline = newt_core::confined_exec::workspace_confined_caveats(&workspace);
    baseline.exec = Scope::only(["/bin/rm".into()]);
    // Existing network authority is separate from approval of this deletion.
    baseline.net = Scope::All;
    let source = format!("/bin/rm -rf -- '{}'", approved.display());
    let declared = serde_json::json!({
        "command": source,
        "fs_read": [approved],
        "fs_write": [approved],
    });
    let mut gate = OnceGate {
        allow: false,
        requests: Vec::new(),
    };

    let refused = command(&workspace, declared.clone(), &baseline, Some(&mut gate)).await;
    assert_preserved(&approved, &refused);
    assert_preserved(&sibling, &refused);
    assert_eq!(
        gate.requests,
        vec![
            (DenialKind::FsRead, approved.to_string_lossy().into_owned()),
            (DenialKind::FsWrite, approved.to_string_lossy().into_owned()),
        ],
        "a scoped cleanup must reach the ordinary approval gate: {refused}"
    );

    gate.allow = true;
    let allowed = command(&workspace, declared.clone(), &baseline, Some(&mut gate)).await;
    assert!(
        !approved.exists(),
        "approved native cleanup did not execute: {allowed}"
    );
    assert_preserved(&sibling, &allowed);

    // An allow-once decision does not become a standing grant on the next call.
    tree(&approved);
    let undeclared = command(
        &workspace,
        serde_json::json!({"command": source}),
        &baseline,
        None,
    )
    .await;
    assert_preserved(&approved, &undeclared);
    assert_preserved(&sibling, &undeclared);

    // Even an admitted process cannot delete its unapproved sibling. The first
    // operand proves the process ran; the second exercises the actual kernel fence.
    let bounded = command(
        &workspace,
        serde_json::json!({
            "command": format!("/bin/rm -rf -- '{}' '{}'", approved.display(), sibling.display()),
            "fs_read": [approved],
            "fs_write": [approved],
        }),
        &baseline,
        Some(&mut gate),
    )
    .await;
    assert!(
        !approved.exists(),
        "the authorized operand must be deleted: {bounded}"
    );
    assert_preserved(&sibling, &bounded);

    // Filesystem authority never implies command authority.
    tree(&approved);
    let mut no_exec = newt_core::agentic::widen_caveats(
        &baseline,
        &[
            (DenialKind::FsRead, approved.to_string_lossy().into_owned()),
            (DenialKind::FsWrite, approved.to_string_lossy().into_owned()),
        ],
    );
    no_exec.exec = Scope::none();
    let refused = command(
        &workspace,
        serde_json::json!({"command": source}),
        &no_exec,
        None,
    )
    .await;
    assert_preserved(&approved, &refused);
    gate.requests.clear();
    let admitted = command(
        &workspace,
        serde_json::json!({"command": source}),
        &no_exec,
        Some(&mut gate),
    )
    .await;
    assert!(
        !approved.exists(),
        "approved exec must run inside its filesystem grant: {admitted}"
    );
    assert_eq!(gate.requests, vec![(DenialKind::Exec, "/bin/rm".into())]);
    assert_preserved(&sibling, &admitted);

    // A directory grant admits ordinary create/write/rename/delete commands
    // throughout that directory without per-command or per-file permission.
    let directory = newt_core::ToolPermissions {
        preset: newt_core::PermissionPreset::WorkspaceFullAccess,
        net: vec!["*".into()],
        ..Default::default()
    }
    .to_caveats(workspace.to_str().unwrap());
    let created = command(
        &workspace,
        serde_json::json!({"command": "mkdir -p ordinary/nested; printf contents > ordinary/nested/first; mv ordinary/nested/first ordinary/nested/renamed"}),
        &directory,
        None,
    ).await;
    assert_eq!(
        std::fs::read(workspace.join("ordinary/nested/renamed")).unwrap(),
        b"contents",
        "{created}"
    );
    let removed = command(
        &workspace,
        serde_json::json!({"command": "rm -rf ordinary"}),
        &directory,
        None,
    )
    .await;
    assert!(
        !workspace.join("ordinary").exists(),
        "directory-authorized cleanup failed: {removed}"
    );
    let escaped = command(
        &workspace,
        serde_json::json!({"command": "rm -rf -- '../cleanup-outside/unapproved tree'"}),
        &directory,
        None,
    )
    .await;
    assert_preserved(&sibling, &escaped);
    std::os::unix::fs::symlink(&sibling, workspace.join("outside-link")).unwrap();
    let escaped = command(
        &workspace,
        serde_json::json!({"command": "rm -rf -- outside-link/nested"}),
        &directory,
        None,
    )
    .await;
    assert_preserved(&sibling, &escaped);

    // Full authority also reaches the same native implementation, with no
    // model-specific one-file restriction or path-name exception.
    let first = workspace.join("first");
    let second = workspace.join("second");
    tree(&first);
    tree(&second);
    let full = command(
        &workspace,
        serde_json::json!({
            "command": format!("rm -rf -- '{}' '{}'", first.display(), second.display()),
        }),
        &Caveats::top(),
        None,
    )
    .await;
    assert!(
        !first.exists() && !second.exists(),
        "full authority cleanup failed: {full}"
    );
    assert_preserved(&sibling, &full);
    eprintln!("test native_cleanup_uses_scoped_authority ... ok");
}
