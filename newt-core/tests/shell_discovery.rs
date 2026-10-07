//! #2796: real worker/host execution grounds command discovery without a host which.
//! Runs in the normal workspace test job; no model server or installed tools needed.

fn main() {
    if let Some(code) = newt_core::maybe_dispatch() {
        std::process::exit(code);
    }
    #[cfg(unix)]
    tokio::runtime::Runtime::new().unwrap().block_on(run());
}

#[cfg(unix)]
async fn run() {
    use agent_bridle::{Caveats, Gate, Tool};
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let bin = root.join("bin with spaces");
    std::fs::create_dir(&bin).unwrap();
    for name in ["gh", "git"] {
        let path = bin.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut passed = true;
    for tool in [
        Box::new(agent_bridle::BrushShellTool::new()) as Box<dyn Tool>,
        Box::new(agent_bridle::HostShellTool::new()),
    ] {
        let cx = Gate::new(0)
            .authorize(tool.as_ref(), &Caveats::top())
            .unwrap();
        for (cmd, expected, status) in [
            (
                "which gh && which git",
                format!("{}/gh\n{}/git\n", bin.display(), bin.display()),
                0,
            ),
            ("which missing_2796", String::new(), 1),
            (
                "printf ignored | which git",
                format!("{}/git\n", bin.display()),
                0,
            ),
            (
                "PATH='bin with spaces'; which gh",
                "bin with spaces/gh\n".into(),
                0,
            ),
        ] {
            let out = tool
                .invoke(
                    serde_json::json!({
                        "cmd": cmd, "cwd": root, "env": {"PATH": bin}
                    }),
                    &cx,
                )
                .await
                .unwrap();
            if (out["exit_code"].as_i64().unwrap() == 0) != (status == 0)
                || out["stdout"] != expected
            {
                eprintln!("{cmd}: expected {status}/{expected:?}, got {out}");
                passed = false;
            }
        }
    }
    assert!(passed, "#2796 shell discovery regression");

    // Discovering a command must not grant permission to execute it.
    let tool = agent_bridle::BrushShellTool::new();
    let caveats = Caveats {
        exec: agent_bridle::Scope::only(["unrelated-command".to_owned()]),
        ..Caveats::top()
    };
    let cx = Gate::new(0).authorize(&tool, &caveats).unwrap();
    let out = tool
        .invoke(
            serde_json::json!({
                "cmd": "which gh && gh", "cwd": root, "env": {"PATH": bin}
            }),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out["denied"], true, "lookup must not authorize gh: {out}");
    assert_eq!(out["stdout"], format!("{}/gh\n", bin.display()), "{out}");
}
