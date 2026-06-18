//! `newt crew` — the human front door to the multi-LLM crew control loop
//! (`newt_scheduler::run_crew`). This module provides the **effects** side: a
//! [`WorktreeWorkspace`] that runs the crew against an *isolated git worktree*
//! (never the live tree — the harness-owns-verification guardrail), plus the
//! test-command inference the front door uses.
//!
//! Design: `docs/design/crew-front-door-and-workflow-tui.md`,
//! `docs/design/config-scaling-deployment-and-trust.md`.

use std::path::{Path, PathBuf};
use std::process::Command;

use newt_scheduler::{Edit, Workspace};

/// Infer the verification command for a repo at `dir`, in priority order:
/// a `justfile` → `just check`; a `Cargo.toml` → `cargo test`; a Python project
/// (`pyproject.toml` / `pytest.ini` / `tox.ini`) → `pytest -x`. Returns `None`
/// when none is found — the front door then **refuses** rather than running a
/// silent no-op "test" (a crew that never verified would be a false success).
pub fn infer_test_command(dir: &Path) -> Option<String> {
    let has = |name: &str| dir.join(name).exists();
    if has("justfile") || has("Justfile") {
        Some("just check".to_string())
    } else if has("Cargo.toml") {
        Some("cargo test".to_string())
    } else if has("pyproject.toml") || has("pytest.ini") || has("tox.ini") {
        Some("pytest -x".to_string())
    } else {
        None
    }
}

/// Run `git <args>` in `dir`, returning trimmed stdout on success.
fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").args(args).current_dir(dir).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A [`Workspace`] backed by an **isolated git worktree** under
/// `<base>/.newt/worktrees/<id>/` (gitignored, inside cwd so writes stay within
/// the fs confinement). Edits land in the worktree, never the live tree;
/// `run_test` shells the verification command there. `Drop` removes the worktree.
pub struct WorktreeWorkspace {
    /// The original repo (where `git worktree` commands run).
    base: PathBuf,
    /// The isolated worktree path.
    worktree: PathBuf,
    /// The verification command (e.g. `just check`).
    test_cmd: String,
}

impl WorktreeWorkspace {
    /// Create a detached worktree at `<base>/.newt/worktrees/<id>` off `HEAD`.
    /// `base` must be a git repo with at least one commit.
    pub fn create(base: &Path, id: &str, test_cmd: String) -> anyhow::Result<Self> {
        let worktree = base.join(".newt").join("worktrees").join(id);
        if let Some(parent) = worktree.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // --detach: a free-floating checkout of HEAD, not a new branch.
        git(
            base,
            &[
                "worktree",
                "add",
                "--detach",
                worktree.to_str().unwrap_or_default(),
                "HEAD",
            ],
        )?;
        Ok(Self {
            base: base.to_path_buf(),
            worktree,
            test_cmd,
        })
    }

    /// The isolated worktree path (e.g. to show touched files relative to it).
    pub fn path(&self) -> &Path {
        &self.worktree
    }

    /// Remove the worktree (best-effort). Called by `Drop`; also callable early.
    pub fn cleanup(&self) {
        let _ = git(
            &self.base,
            &[
                "worktree",
                "remove",
                "--force",
                self.worktree.to_str().unwrap_or_default(),
            ],
        );
    }
}

impl Drop for WorktreeWorkspace {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl Workspace for WorktreeWorkspace {
    fn files(&self) -> Vec<String> {
        // Tracked files in the worktree (gitignore-respecting, no junk).
        git(&self.worktree, &["ls-files"])
            .map(|o| {
                o.lines()
                    .map(str::to_string)
                    .filter(|l| !l.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.worktree.join(path)).ok()
    }

    fn apply(&mut self, edits: &[Edit]) -> Vec<String> {
        let mut written = Vec::new();
        for e in edits {
            let full = self.worktree.join(&e.path);
            if let Some(parent) = full.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(&full, &e.new_content).is_ok() {
                written.push(e.path.clone());
            }
        }
        written
    }

    fn run_test(&self) -> (bool, String) {
        // Shell out via the platform shell so a command string like `just check`
        // or `cargo test` runs as written.
        #[cfg(unix)]
        let mut cmd = {
            let mut c = Command::new("sh");
            c.arg("-c").arg(&self.test_cmd);
            c
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(&self.test_cmd);
            c
        };
        match cmd.current_dir(&self.worktree).output() {
            Ok(o) => {
                let out = format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                (o.status.success(), out)
            }
            Err(e) => (false, format!("failed to run `{}`: {e}", self.test_cmd)),
        }
    }
}

// ---------------------------------------------------------------------------
// `newt crew` command (B2)
// ---------------------------------------------------------------------------

use newt_core::Config;
use newt_scheduler::{
    run_crew, BackendPool, CrewConfig, CrewOutcome, CrewStatus, Dispatcher, LocalDispatcher,
    StaticSource,
};

/// Parsed `newt crew` arguments.
pub struct CrewArgs {
    pub task: String,
    pub crew: Option<String>,
    pub dir: Option<PathBuf>,
    pub test: Option<String>,
    pub max_attempts: Option<u32>,
    pub dry_run: bool,
}

/// Entry point for `newt crew`: resolve config, run the crew with the real
/// (local-HTTP) dispatcher, return the process exit code (0 passed, 2 needs
/// human review, errors bubble up as `Err` → exit 1).
pub async fn run_cli(args: CrewArgs) -> anyhow::Result<i32> {
    let cfg = Config::resolve().map_err(|e| anyhow::anyhow!("config: {e}"))?;
    run_with(&cfg, args, &LocalDispatcher).await
}

/// The testable core: same as [`run_cli`] but with the inference `Dispatcher`
/// and resolved `cfg` injected (tests pass an in-memory config + a mock).
async fn run_with(
    cfg: &Config,
    args: CrewArgs,
    dispatcher: &dyn Dispatcher,
) -> anyhow::Result<i32> {
    let crew_name = resolve_crew_name(cfg, args.crew.as_deref())?;
    let crew = &cfg.crews[&crew_name];
    crew.validate(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;

    let planner_model = model_for_role(cfg, &crew.planner)?;
    let navigator_model = match &crew.navigator {
        Some(n) => model_for_role(cfg, n)?,
        None => planner_model.clone(),
    };
    let triage_model = match &crew.triage {
        Some(t) => model_for_role(cfg, t)?,
        None => planner_model.clone(),
    };

    let dir = args
        .dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let test_cmd = args
        .test
        .clone()
        .or_else(|| crew.test.clone())
        .or_else(|| infer_test_command(&dir))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no verification command — pass --test, set the crew's `test`, or add a \
                 justfile / Cargo.toml / pyproject.toml to {}",
                dir.display()
            )
        })?;
    let max_attempts = args
        .max_attempts
        .or_else(|| crew.budgets.as_ref().and_then(|b| b.max_attempts))
        .unwrap_or(3);

    println!(
        "crew '{crew_name}': planner▸{planner_model}  navigator▸{navigator_model}  \
         triage▸{triage_model}  (max {max_attempts}, test `{test_cmd}`)"
    );
    if args.dry_run {
        return Ok(0);
    }

    // Build the pool from config. We do NOT pre-probe: the dispatcher's own error
    // drives failover (an unreachable backend errors → next candidate → `None`),
    // which keeps the loop honest without a probe round-trip.
    let pool = BackendPool::from_source(&StaticSource::from_configs(cfg.backends.iter()));
    let mut ws = WorktreeWorkspace::create(&dir, &worktree_id(), test_cmd)?;
    let crew_cfg = CrewConfig {
        navigator_model,
        planner_model,
        triage_model,
        max_attempts,
    };
    let outcome = run_crew(&pool, dispatcher, &mut ws, &crew_cfg, &args.task).await;
    // Drop ws (removes the worktree) BEFORE we may process::exit upstream.
    let touched_in = ws.path().to_path_buf();
    drop(ws);
    Ok(render(&outcome, &touched_in))
}

/// Pick the crew: the explicit `--crew`, else the sole crew, else an error.
fn resolve_crew_name(cfg: &Config, explicit: Option<&str>) -> anyhow::Result<String> {
    if let Some(n) = explicit {
        if cfg.crews.contains_key(n) {
            return Ok(n.to_string());
        }
        anyhow::bail!("no crew named '{n}' (known: {})", names(cfg.crews.keys()));
    }
    match cfg.crews.len() {
        1 => Ok(cfg.crews.keys().next().unwrap().clone()),
        0 => anyhow::bail!("no crews defined — add a [crews.<name>] or ~/.newt/crews/<name>.toml"),
        _ => anyhow::bail!(
            "multiple crews defined ({}) — pick one with --crew",
            names(cfg.crews.keys())
        ),
    }
}

/// The model a role-loadout pins: its `model` (with any `@variant` stripped for
/// pool pinning), else its provider backend's default model.
fn model_for_role(cfg: &Config, loadout_name: &str) -> anyhow::Result<String> {
    let l = cfg
        .loadouts
        .get(loadout_name)
        .ok_or_else(|| anyhow::anyhow!("role loadout '{loadout_name}' not found"))?;
    if let Some(m) = &l.model {
        return Ok(m.split('@').next().unwrap_or(m).to_string());
    }
    if let Some(p) = &l.provider {
        if let Some(b) = cfg.backends.iter().find(|b| &b.name == p) {
            return Ok(b.model.clone());
        }
    }
    anyhow::bail!(
        "role loadout '{loadout_name}' has no model (set `model` or a `provider` with a backend)"
    )
}

fn names<'a>(keys: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<&str> = keys.map(String::as_str).collect();
    if v.is_empty() {
        "none".to_string()
    } else {
        v.join(", ")
    }
}

/// A unique worktree id for this run (pid + nanos — no extra deps).
fn worktree_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("crew-{}-{nanos}", std::process::id())
}

/// Print the outcome; return the exit code (0 passed, 2 needs human review).
fn render(o: &CrewOutcome, worktree: &Path) -> i32 {
    match o.status {
        CrewStatus::Passed => {
            let touched = if o.touched.is_empty() {
                "(none)".to_string()
            } else {
                o.touched.join(", ")
            };
            println!(
                "✓ crew passed in {} attempt(s) — touched: {touched}\n  worktree: {}",
                o.attempts,
                worktree.display()
            );
            0
        }
        CrewStatus::NeedsHumanReview => {
            println!(
                "⚠ crew needs human review — {} attempt(s) without a green check",
                o.attempts
            );
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway git repo with one committed file at `name` containing `body`.
    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            git(p, &args).unwrap();
        }
        std::fs::write(p.join("hello.txt"), "world\n").unwrap();
        git(p, &["add", "-A"]).unwrap();
        git(p, &["commit", "-qm", "init"]).unwrap();
        dir
    }

    #[test]
    fn infer_test_command_priority() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(infer_test_command(dir.path()), None, "no markers → None");
        std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(infer_test_command(dir.path()).as_deref(), Some("pytest -x"));
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(
            infer_test_command(dir.path()).as_deref(),
            Some("cargo test")
        );
        std::fs::write(dir.path().join("justfile"), "").unwrap();
        assert_eq!(
            infer_test_command(dir.path()).as_deref(),
            Some("just check")
        );
    }

    #[test]
    fn worktree_isolates_reads_and_writes() {
        let repo = git_repo();
        let mut ws = WorktreeWorkspace::create(repo.path(), "t1", "true".into()).unwrap();

        // files() lists tracked files; read() reads them.
        assert!(ws.files().iter().any(|f| f == "hello.txt"));
        assert_eq!(ws.read("hello.txt").as_deref(), Some("world\n"));
        assert_eq!(ws.read("nope.txt"), None);

        // apply() writes into the WORKTREE, not the live tree.
        let written = ws.apply(&[Edit {
            path: "src/new.rs".into(),
            new_content: "fn main() {}\n".into(),
        }]);
        assert_eq!(written, vec!["src/new.rs".to_string()]);
        assert_eq!(ws.read("src/new.rs").as_deref(), Some("fn main() {}\n"));
        assert!(
            !repo.path().join("src/new.rs").exists(),
            "edit must NOT touch the live tree"
        );
    }

    // run_test shells the platform shell; the assertions below use POSIX
    // commands, so they're Unix-only (the deploy targets are macOS + Linux).
    #[cfg(unix)]
    #[test]
    fn run_test_passes_and_reports_failure() {
        let repo = git_repo();
        let ok = WorktreeWorkspace::create(repo.path(), "t2a", "test -f hello.txt".into()).unwrap();
        assert!(ok.run_test().0, "committed file present → pass");
        let bad = WorktreeWorkspace::create(repo.path(), "t2b", "exit 3".into()).unwrap();
        assert!(!bad.run_test().0, "non-zero exit → fail");
    }

    #[test]
    fn cleanup_removes_the_worktree() {
        let repo = git_repo();
        let path = {
            let ws = WorktreeWorkspace::create(repo.path(), "t3", "true".into()).unwrap();
            let p = ws.path().to_path_buf();
            assert!(p.exists());
            p
            // ws dropped here → cleanup()
        };
        assert!(!path.exists(), "Drop removed the worktree");
    }

    // --- B2: the `newt crew` wiring ---------------------------------------

    /// An in-memory config with three role backends/loadouts and one crew.
    fn crew_cfg() -> Config {
        toml::from_str(
            r#"
            [[backends]]
            name = "p"
            endpoint = "http://p:11434"
            model = "planner-m"
            tiers = []
            [[backends]]
            name = "n"
            endpoint = "http://n:11434"
            model = "nav-m"
            tiers = []
            [[backends]]
            name = "t"
            endpoint = "http://t:11434"
            model = "triage-m"
            tiers = []
            [loadouts.planner]
            provider = "p"
            [loadouts.navigator]
            provider = "n"
            [loadouts.triage]
            provider = "t"
            [crews.coder]
            planner = "planner"
            navigator = "navigator"
            triage = "triage"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn resolve_crew_name_explicit_single_none_multiple() {
        let cfg = crew_cfg();
        assert_eq!(resolve_crew_name(&cfg, Some("coder")).unwrap(), "coder");
        assert_eq!(resolve_crew_name(&cfg, None).unwrap(), "coder"); // sole crew
        assert!(resolve_crew_name(&cfg, Some("ghost"))
            .unwrap_err()
            .to_string()
            .contains("no crew named 'ghost'"));
        let empty = Config::default();
        assert!(resolve_crew_name(&empty, None)
            .unwrap_err()
            .to_string()
            .contains("no crews defined"));
    }

    #[test]
    fn model_for_role_from_provider_backend_and_missing() {
        let cfg = crew_cfg();
        assert_eq!(model_for_role(&cfg, "planner").unwrap(), "planner-m");
        assert!(model_for_role(&cfg, "ghost").is_err());
    }

    /// Role-aware mock: returns the canned JSON each role's prompt expects,
    /// keyed by the pinned model. The planner emits an edit that creates the
    /// file the verification command checks for, so the crew converges.
    struct RoleMock;
    #[async_trait::async_trait]
    impl Dispatcher for RoleMock {
        async fn dispatch(
            &self,
            _backend: &newt_scheduler::PoolBackend,
            model: &str,
            _req: newt_scheduler::ChatRequest,
        ) -> anyhow::Result<newt_scheduler::ChatReply> {
            let content = match model {
                "nav-m" => r#"{"relevant_files": ["marker.txt"]}"#,
                "planner-m" => r#"{"edits": [{"path": "FIXED.txt", "new_content": "ok\n"}]}"#,
                "triage-m" => r#"{"summary": "missing file", "next_action": "create it"}"#,
                _ => "{}",
            };
            Ok(newt_scheduler::ChatReply {
                content: content.to_string(),
                model_id: model.to_string(),
                usage: None,
            })
        }
    }

    #[cfg(unix)] // the verification command is a POSIX `test -f`
    #[tokio::test]
    async fn crew_converges_with_a_fixing_planner() {
        let repo = git_repo();
        let cfg = crew_cfg();
        let args = CrewArgs {
            task: "make the check pass".into(),
            crew: Some("coder".into()),
            dir: Some(repo.path().to_path_buf()),
            // fails until the planner creates FIXED.txt
            test: Some("test -f FIXED.txt".into()),
            max_attempts: Some(2),
            dry_run: false,
        };
        let code = run_with(&cfg, args, &RoleMock).await.unwrap();
        assert_eq!(code, 0, "planner's edit creates FIXED.txt → verify passes");
    }

    #[tokio::test]
    async fn crew_dry_run_resolves_without_touching_the_repo() {
        let repo = git_repo();
        let cfg = crew_cfg();
        let args = CrewArgs {
            task: "noop".into(),
            crew: Some("coder".into()),
            dir: Some(repo.path().to_path_buf()),
            test: Some("true".into()),
            max_attempts: None,
            dry_run: true,
        };
        // dry-run never builds a worktree or dispatches.
        let code = run_with(&cfg, args, &RoleMock).await.unwrap();
        assert_eq!(code, 0);
        assert!(!repo.path().join(".newt/worktrees").exists());
    }
}
