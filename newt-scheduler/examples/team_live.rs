//! Live smoke test of the plan → task → crew loop against a local Ollama.
//!
//! Run:  cargo run -p newt-scheduler --example team_live
//! Needs a reachable Ollama at $OLLAMA (default http://localhost:11434) with the
//! models below pulled. Creates a throwaway scratch dir; the crew's edits land
//! there, and `run_test` runs a real python check.

use newt_core::{BackendKind, Tier};
use newt_scheduler::{
    run_team, BackendPool, CrewConfig, Edit, Health, LocalDispatcher, PoolBackend, StaticSource,
    SubtaskStatus, TeamConfig, TeamStatus, Workspace, WorkspaceFactory,
};
use std::path::PathBuf;
use std::process::Command;

/// A real filesystem workspace over a scratch dir; `run_test` shells out.
struct FsWorkspace {
    root: PathBuf,
    test_cmd: String,
}

impl Workspace for FsWorkspace {
    fn files(&self) -> Vec<String> {
        std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()
    }
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(path)).ok()
    }
    fn apply(&mut self, edits: &[Edit]) -> Vec<String> {
        edits
            .iter()
            .filter_map(|e| {
                std::fs::write(self.root.join(&e.path), &e.new_content)
                    .ok()
                    .map(|()| {
                        println!("    · wrote {}", e.path);
                        e.path.clone()
                    })
            })
            .collect()
    }
    fn run_test(&self) -> (bool, String) {
        let out = Command::new("sh")
            .arg("-c")
            .arg(&self.test_cmd)
            .current_dir(&self.root)
            .output()
            .expect("run test");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), combined.trim().to_string())
    }
    fn set_test_command(&mut self, cmd: &str) {
        println!("    · verify ← {cmd}");
        self.test_cmd = cmd.to_string();
    }
}

struct FsFactory {
    root: PathBuf,
    test_cmd: String,
    next: usize,
}

impl WorkspaceFactory for FsFactory {
    type Leaf = FsWorkspace;

    fn create(&mut self, _subtask: &str) -> Result<Self::Leaf, String> {
        self.next += 1;
        let root = self.root.join(format!("leaf-{}", self.next));
        std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        Ok(FsWorkspace {
            root,
            test_cmd: self.test_cmd.clone(),
        })
    }

    fn land(&mut self, workspace: Self::Leaf, _subtask: &str) -> Result<String, String> {
        Ok(workspace.root.display().to_string())
    }

    fn consolidate(&mut self, branches: &[String]) -> Result<String, String> {
        Ok(format!("isolated leaves: {}", branches.join(", ")))
    }
}

#[tokio::main]
async fn main() {
    let endpoint = std::env::var("OLLAMA").unwrap_or_else(|_| "http://localhost:11434".to_string());
    let lead = std::env::var("LEAD").unwrap_or_else(|_| "qwen2.5-coder:14b".to_string());
    let planner = std::env::var("PLANNER").unwrap_or_else(|_| "qwen2.5-coder:7b".to_string());
    let small = std::env::var("SMALL").unwrap_or_else(|_| "qwen2.5-coder:3b".to_string());

    let root = std::env::temp_dir().join(format!("newt-team-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let pool = BackendPool::from_source(&StaticSource {
        backends: vec![PoolBackend::new("local", &endpoint, BackendKind::Ollama)
            .with_models([lead.clone(), planner.clone(), small.clone()])
            .with_health(Health::Up)],
    });
    let cfg = TeamConfig {
        lead_model: lead.clone(),
        lead_tier: Tier::Complex,
        crew: CrewConfig {
            navigator_model: small.clone(),
            planner_model: planner.clone(),
            triage_model: small.clone(),
            max_attempts: 2,
            role_timeout: None,
            calibrate_baseline: false,
        },
        max_subtasks: 2,
    };
    let mut workspaces = FsFactory {
        root: root.clone(),
        // Default (whole-goal) check; the lead is expected to install a tighter
        // per-subtask `verify` for each step (otherwise this full check stands,
        // and an early subtask would block — exactly what per-subtask verify fixes).
        test_cmd: "python3 -c \"from calc import add, mul; assert add(2,3)==5 and \
                   mul(2,3)==6; print('OK')\""
            .to_string(),
        next: 0,
    };

    let goal = "In `calc.py`, implement TWO functions: `add(a, b)` returning a+b, and \
                `mul(a, b)` returning a*b. Each is independently checkable: \
                `python3 -c \"from calc import add; assert add(2,3)==5\"` and \
                `python3 -c \"from calc import mul; assert mul(2,3)==6\"`.";

    println!("== team_live ==");
    println!("  endpoint {endpoint}");
    println!("  lead {lead}  planner {planner}  small {small}");
    println!("  scratch {}", root.display());
    println!("  goal: {goal}\n");

    // Throwaway scratch dir, not a git repo — nothing to grep against.
    let no_grounding = |_: &str| Vec::<String>::new();
    let out = run_team(
        &pool,
        &LocalDispatcher,
        &mut workspaces,
        &cfg,
        &newt_core::caveats::Caveats::top(),
        goal,
        &no_grounding,
    )
    .await;

    println!("\n== plan ({} subtasks) ==", out.plan.len());
    for (i, s) in out.plan.iter().enumerate() {
        println!("  {}. {s}", i + 1);
    }
    println!("\n== results ==");
    for r in &out.results {
        let mark = match r.status {
            SubtaskStatus::Passed => "PASS",
            SubtaskStatus::NeedsHumanReview => "NEEDS-REVIEW",
            SubtaskStatus::Skipped => "skipped",
        };
        println!("  [{mark}] ({}att) {}", r.attempts, r.subtask);
    }
    println!(
        "\n== TEAM: {:?} ==  (consolidated: {:?})",
        out.status, out.consolidated
    );
    std::process::exit(match out.status {
        TeamStatus::AllPassed => 0,
        _ => 2,
    });
}
