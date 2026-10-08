//! Native agent-bridle #421 proof. Run explicitly with --ignored on macOS.
//! A dispatch-capable main is required for Brush's authenticated worker re-exec.
fn main() {
    if let Some(code) = newt_core::maybe_dispatch() {
        std::process::exit(code);
    }
    #[cfg(target_os = "macos")]
    if std::env::args().any(|arg| arg == "--ignored") {
        run();
    }
}

#[cfg(target_os = "macos")]
fn run() {
    use agent_bridle::{BrushShellTool, Caveats, Gate, Scope, Tool};
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let mut caveats = Caveats {
        exec: Scope::only(["gh".to_owned()]),
        fs_write: Scope::only([root.to_string_lossy().into_owned()]),
        fs_read: Scope::only([root.to_string_lossy().into_owned()]),
        net: Scope::none(),
        ..Caveats::top()
    };
    let path = newt_core::exec_grants::dispatch_path().expect("native gh PATH required");
    assert!(
        newt_core::exec_grants::resolve_basenames(&mut caveats, Some(&path))
            .1
            .is_empty()
    );
    let Scope::Only(grants) = &caveats.exec else {
        panic!("exec must stay confined")
    };
    assert_eq!(grants.len(), 2, "basename plus one exact path: {grants:?}");
    assert!(grants.contains("gh"));
    assert_eq!(caveats.net, Scope::none());
    let tool = BrushShellTool::new();
    let context = Gate::new(0).authorize(&tool, &caveats).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = runtime
        .block_on(tool.invoke(
            serde_json::json!({
                "cmd": "gh --version", "cwd": root,
                "env": {"PATH": path.to_str().unwrap()}
            }),
            &context,
        ))
        .unwrap();
    assert_eq!(output["sandbox_kind"], "seatbelt", "{output}");
    assert_eq!(output["exit_code"], 0, "{output}");
    assert!(
        output["stdout"]
            .as_str()
            .unwrap()
            .starts_with("gh version "),
        "{output}"
    );
    println!("GH_BASENAME_SEATBELT_CONFIRMED: {output}");
}
