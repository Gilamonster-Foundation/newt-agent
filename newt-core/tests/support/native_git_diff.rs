//! Ordinary native Git diff must not become an empty external diff command.

use std::path::Path;

use newt_core::{execute_tool, NoMcp, Scope};

pub async fn run(root: &Path) {
    let workspace = root.join("native-git-diff");
    std::fs::create_dir(&workspace).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&workspace)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
    };
    git(&["init", "-q"]);
    std::fs::write(workspace.join(".gitignore"), "__pycache__/\n").unwrap();
    git(&["add", ".gitignore"]);
    std::fs::write(workspace.join(".gitignore"), "__pycache__/\n*.pyc\n").unwrap();
    let mut caveats = newt_core::confined_exec::workspace_confined_caveats(&workspace);
    caveats.exec = Scope::All;
    caveats.net = Scope::All;
    // Exact live source: the later successful commands must not conceal a
    // failed first diff. No build grant or shared temporary write root exists.
    let source = "git diff -- .gitignore; echo \"---STAGED?---\"; git diff --cached --name-only; echo \"---GREP---\"; grep -n 'pyc' .gitignore";
    let output = execute_tool(
        "run_command",
        &serde_json::json!({"command": source}),
        &workspace.to_string_lossy(),
        false,
        100,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(
        output.contains("diff --git a/.gitignore b/.gitignore") && output.contains("+*.pyc"),
        "ordinary diff must report the actual change: {output}"
    );
    assert!(
        output.contains("---STAGED?---") && output.contains("---GREP---"),
        "{output}"
    );
    assert!(
        !output.contains("fatal:") && !output.contains("Operation not permitted"),
        "{output}"
    );
    println!("test native_git_diff_keeps_ordinary_semantics ... ok");
}
