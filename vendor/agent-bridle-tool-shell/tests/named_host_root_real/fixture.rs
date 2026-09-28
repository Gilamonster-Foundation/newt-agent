//! Native executable copies and child actions shared by shell acceptance tests.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_bridle_core::{Caveats, ChildNetworkPolicy, SandboxPolicy, Scope};
use agent_bridle_tool_shell::BrushShellTool;

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

pub(super) fn probe() -> i32 {
    let args: Vec<String> = std::env::args().skip(2).collect();
    match args.first().map(String::as_str) {
        Some("literal") => println!("LITERAL:{}", args.get(1).map_or("", String::as_str)),
        Some("environment") => {
            println!("ENV:{}", std::env::var("ROOT_TOKEN").expect("explicit env"));
            println!("PATH:{}", std::env::var("PATH").expect("explicit path"));
            eprintln!("ROOT_STDERR");
        }
        Some("observed-output") => return super::observation::output_probe(),
        Some("forward") => {
            return Command::new(std::env::var_os("ROOT_CHILD").expect("child path"))
                .args(["--host-root-probe", args.get(1).expect("child action")])
                .status()
                .map_or_else(
                    |error| {
                        eprintln!("CHILD_SPAWN_ERROR:{error}");
                        42
                    },
                    |status| status.code().unwrap_or(44),
                );
        }
        Some("inside") => {
            assert_eq!(std::fs::read("inside-source").unwrap(), b"inside-value");
            std::fs::write("inside-result", b"inside-write").unwrap();
            println!("INSIDE_READ_WRITE_OK");
        }
        Some("outside") => {
            match std::fs::read(std::env::var_os("ROOT_OUTSIDE").expect("outside path")) {
                Ok(bytes) => println!("OUTSIDE:{}", String::from_utf8_lossy(&bytes)),
                Err(error) => {
                    eprintln!("OUTSIDE_READ_ERROR:{:?}", error.kind());
                    return 43;
                }
            }
        }
        Some("delayed-marker") => {
            std::thread::sleep(Duration::from_millis(800));
            std::fs::write("late-marker", b"survived").unwrap();
        }
        Some("hold-pipe") => {
            std::thread::sleep(Duration::from_secs(3));
            std::fs::write("late-marker", b"survived").unwrap();
        }
        Some("orphan-pipe") => {
            #[expect(
                clippy::zombie_processes,
                reason = "this regression requires a dead leader with a live pipe-holding descendant; the managed tree supervisor must terminate it"
            )]
            let _child = Command::new(std::env::var_os("ROOT_CHILD").expect("child path"))
                .args(["--host-root-probe", "hold-pipe"])
                .spawn()
                .expect("grandchild starts and holds inherited stdout");
            std::fs::write("exiting-root-pid", std::process::id().to_string()).unwrap();
            // The core reaper now owns the descendant and both pipe writers.
        }
        Some("wait-child") => {
            let mut child = Command::new(std::env::var_os("ROOT_CHILD").expect("child path"))
                .args(["--host-root-probe", "delayed-marker"])
                .spawn()
                .expect("child starts under named root");
            std::fs::write("root-started", b"started").unwrap();
            return child.wait().unwrap().code().unwrap_or(44);
        }
        _ => panic!("unknown probe action"),
    }
    0
}

pub(super) struct Fixture {
    directory: PathBuf,
    pub(super) workspace: PathBuf,
    pub(super) root: PathBuf,
    pub(super) child: PathBuf,
    pub(super) outside: PathBuf,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "bridle-named-shell-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&directory).unwrap();
        let directory = directory.canonicalize().unwrap();
        let workspace = directory.join("workspace");
        let tools = directory.join("tools");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&tools).unwrap();
        let root = tools.join(format!("root-image{}", std::env::consts::EXE_SUFFIX));
        let child = tools.join(format!("child-image{}", std::env::consts::EXE_SUFFIX));
        for executable in [&root, &child] {
            std::fs::copy(std::env::current_exe().unwrap(), executable).unwrap();
        }
        let outside = directory.join("outside-sentinel");
        std::fs::write(&outside, b"known-readable-outside-value").unwrap();
        std::fs::write(workspace.join("inside-source"), b"inside-value").unwrap();
        Self {
            directory,
            workspace,
            root,
            child,
            outside,
        }
    }

    pub(super) fn protected_roots(&self) -> BTreeSet<String> {
        [self.outside.display().to_string()].into()
    }

    pub(super) fn tool(&self, enabled: bool) -> BrushShellTool {
        let tool = BrushShellTool::new().with_sandbox_policy(Arc::new(SandboxPolicy {
            child_network: ChildNetworkPolicy::DenyDirect,
            named_root_protected_roots: Some(self.protected_roots()),
            ..SandboxPolicy::default()
        }));
        if enabled {
            tool.with_named_host_roots()
        } else {
            tool
        }
    }

    pub(super) fn caveats(&self) -> Caveats {
        Caveats {
            exec: Scope::only([self.root.display().to_string()]),
            fs_read: Scope::only([
                self.workspace.display().to_string(),
                self.root.parent().unwrap().display().to_string(),
            ]),
            fs_write: Scope::only([self.workspace.display().to_string()]),
            net: Scope::none(),
            ..Caveats::top()
        }
    }

    pub(super) fn arguments(&self, action: &str) -> serde_json::Value {
        serde_json::json!({
            "cmd": format!("'{}' --host-root-probe {action}", self.root.display()),
            "cwd": self.workspace,
            "env": {
                "ROOT_TOKEN": "explicit-value",
                "PATH": "explicit-path",
                "ROOT_CHILD": self.child,
                "ROOT_OUTSIDE": self.outside,
            },
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
