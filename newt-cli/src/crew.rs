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

/// Is `path` a safe in-worktree edit target — built only from `Normal` (and `.`)
/// components, so no root/drive prefix and no `..` escape? `Path::join` discards
/// the base for an absolute path, so this guard (not the `fs_write` caveat, which
/// is `Scope::All` on the crew/plan path) is the real worktree boundary.
///
/// Checked by components, NOT `is_absolute()`: on Windows `/etc/passwd` is
/// root-relative and `is_absolute()` is `false`, so an `is_absolute` guard would
/// wrongly admit it. A `RootDir` / drive `Prefix` / `ParentDir` component is
/// refused on every platform.
fn is_safe_worktree_path(path: &str) -> bool {
    use std::path::Component;
    Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
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
    /// Create a detached worktree at `<base>/.newt/worktrees/<id>` off
    /// `base_ref` (any git commit-ish — a sha, branch, or `HEAD`). `base` must be
    /// a git repo with at least one commit.
    pub fn create(base: &Path, id: &str, base_ref: &str, test_cmd: String) -> anyhow::Result<Self> {
        let worktree = base.join(".newt").join("worktrees").join(id);
        if let Some(parent) = worktree.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // --detach: a free-floating checkout of `base_ref` (the cumulative chain
        // tip), not a new branch.
        git(
            base,
            &[
                "worktree",
                "add",
                "--detach",
                worktree.to_str().unwrap_or_default(),
                base_ref,
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

    /// The crew's changes as a unified diff (all edits incl. new files), for the
    /// overseer to review. Stages everything in the throwaway worktree, then diffs
    /// the index against HEAD. Empty string if nothing changed or git errs.
    pub fn diff(&self) -> String {
        let _ = git(&self.worktree, &["add", "-A"]);
        git(&self.worktree, &["diff", "--cached", "HEAD"]).unwrap_or_default()
    }

    /// **Land** the crew's work as a commit on a new `branch` in the SHARED object
    /// store, so verified work persists (a reviewable, mergeable branch the base
    /// repo sees) instead of being thrown away with the worktree. The worktree is a
    /// linked `git worktree`, so the commit + branch ref live in the common `.git`
    /// and survive `cleanup()`. Returns `(branch, short_sha)`; errs if nothing
    /// changed. (Provenance: a content-addressed commit, authored by the agent
    /// identity — the seam where agent-mesh signing later attests the crew member.)
    pub fn commit_to_branch(
        &self,
        branch: &str,
        author_name: &str,
        author_email: &str,
        message: &str,
    ) -> anyhow::Result<(String, String)> {
        git(&self.worktree, &["checkout", "-q", "-b", branch])?;
        git(&self.worktree, &["add", "-A"])?;
        // `diff --cached --quiet` exits 0 when the index matches HEAD (nothing to
        // land) → don't manufacture an empty commit.
        if git(&self.worktree, &["diff", "--cached", "--quiet"]).is_ok() {
            anyhow::bail!("no changes to land");
        }
        git(
            &self.worktree,
            &[
                "-c",
                &format!("user.name={author_name}"),
                "-c",
                &format!("user.email={author_email}"),
                "commit",
                "-q",
                "-m",
                message,
            ],
        )?;
        let sha = git(&self.worktree, &["rev-parse", "--short", "HEAD"])?;
        Ok((branch.to_string(), sha))
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
        // Confine reads to the worktree (#521): refuse absolute / `..`-escaping
        // paths — the same structural boundary as `apply` (#637). `Path::join`
        // discards the base for an absolute path, so without this guard
        // `read("/etc/passwd")` would escape the worktree and read the host.
        if !is_safe_worktree_path(path) {
            return None;
        }
        std::fs::read_to_string(self.worktree.join(path)).ok()
    }

    fn apply(&mut self, edits: &[Edit]) -> Vec<String> {
        let mut written = Vec::new();
        for e in edits {
            // STRUCTURAL worktree boundary: refuse an absolute or `..`-escaping
            // edit path. `Path::join` silently discards the base for an absolute
            // path (`worktree.join("/etc/passwd") == "/etc/passwd"`), so the
            // fs_write caveat — often `Scope::All` on the crew/plan path — is NOT
            // the boundary; this guard is. Closes the escape for `newt crew` and
            // `newt plan` alike.
            if !is_safe_worktree_path(&e.path) {
                continue;
            }
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

/// Args for `newt plan <file>` — preview, or `--execute` an overseer-authored plan.
pub struct PlanArgs {
    pub file: PathBuf,
    pub dir: Option<PathBuf>,
    /// Actually dispatch the crews. The default is preview-only, so autonomous
    /// multi-crew execution always needs this second, explicit affirmation.
    pub execute: bool,
    /// `--one-shot`: this run carries the human's tacit approval, so grant each
    /// leaf the fs authority it needs (`grant_one_shot_authority`). A plain
    /// `--execute` of a reviewed file does NOT — the human granted caveats by
    /// editing the TOML.
    pub one_shot: bool,
    /// Refuse to execute a plan with more leaves than this without an explicit
    /// raise — each leaf is an autonomous crew with no per-leaf human review.
    pub max_leaves: usize,
}

/// A one-screen preview of a plan: goal, subtask count, and the **leaves** (the
/// dispatch units) with their deps. A dep that names a non-leaf (a branch, never
/// dispatched) or an absent id is flagged `[will stall]` — it can never reach
/// `Done`, so a leaf waiting on it would never run. Pure (no fs, no dispatch) so
/// the preview and the unit tests share it.
pub fn render_plan_preview(plan: &newt_core::plan::Plan) -> String {
    use std::collections::HashSet;
    let leaves = plan.leaves();
    let leaf_ids: HashSet<&str> = leaves.iter().map(|s| s.id.as_str()).collect();
    let mut out = String::new();
    if let Some(g) = &plan.goal {
        out.push_str(&format!("goal: {g}\n"));
    }
    out.push_str(&format!(
        "{} subtask(s); {} leaf/leaves to dispatch:\n",
        plan.subtasks.len(),
        leaves.len()
    ));
    for leaf in &leaves {
        let after = if leaf.deps.is_empty() {
            String::new()
        } else {
            let deps: Vec<String> = leaf
                .deps
                .iter()
                .map(|d| {
                    if leaf_ids.contains(d.as_str()) {
                        d.clone()
                    } else {
                        // not a leaf (a branch, or an absent id) → never Done.
                        format!("{d} [will stall]")
                    }
                })
                .collect();
            format!("  (after {})", deps.join(", "))
        };
        out.push_str(&format!("  • {} — {}{after}\n", leaf.id, leaf.instruction));
    }
    out
}

/// Pre-execution structural sanity (B3): the problems that would doom an
/// autonomous run — a dep naming a non-existent subtask (stalls forever), an
/// empty-instruction leaf, or a plan with no dispatchable leaves (nothing runs).
/// Empty result = structurally runnable. Pure (no fs / dispatch).
fn plan_sanity(plan: &newt_core::plan::Plan) -> Vec<String> {
    use std::collections::HashSet;
    let ids: HashSet<&str> = plan.subtasks.iter().map(|s| s.id.as_str()).collect();
    let mut problems = Vec::new();
    for s in &plan.subtasks {
        for d in &s.deps {
            if !ids.contains(d.as_str()) {
                problems.push(format!(
                    "subtask `{}` depends on `{}`, which no subtask defines — it can never run",
                    s.id, d
                ));
            }
        }
        if s.instruction.trim().is_empty() {
            problems.push(format!("subtask `{}` has an empty instruction", s.id));
        }
    }
    if plan.leaves().is_empty() && !plan.subtasks.is_empty() {
        problems.push(
            "the plan has no dispatchable leaves (every subtask is a parent) — nothing would run"
                .to_string(),
        );
    }
    problems
}

/// `newt plan <file>` — PREVIEW an overseer-authored plan, or (`--execute`)
/// dispatch it leaf-by-leaf via a crew (each leaf in its own worktree, through the
/// same `LocalCrewRunner` the in-session `crew` tool uses).
///
/// Preview is the **default** because `--execute` runs an autonomous DAG of crews
/// with no per-leaf human review (the per-leaf `verify` is the only gate); it is
/// bounded by `--max-leaves`. The run-log (statuses + results) is written to a
/// sibling `<file>.run.toml` — the authored source file is **never modified**.
/// Exit 0 = preview / complete, 1 = incomplete (failed or stalled).
pub async fn run_plan_cli(args: PlanArgs) -> anyhow::Result<i32> {
    let toml = std::fs::read_to_string(&args.file)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", args.file.display()))?;
    let mut plan = newt_core::plan::Plan::from_toml_str(&toml)
        .map_err(|e| anyhow::anyhow!("parse plan {}: {e}", args.file.display()))?;
    println!("plan: {}", args.file.display());
    print!("{}", render_plan_preview(&plan));
    if !args.execute {
        println!("\n(preview only — re-run with --execute to dispatch one crew per leaf)");
        return Ok(0);
    }
    // `--one-shot` on a FILE is tacit approval to grant the leaves their fs
    // authority; a plain `--execute` respects the caveats the human reviewed in.
    if args.one_shot {
        grant_one_shot_authority(&mut plan);
    }
    // Run-log lands in a SIBLING artifact — never modify the authored source.
    let log_path = args.file.with_extension("run.toml");
    execute_plan(&mut plan, args.dir, args.max_leaves, Some(log_path), None).await
}

/// `--one-shot`'s tacit approval, made real: grant every subtask the filesystem
/// authority an autonomous run needs — `fs_read` + `fs_write` (worktree-bounded
/// by the runner's apply-path guard). `exec` + `net` stay DENIED: verification
/// runs via the runner's TRUSTED inferred command (`just check` / `cargo test`),
/// not the model-authored `verify` shell, and the crew needs no network. Without
/// this, every authored leaf is default-deny and can edit nothing.
fn grant_one_shot_authority(plan: &mut newt_core::plan::Plan) {
    use newt_core::role_profile::ScopeSpec;
    for s in &mut plan.subtasks {
        s.caveat_policy.fs_read = ScopeSpec::default(); // "all"
        s.caveat_policy.fs_write = ScopeSpec::default(); // "all"
    }
    println!(
        "--one-shot: granted each leaf fs_read + fs_write (worktree-bounded); \
         exec + net stay denied (verify runs via the runner's trusted command)."
    );
}

/// Re-ground a failed leaf from its build error (#692): parse the unresolved
/// symbol rustc names, grep its DEFINITION (#687's pattern), and — if the
/// instruction doesn't already target that file — append a grounding correction
/// so the retried leaf edits the real seam.
struct DefGroundReground {
    dir: std::path::PathBuf,
}
impl newt_core::agentic::Reground for DefGroundReground {
    fn reground(&self, error: &str, instruction: &str) -> Option<String> {
        let sym = unresolved_symbol(error)?;
        let sites = git(
            &self.dir,
            &[
                "grep",
                "-nE",
                "-e",
                &definition_grep_pattern(&sym),
                "--",
                "*.rs",
            ],
        )
        .ok()?;
        let file = sites.lines().next()?.split(':').next()?.to_string();
        if file.is_empty() || instruction.contains(&file) {
            return None;
        }
        Some(format!(
            "{instruction}\n\nGROUNDING: `{sym}` is defined at {file} — make the edit there, do not invent paths."
        ))
    }
}

/// The unresolved symbol named in a rustc error, if any (#692) — `cannot find
/// function/value/type` `X`, `no method named` `X`, etc.
fn unresolved_symbol(error: &str) -> Option<String> {
    const MARKERS: &[&str] = &[
        "cannot find function `",
        "cannot find value `",
        "cannot find type `",
        "cannot find macro `",
        "cannot find struct, variant or union type `",
        "no method named `",
        "no function or associated item named `",
        "use of undeclared `",
    ];
    for m in MARKERS {
        if let Some(i) = error.find(m) {
            let rest = &error[i + m.len()..];
            if let Some(j) = rest.find('`') {
                let sym = &rest[..j];
                if !sym.is_empty() {
                    return Some(sym.to_string());
                }
            }
        }
    }
    None
}

/// Execute a parsed/authored plan autonomously: enforce the `--max-leaves`
/// autonomy bound, then drive `run_plan` via a `LocalCrewRunner`
/// (`Presence::Prompt` — the human's `--execute`/`--one-shot` gesture). Prints
/// per-leaf progress; writes a run-log to `log_path` when work actually ran.
/// Returns 0 = plan complete, 1 = incomplete. The source is never modified.
pub async fn execute_plan(
    plan: &mut newt_core::plan::Plan,
    dir: Option<PathBuf>,
    max_leaves: usize,
    log_path: Option<PathBuf>,
    locked_verify: Option<String>,
) -> anyhow::Result<i32> {
    // B3: refuse a structurally-doomed plan BEFORE spending crew runs on it — a
    // dep on a non-existent subtask, an empty-instruction leaf, or no
    // dispatchable leaves all guarantee a stall / wasted dispatch.
    let problems = plan_sanity(plan);
    if !problems.is_empty() {
        eprintln!("✗ plan is not structurally runnable — refusing to dispatch:");
        for p in &problems {
            eprintln!("  - {p}");
        }
        return Ok(2);
    }
    // Autonomy bound: refuse an oversized autonomous fan-out unless the human
    // explicitly raises the cap (each leaf runs with no per-leaf review).
    let leaf_count = plan.leaves().len();
    if leaf_count > max_leaves {
        return Err(anyhow::anyhow!(
            "plan has {leaf_count} leaves (> --max-leaves {max_leaves}); each is an autonomous \
             crew with no per-leaf review. Re-run with `--max-leaves {leaf_count}` to confirm you \
             intend to run them all."
        ));
    }
    let cfg = Config::resolve().map_err(|e| anyhow::anyhow!("config: {e}"))?;
    let dir = dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    println!(
        "executing {leaf_count} leaf/leaves autonomously (one crew each; per-leaf verify gates each)…"
    );
    // Honest, non-top session caveats (mirrors `newt crew`): fs_write=All — the
    // real boundary is the per-leaf worktree + the apply-path guard
    // (`is_safe_worktree_path`), NOT this caveat — exec/net locked, so a
    // plan-authored `verify` needing exec is dropped fail-closed (#634). The
    // `--execute`/`--one-shot` gesture is modelled as `Presence::Prompt` (the
    // BOOT attest ceremony is #472).
    let caveats = newt_acp_worker::worker_session_caveats(None);
    let reground = DefGroundReground { dir: dir.clone() };
    let runner =
        crate::crew_runner::LocalCrewRunner::new(cfg, dir, newt_core::agentic::Presence::Prompt)
            .with_locked_verify(locked_verify);
    let run = newt_core::agentic::run_plan_with_reground(plan, &caveats, &runner, &reground).await;
    for id in &run.dispatched {
        if let Some(s) = plan.subtask(id) {
            println!("  [{:?}] {}", s.status, id);
        }
    }
    if let Some(e) = &run.failed {
        println!("✗ stopped at a failed leaf: {e}");
    }
    if !run.remaining.is_empty() {
        println!("remaining (blocked/stalled): {}", run.remaining.join(", "));
    }
    println!(
        "{}",
        if run.complete {
            "✓ plan complete"
        } else {
            "plan incomplete"
        }
    );
    // Write the run-log only when work actually ran.
    if !run.dispatched.is_empty() {
        if let Some(log) = log_path {
            std::fs::write(&log, plan.to_toml_string()?)
                .map_err(|e| anyhow::anyhow!("write run-log {}: {e}", log.display()))?;
            println!("run-log → {}", log.display());
        }
    }
    Ok(i32::from(!run.complete))
}

// ── Plan authoring (the overseer's decompose step) ────────────────────────────

/// The overseer's decompose prompt: turn a goal into a dependency-ordered
/// `plan::Plan`. It asks for the WORK only — never authority — so an authored
/// plan carries default-deny caveats the human grants by editing the TOML.
const PLAN_AUTHOR_SYSTEM: &str = "You are a planning lead. Decompose the GOAL into a \
    dependency-ordered plan of the FEWEST engineering subtasks that accomplish it — each \
    subtask MUST change code (produce a file edit). Reply with ONLY JSON: \
    {\"goal\":\"<the goal>\",\"subtasks\":[{\"id\":\"<short-kebab-id>\",\
    \"instruction\":\"<imperative code-changing step>\",\
    \"deps\":[\"<id of a step that must finish first>\"],\
    \"verify\":\"<shell command that exits 0 once THIS step is done; omit if none>\"}]}. \
    A small, single-file change is ONE subtask. Do NOT create separate \
    inspect/understand/explore/locate/verify/test/run-tests subtasks — the harness reads \
    the repo for you and automatically verifies EVERY subtask after it runs (put a step's \
    own check in its `verify` FIELD, never as a standalone subtask). Use `deps` for \
    ordering (a step lists the ids it waits on). Ids: short, stable, unique. Do NOT grant \
    permissions or describe authority — only the work.";

/// The first balanced `{…}` span in `s` (string-aware), so a model reply wrapped
/// in ```json fences or prose still parses. `None` if there is no balanced object.
fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_str {
            match c {
                b'\\' if !esc => esc = true,
                b'"' if !esc => in_str = false,
                _ => esc = false,
            }
        } else if c == b'"' {
            in_str = true;
        } else if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(&s[start..=i]);
            }
        }
    }
    None
}

/// Parse a model-authored plan (tolerating fences/prose) into a `plan::Plan`.
/// Every subtask gets **default-deny** caveats — the model proposes the WORK, never
/// the authority; the human grants it by editing the TOML. `None` when no parseable
/// `{…}` object with a non-empty `subtasks` list (each with `id` + `instruction`)
/// is found. Parsed with `serde_json::Value` (newt-cli has no `serde` derive dep).
fn parse_authored_plan(raw: &str) -> Option<newt_core::plan::Plan> {
    use newt_core::plan::{Aggregation, CaveatPolicy, Plan, Subtask, SubtaskStatus};
    use std::collections::HashSet;
    let v: serde_json::Value = serde_json::from_str(extract_json_object(raw)?).ok()?;
    let arr = v.get("subtasks")?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut seen = HashSet::new();
    let mut subtasks = Vec::with_capacity(arr.len());
    for s in arr {
        // Reject empty / whitespace-only ids+instructions, and DUPLICATE ids.
        // ids must be unique: `Plan::mark` updates the first match while
        // `next_ready_leaf` finds the next unmarked one, so duplicate ids would
        // desync the execute-time cursor into an unbounded re-dispatch loop.
        let id = s.get("id")?.as_str()?.trim().to_string();
        let instruction = s.get("instruction")?.as_str()?.trim().to_string();
        if id.is_empty() || instruction.is_empty() || !seen.insert(id.clone()) {
            return None;
        }
        let deps = s
            .get("deps")
            .and_then(|d| d.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        subtasks.push(Subtask {
            id,
            instruction,
            deps,
            parallel_ok: false,
            context: Vec::new(),
            verify: s.get("verify").and_then(|x| x.as_str()).map(str::to_string),
            status: SubtaskStatus::Pending,
            result: None,
            parent: None,
            caveat_policy: CaveatPolicy::default(), // default-DENY: model proposes work, not authority
        });
    }
    Some(Plan {
        goal: v.get("goal").and_then(|x| x.as_str()).map(str::to_string),
        aggregation: Aggregation::default(),
        subtasks,
    })
}

/// Decompose `goal` into a `plan::Plan` by asking `model` (the overseer seat).
/// The model proposes the work; caveats stay default-deny.
pub async fn author_plan(
    pool: &newt_scheduler::BackendPool,
    dispatcher: &dyn Dispatcher,
    model: &str,
    goal: &str,
    max_subtasks: usize,
) -> anyhow::Result<newt_core::plan::Plan> {
    let req = newt_scheduler::ChatRequest::new()
        .system(PLAN_AUTHOR_SYSTEM)
        .user(format!("GOAL:\n{goal}\n\nAt most {max_subtasks} subtasks."));
    let reply = pool
        .run_role(dispatcher, newt_core::Tier::Complex, model, req)
        .await
        .ok_or_else(|| {
            anyhow::anyhow!("no live model reachable to author the plan (model {model})")
        })?;
    parse_authored_plan(&reply.result.content).ok_or_else(|| {
        anyhow::anyhow!(
            "the model did not return a parseable JSON plan. Raw reply:\n{}",
            reply.result.content
        )
    })
}

/// `newt plan --goal "<g>"` — author a plan from a goal, write it (or print it),
/// and show the preview. Does NOT execute: the human reviews/edits, then runs
/// `newt plan <file> --execute`.
/// Author a plan from a goal (the overseer's decompose step) and RETURN it.
/// Shared by [`author_plan_cli`] (which writes/prints it) and
/// [`one_shot_goal_cli`] (which executes it).
pub async fn author_plan_to_plan(
    goal: &str,
    max_subtasks: usize,
    repo_dir: &Path,
) -> anyhow::Result<newt_core::plan::Plan> {
    let cfg = Config::resolve().map_err(|e| anyhow::anyhow!("config: {e}"))?;
    let model = cfg
        .backends
        .first()
        .map(|b| b.model.clone())
        .ok_or_else(|| {
            anyhow::anyhow!("no backend configured to author a plan (add a [[backends]])")
        })?;
    let pool = BackendPool::from_source(&StaticSource::from_configs(cfg.backends.iter()));
    let mut effective_goal = goal.to_string();
    // Phase 1a (#646): READ any GitHub issue/PR the goal references, via `gh` (a
    // harness-available tool), so the planner decomposes the ACTUAL document
    // instead of a bare link it cannot fetch. Best-effort — a missing /
    // unauthenticated `gh` just leaves the goal text alone.
    let docs = fetch_referenced_docs(goal);
    if !docs.is_empty() {
        println!(
            "read referenced GitHub doc(s) via gh ({} chars)",
            docs.len()
        );
        effective_goal.push_str(&format!(
            "\n\nThe referenced document(s) below are the TASK to implement — \
             decompose THESE into concrete engineering subtasks:{docs}"
        ));
    }
    // Phase 1b: READ the TARGET REPO's structure (language, build, layout) so the
    // planner authors subtasks that fit THIS codebase — e.g. Rust crates, not a
    // hallucinated Python stack. Best-effort: a non-repo dir adds nothing.
    let repo = fetch_repo_context(repo_dir);
    if !repo.is_empty() {
        println!("read target-repo context ({} chars)", repo.len());
        effective_goal.push_str(&repo);
    }
    // Phase 1c (B2): GREP the repo for the commands/symbols the TASK references
    // (slash-commands, backtick-quoted identifiers), so the planner targets REAL
    // files instead of guessing paths. Greps the goal+issue text, not the repo
    // layout. Best-effort: a non-repo dir / no matches adds nothing.
    let hits = fetch_code_grep_hits(&format!("{goal}{docs}"), repo_dir);
    if !hits.is_empty() {
        println!(
            "found relevant code location(s) via grep ({} chars)",
            hits.len()
        );
        effective_goal.push_str(&hits);
    }
    // Phase 2: decompose the (doc + repo-augmented) goal into a plan.
    println!("authoring a plan for: {goal}  (model {model})…");
    author_plan(
        &pool,
        &LocalDispatcher,
        &model,
        &effective_goal,
        max_subtasks,
    )
    .await
}

/// Bounded structural context about the target repo at `dir` — its language /
/// build system and top-level layout — so the planner authors subtasks that fit
/// THIS codebase (e.g. Rust crates `newt-cli`/`newt-core`, not a hallucinated
/// FastAPI/Django stack). Best-effort: a non-repo / unreadable `dir` yields "".
fn fetch_repo_context(dir: &Path) -> String {
    let mut facts: Vec<String> = Vec::new();
    if dir.join("Cargo.toml").exists() {
        facts.push("Language/build: Rust (a cargo workspace).".into());
    } else if dir.join("package.json").exists() {
        facts.push("Language/build: JavaScript/TypeScript (npm).".into());
    } else if dir.join("pyproject.toml").exists() || dir.join("setup.py").exists() {
        facts.push("Language/build: Python.".into());
    } else if dir.join("go.mod").exists() {
        facts.push("Language/build: Go.".into());
    }
    if let Some(cmd) = infer_test_command(dir) {
        facts.push(format!("Verify/build command: `{cmd}`."));
    }
    // Top-level tracked entries (the crate / dir layout), bounded.
    if let Ok(top) = git(dir, &["ls-tree", "--name-only", "HEAD"]) {
        let entries: Vec<&str> = top.lines().filter(|l| !l.is_empty()).take(40).collect();
        if !entries.is_empty() {
            facts.push(format!("Top-level entries: {}.", entries.join(", ")));
        }
    }
    if facts.is_empty() {
        return String::new();
    }
    format!(
        "\n\nTarget repository context — author subtasks that fit THIS codebase \
         (use its real language, build command, and directories; do NOT invent a \
         different stack):\n- {}",
        facts.join("\n- ")
    )
}

/// GitHub issue/PR references in `goal`: `(owner, repo, kind, number)` for each
/// `github.com/<owner>/<repo>/(issues|pull)/<n>` URL. Tolerant of trailing
/// punctuation on the number and of the URL sitting in surrounding prose.
fn github_refs(goal: &str) -> Vec<(String, String, String, String)> {
    let mut refs = Vec::new();
    for token in goal.split_whitespace() {
        let Some(idx) = token.find("github.com/") else {
            continue;
        };
        let parts: Vec<&str> = token[idx + "github.com/".len()..].split('/').collect();
        if parts.len() < 4 {
            continue;
        }
        let num: String = parts[3]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !num.is_empty() && (parts[2] == "issues" || parts[2] == "pull") {
            refs.push((
                parts[0].to_string(),
                parts[1].to_string(),
                parts[2].to_string(),
                num,
            ));
        }
    }
    refs
}

/// High-signal code-ish terms in `text` to grep for: slash-commands (`/dgx`)
/// and backtick-quoted single tokens (`help_lines`). Distinctive enough to
/// locate real code without the noise a bare word like "help" would drown it in.
fn grep_terms(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    // Backtick-quoted single tokens.
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('`') else { break };
        let span = rest[..close].trim();
        if (3..=40).contains(&span.chars().count())
            && span.split_whitespace().count() == 1
            && span.chars().any(|c| c.is_ascii_alphanumeric())
        {
            terms.push(span.to_string());
        }
        rest = &rest[close + 1..];
    }
    // Slash-commands: `/word`.
    for tok in text.split(|c: char| c.is_whitespace() || "()[]{}<>,;:\"'".contains(c)) {
        if let Some(after) = tok.strip_prefix('/') {
            let cmd: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if cmd.len() >= 2 {
                terms.push(format!("/{cmd}"));
            }
        }
    }
    terms.sort();
    terms.dedup();
    terms.truncate(12);
    terms
}

/// One grep block for a single task term: the DEFINITION sites (the real seam)
/// and the other mention sites, kept apart so [`format_grounding_hits`] can rank
/// definitions FIRST and never truncate them. The #687 bug was `git grep |
/// take(3)`: because `git grep` is path-alphabetical, an earlier-sorting
/// same-named decoy (e.g. `newt-cli/crew.rs` mentions of `help_lines`) buried
/// the real `fn help_lines()` in `newt-tui`, so the planner was told to edit a
/// file where the symbol does not exist.
struct GroundingBlock {
    term: String,
    defs: Vec<String>,
    mentions: Vec<String>,
}

/// Escape ERE metacharacters so a task term matches literally inside the
/// definition pattern (grep terms are usually bare symbols, but be safe).
fn ere_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.^$*+?()[]{}|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A POSIX-ERE pattern (for `git grep -E`) matching a Rust DEFINITION of `term`
/// — `fn`/`struct`/`trait`/`enum`/`type`/`const`/`static`/`union`/`mod`, with an
/// optional `pub`/`async`/`unsafe`, at line start, the symbol word-bounded — and
/// NOT a bare mention.
fn definition_grep_pattern(term: &str) -> String {
    format!(
        "^[[:space:]]*(pub[[:space:]]+)?(async[[:space:]]+)?(unsafe[[:space:]]+)?\
         (fn|struct|trait|enum|type|const|static|union|mod)[[:space:]]+{}([^A-Za-z0-9_]|$)",
        ere_escape(term)
    )
}

fn truncate_grep_line(line: &str) -> String {
    line.chars().take(160).collect()
}

/// Render grounding blocks for the plan-author prompt: DEFINITION sites first
/// (marked `[def]`, never truncated, per-term cap), so the real seam is always
/// surfaced, then a couple of non-definition mentions for context, under an
/// overall budget. Pure — unit-testable without git.
fn format_grounding_hits(blocks: &[GroundingBlock]) -> String {
    let mut out = String::new();
    let mut total = 0usize;
    for b in blocks {
        if total >= 25 {
            break;
        }
        // Definitions FIRST and untruncated — the real seam can never be buried
        // by an alphabetically-earlier decoy (the #687 fix).
        for d in b.defs.iter().take(6) {
            if total >= 25 {
                break;
            }
            out.push_str(&format!("  {} [def]: {}\n", b.term, truncate_grep_line(d)));
            total += 1;
        }
        // Then a couple of mentions not already shown as definitions.
        let mut shown_mentions = 0usize;
        for m in &b.mentions {
            if total >= 25 || shown_mentions >= 2 {
                break;
            }
            if b.defs.contains(m) {
                continue;
            }
            out.push_str(&format!("  {}: {}\n", b.term, truncate_grep_line(m)));
            total += 1;
            shown_mentions += 1;
        }
    }
    if out.is_empty() {
        return String::new();
    }
    format!(
        "\n\nRelevant existing code — task terms already appear here. A `[def]` \
         line is the DEFINITION site (the real seam to edit); implement AT these \
         sites, do NOT invent file paths:\n{out}"
    )
}

/// Grep the target repo for the task's high-signal terms and return where they
/// already appear (bounded), so the planner implements AT real sites instead of
/// guessing paths. Definition sites are surfaced first and never truncated
/// (#687). Best-effort: a non-repo `dir` / no matches yields "".
fn fetch_code_grep_hits(task: &str, dir: &Path) -> String {
    let lines = |res: anyhow::Result<String>| -> Vec<String> {
        res.map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default()
    };
    let mut blocks = Vec::new();
    for term in grep_terms(task) {
        let defs = lines(git(
            dir,
            &[
                "grep",
                "-nE",
                "-e",
                &definition_grep_pattern(&term),
                "--",
                "*.rs",
            ],
        ));
        let mentions = lines(git(dir, &["grep", "-n", "-F", "-e", &term, "--", "*.rs"]));
        if !defs.is_empty() || !mentions.is_empty() {
            blocks.push(GroundingBlock {
                term,
                defs,
                mentions,
            });
        }
    }
    format_grounding_hits(&blocks)
}

/// Read each GitHub issue/PR referenced in `goal` with `gh … view --json
/// title,body` and return their text. Best-effort: a `gh` that is missing,
/// unauthenticated, or errors contributes nothing (and prints a note), so
/// authoring still proceeds from the goal text alone.
fn fetch_referenced_docs(goal: &str) -> String {
    let mut out = String::new();
    for (owner, repo, kind, num) in github_refs(goal) {
        let sub = if kind == "pull" { "pr" } else { "issue" };
        let slug = format!("{owner}/{repo}");
        match std::process::Command::new("gh")
            .args([sub, "view", &num, "--repo", &slug, "--json", "title,body"])
            .output()
        {
            Ok(o) if o.status.success() => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                    let title = v["title"].as_str().unwrap_or("");
                    let body = v["body"].as_str().unwrap_or("");
                    out.push_str(&format!(
                        "\n\n--- {sub} {slug}#{num}: {title} ---\n{body}\n"
                    ));
                }
            }
            Ok(o) => eprintln!(
                "note: `gh {sub} view {num}` failed ({}) — planning from the goal text alone",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => {
                eprintln!("note: could not run `gh` ({e}) — planning from the goal text alone");
            }
        }
    }
    out
}

/// `newt plan --goal "<text>" --one-shot`: author a plan from the goal AND
/// execute it autonomously in one gesture — the headless autonomous drive (e.g.
/// the #548 evaluator). The `--one-shot` flag is the approval, like `--execute`.
pub async fn one_shot_goal_cli(
    goal: &str,
    dir: Option<PathBuf>,
    max_leaves: usize,
    locked_verify: Option<String>,
) -> anyhow::Result<i32> {
    // The repo we're about to modify IS the planning context (--dir, else cwd).
    let repo_dir = dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut plan = author_plan_to_plan(goal, max_leaves, &repo_dir).await?;
    print!("{}", render_plan_preview(&plan));
    grant_one_shot_authority(&mut plan);
    println!("\n--one-shot: executing the authored plan autonomously…");
    // No source file to shadow — write the run-log into the cwd.
    execute_plan(
        &mut plan,
        dir,
        max_leaves,
        Some(PathBuf::from("plan.run.toml")),
        locked_verify,
    )
    .await
}

pub async fn author_plan_cli(
    goal: String,
    output: Option<PathBuf>,
    max_subtasks: usize,
    dir: Option<PathBuf>,
) -> anyhow::Result<i32> {
    let repo_dir = dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let plan = author_plan_to_plan(&goal, max_subtasks, &repo_dir).await?;
    let toml = plan
        .to_toml_string()
        .map_err(|e| anyhow::anyhow!("serialize plan: {e}"))?;
    print!("{}", render_plan_preview(&plan));
    match &output {
        Some(path) => {
            // Don't clobber an existing plan the human may have edited (granted
            // caveats into). Authoring writes a fresh draft only.
            if path.exists() {
                anyhow::bail!(
                    "{} already exists — choose another -o path or remove it first \
                     (authoring won't overwrite a plan you may have edited)",
                    path.display()
                );
            }
            std::fs::write(path, &toml)
                .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
            println!(
                "\nwrote plan → {} — review/edit (grant caveats where needed), then \
                 `newt plan {} --execute`",
                path.display(),
                path.display()
            );
        }
        None => println!(
            "\n--- plan.toml (write to a file, then `newt plan <file> --execute`) ---\n{toml}"
        ),
    }
    Ok(0)
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
    let mut ws = WorktreeWorkspace::create(&dir, &worktree_id(), "HEAD", test_cmd)?;
    let crew_cfg = CrewConfig {
        navigator_model,
        planner_model,
        triage_model,
        max_attempts,
    };
    // Honest, non-top session caveats (the #94 guardrail forbids `Caveats::top()`
    // in dispatch code): exec/net are locked down. The REAL fs boundary here is
    // the throwaway git worktree, NOT a caveat leash: `worker_session_caveats(None)`
    // returns `fs_write = Scope::All` (identity.rs), so `permits_fs_write` is true
    // for every path and `run_crew`'s REFUSED branch is dead code. A scoped
    // `fs_write` leash would be a real second boundary, but isn't wired yet.
    //
    // NOTE: the CWD fs-lock (newt_core::caveats::apply_cli_fs_grants, applied for
    // `newt code`) is NOT applied here yet — `run_crew` enforces `permits_fs_write`
    // by EXACT match on the planner's (relative) edit paths, which an
    // absolute-workspace-root grant can't match, so the lock would deny
    // legitimate edits. The crew is already sandboxed by its isolated git
    // worktree. Reconciling the scheduler's path-enforcement with the lock is a
    // follow-up.
    let caveats = newt_acp_worker::worker_session_caveats(None);
    let outcome = run_crew(&pool, dispatcher, &mut ws, &crew_cfg, &caveats, &args.task).await;
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
pub(crate) fn model_for_role(cfg: &Config, loadout_name: &str) -> anyhow::Result<String> {
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
pub(crate) fn worktree_id() -> String {
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

    #[test]
    fn plan_author_prompt_steers_to_minimal_action_only_subtasks() {
        // Regression: the planner must author the FEWEST, code-changing subtasks and
        // NOT separate inspect/verify/test steps. Those steps land nothing and (after
        // the crew-honesty fix) are correctly marked Failed — leaving a plan whose fix
        // DID land falsely reported "incomplete". Steering the prompt is the root fix.
        assert!(PLAN_AUTHOR_SYSTEM.contains("FEWEST"));
        assert!(PLAN_AUTHOR_SYSTEM.contains("MUST change code"));
        assert!(PLAN_AUTHOR_SYSTEM.contains("Do NOT create separate"));
        assert!(PLAN_AUTHOR_SYSTEM.contains("verifies EVERY subtask"));
    }

    #[test]
    fn github_refs_parses_issue_and_pr_urls_in_prose() {
        // The exact prompt shape from the #548 exercise: a URL in surrounding prose.
        let refs = github_refs(
            "https://github.com/Gilamonster-Foundation/newt-agent/issues/548 <- take a look",
        );
        assert_eq!(
            refs,
            vec![(
                "Gilamonster-Foundation".to_string(),
                "newt-agent".to_string(),
                "issues".to_string(),
                "548".to_string(),
            )]
        );
        // PR URLs (kind = pull) and trailing punctuation on the number.
        let refs = github_refs("see https://github.com/o/r/pull/12).");
        assert_eq!(refs[0].2, "pull");
        assert_eq!(refs[0].3, "12");
        // No GitHub URL ⇒ nothing (authoring just uses the goal text).
        assert!(github_refs("implement the thing").is_empty());
    }

    #[test]
    fn one_shot_authority_grants_fs_keeps_exec_and_net_denied() {
        use newt_core::role_profile::{ScopeKeyword, ScopeSpec};
        let denied = ScopeSpec::Keyword(ScopeKeyword::None);
        let all = ScopeSpec::Keyword(ScopeKeyword::All);
        let mut plan = newt_core::plan::Plan::from_toml_str(
            "goal = \"g\"\n[[subtask]]\nid = \"a\"\ninstruction = \"do a thing\"\n",
        )
        .unwrap();
        // Authored default is fully denied on every axis.
        assert_eq!(plan.subtasks[0].caveat_policy.fs_write, denied);
        grant_one_shot_authority(&mut plan);
        let pol = &plan.subtasks[0].caveat_policy;
        assert_eq!(pol.fs_read, all, "fs_read granted");
        assert_eq!(pol.fs_write, all, "fs_write granted");
        // No shell or network authority is handed out.
        assert_eq!(pol.exec, denied, "exec stays denied");
        assert_eq!(pol.net, denied, "net stays denied");
    }

    #[test]
    fn plan_preview_lists_leaves_and_their_deps() {
        // Pure (in-memory plan, no fs): the preview shows the goal, the subtask
        // count, and the LEAVES (dispatch units) — a branch is not listed.
        let plan = newt_core::plan::Plan::from_toml_str(
            "goal = \"ship it\"\n\
             [[subtask]]\nid=\"epic\"\ninstruction=\"big\"\n\
             [[subtask]]\nid=\"a\"\ninstruction=\"do a\"\nparent=\"epic\"\n\
             [[subtask]]\nid=\"b\"\ninstruction=\"do b\"\nparent=\"epic\"\ndeps=[\"a\"]\n",
        )
        .unwrap();
        let preview = render_plan_preview(&plan);
        assert!(preview.contains("goal: ship it"), "{preview}");
        assert!(preview.contains("3 subtask(s); 2 leaf"), "{preview}");
        assert!(preview.contains("• a — do a"), "{preview}");
        assert!(preview.contains("• b — do b  (after a)"), "{preview}");
        assert!(
            !preview.contains("• epic"),
            "a branch is not a dispatch unit"
        );
        assert!(!preview.contains("will stall"), "a-leaf dep is satisfiable");
    }

    #[test]
    fn plan_preview_flags_deps_that_will_stall() {
        // A leaf depending on a BRANCH (epic, never dispatched) or an ABSENT id
        // can never reach Done → flag it so the preview reveals an unrunnable plan.
        let plan = newt_core::plan::Plan::from_toml_str(
            "[[subtask]]\nid=\"epic\"\ninstruction=\"branch\"\n\
             [[subtask]]\nid=\"a\"\ninstruction=\"do a\"\nparent=\"epic\"\ndeps=[\"epic\"]\n\
             [[subtask]]\nid=\"b\"\ninstruction=\"do b\"\nparent=\"epic\"\ndeps=[\"ghost\"]\n",
        )
        .unwrap();
        let preview = render_plan_preview(&plan);
        assert!(
            preview.contains("epic [will stall]"),
            "branch dep: {preview}"
        );
        assert!(
            preview.contains("ghost [will stall]"),
            "absent dep: {preview}"
        );
    }

    #[test]
    fn worktree_path_guard_refuses_absolute_and_dotdot() {
        // The structural worktree boundary (Path::join discards the base for an
        // absolute path, so fs_write=All is not the boundary — this guard is).
        assert!(is_safe_worktree_path("src/lib.rs"));
        assert!(is_safe_worktree_path("a/b/c.txt"));
        assert!(!is_safe_worktree_path("/etc/passwd"), "absolute escapes");
        assert!(
            !is_safe_worktree_path("../../../etc/cron.d/x"),
            ".. escapes"
        );
        assert!(!is_safe_worktree_path("a/../../b"), "embedded .. escapes");
    }

    #[test]
    fn worktree_read_is_confined_to_the_worktree() {
        // #521: a crew read is fenced to the worktree — an absolute / `..` path
        // can't escape to the host, while a legitimate relative read still works.
        let repo = git_repo();
        let ws =
            WorktreeWorkspace::create(repo.path(), &worktree_id(), "HEAD", "true".into()).unwrap();
        assert_eq!(ws.read("hello.txt").as_deref(), Some("world\n"));
        assert!(ws.read("/etc/hostname").is_none(), "absolute read refused");
        assert!(
            ws.read("../../../../etc/hostname").is_none(),
            ".. read refused"
        );
    }

    #[test]
    fn parse_authored_plan_maps_json_to_a_default_deny_plan() {
        // Tolerates fences/prose; maps deps/verify; authority is NOT model-granted.
        let raw = "Sure! ```json\n{\"goal\":\"g\",\"subtasks\":[\
            {\"id\":\"a\",\"instruction\":\"do a\",\"verify\":\"just check\"},\
            {\"id\":\"b\",\"instruction\":\"do b\",\"deps\":[\"a\"]}]}\n``` (done)";
        let plan = parse_authored_plan(raw).expect("parsed a plan");
        assert_eq!(plan.goal.as_deref(), Some("g"));
        assert_eq!(plan.subtasks.len(), 2);
        assert_eq!(plan.subtasks[1].deps, vec!["a"]);
        assert_eq!(plan.subtasks[0].verify.as_deref(), Some("just check"));
        assert!(plan.subtasks[0].parent.is_none());
        // default-DENY: the model proposes work, never authority.
        assert_eq!(
            plan.subtasks[0].caveat_policy,
            newt_core::plan::CaveatPolicy::default()
        );
    }

    #[test]
    fn parse_authored_plan_rejects_empty_or_unparseable() {
        assert!(parse_authored_plan("no json at all").is_none());
        assert!(
            parse_authored_plan("{\"goal\":\"g\",\"subtasks\":[]}").is_none(),
            "empty subtasks → not a usable plan"
        );
        assert!(parse_authored_plan("{not json}").is_none());
        // DUPLICATE ids → rejected (they would desync the execute-time cursor into
        // an unbounded re-dispatch loop).
        assert!(
            parse_authored_plan(
                "{\"subtasks\":[{\"id\":\"a\",\"instruction\":\"x\"},{\"id\":\"a\",\"instruction\":\"y\"}]}"
            )
            .is_none(),
            "duplicate ids"
        );
        // Empty / whitespace id or instruction → rejected.
        assert!(
            parse_authored_plan("{\"subtasks\":[{\"id\":\"  \",\"instruction\":\"x\"}]}").is_none(),
            "blank id"
        );
        assert!(
            parse_authored_plan("{\"subtasks\":[{\"id\":\"a\",\"instruction\":\"\"}]}").is_none(),
            "empty instruction"
        );
    }

    #[tokio::test]
    async fn author_plan_decomposes_a_goal_via_the_model() {
        struct PlanMock;
        #[async_trait::async_trait]
        impl Dispatcher for PlanMock {
            async fn dispatch(
                &self,
                _b: &newt_scheduler::PoolBackend,
                _m: &str,
                _r: newt_scheduler::ChatRequest,
            ) -> anyhow::Result<newt_scheduler::ChatReply> {
                Ok(newt_scheduler::ChatReply {
                    content: "```json\n{\"goal\":\"ship\",\"subtasks\":[\
                        {\"id\":\"a\",\"instruction\":\"do a\",\"verify\":\"cargo test\"},\
                        {\"id\":\"b\",\"instruction\":\"do b\",\"deps\":[\"a\"]}]}\n```"
                        .to_string(),
                    model_id: "m".into(),
                    usage: None,
                })
            }
        }
        let cfg: Config = toml::from_str(
            "[[backends]]\nname=\"x\"\nendpoint=\"http://x:11434\"\nmodel=\"m\"\ntiers=[]\n",
        )
        .unwrap();
        let pool = BackendPool::from_source(&StaticSource::from_configs(cfg.backends.iter()));
        let plan = author_plan(&pool, &PlanMock, "m", "ship the thing", 8)
            .await
            .expect("authored a plan");
        assert_eq!(plan.goal.as_deref(), Some("ship"));
        assert_eq!(plan.subtasks.len(), 2);
        assert_eq!(plan.subtasks[1].deps, vec!["a"]);
        assert_eq!(
            plan.subtasks[0].caveat_policy,
            newt_core::plan::CaveatPolicy::default()
        );
    }

    /// A throwaway git repo with one committed file at `name` containing `body`.
    #[test]
    fn grep_terms_extracts_slash_commands_and_backtick_tokens_only() {
        let terms =
            grep_terms("roll up `/dgx` help; refactor `help_lines` and /models — but not help");
        assert!(terms.contains(&"/dgx".to_string()), "{terms:?}");
        assert!(terms.contains(&"/models".to_string()), "{terms:?}");
        assert!(terms.contains(&"help_lines".to_string()), "{terms:?}");
        // Bare common words (like "help") are NOT extracted — too noisy to grep.
        assert!(!terms.iter().any(|t| t == "help"), "{terms:?}");
    }

    #[test]
    fn plan_sanity_flags_dangling_deps_and_passes_clean_plans() {
        // Clean: b depends on a, both defined.
        let ok = newt_core::plan::Plan::from_toml_str(
            "goal = \"g\"\n[[subtask]]\nid = \"a\"\ninstruction = \"do a\"\n\
             [[subtask]]\nid = \"b\"\ninstruction = \"do b\"\ndeps = [\"a\"]\n",
        )
        .unwrap();
        assert!(plan_sanity(&ok).is_empty(), "{:?}", plan_sanity(&ok));
        // Dangling: b depends on a `ghost` no subtask defines.
        let bad = newt_core::plan::Plan::from_toml_str(
            "goal = \"g\"\n[[subtask]]\nid = \"b\"\ninstruction = \"do b\"\ndeps = [\"ghost\"]\n",
        )
        .unwrap();
        let probs = plan_sanity(&bad);
        assert!(probs.iter().any(|p| p.contains("ghost")), "{probs:?}");
    }

    #[test]
    fn repo_context_detects_language_layout_and_skips_non_repo() {
        let repo = git_repo();
        std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(repo.path().join("main.rs"), "fn main() {}\n").unwrap();
        git(repo.path(), &["add", "-A"]).unwrap();
        git(repo.path(), &["commit", "-qm", "add cargo"]).unwrap();
        let ctx = fetch_repo_context(repo.path());
        assert!(ctx.contains("Rust"), "detects Rust: {ctx}");
        assert!(
            ctx.contains("cargo test"),
            "infers the build command: {ctx}"
        );
        assert!(ctx.contains("Cargo.toml"), "lists top-level entries: {ctx}");
        // A non-repo dir contributes nothing (authoring uses the goal text alone).
        let empty = tempfile::tempdir().unwrap();
        assert!(fetch_repo_context(empty.path()).is_empty());
    }

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
            // Keep line endings verbatim so checked-out content matches on
            // Windows runners (default autocrlf would turn "\n" into "\r\n").
            vec!["config", "core.autocrlf", "false"],
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
    fn commit_to_branch_lands_work_visible_to_base() {
        let repo = git_repo();
        let mut ws =
            WorktreeWorkspace::create(repo.path(), "land1", "HEAD", "true".into()).unwrap();
        ws.apply(&[Edit {
            path: "added.rs".into(),
            new_content: "pub fn f() {}\n".into(),
        }]);
        let (branch, sha) = ws
            .commit_to_branch("crew/land1", "newt", "newt@bot", "land it")
            .unwrap();
        assert_eq!(branch, "crew/land1");
        assert!(!sha.is_empty());
        // The branch lives in the SHARED object store → the base repo sees it and
        // it carries the work, even after the worktree is dropped.
        drop(ws);
        let files = git(repo.path(), &["ls-tree", "-r", "--name-only", "crew/land1"]).unwrap();
        assert!(
            files.lines().any(|l| l == "added.rs"),
            "branch carries the work: {files}"
        );
        // Base working tree is untouched until a human merges the branch.
        assert!(
            !repo.path().join("added.rs").exists(),
            "base tree untouched until merge"
        );
    }

    #[test]
    fn leaf_chains_off_prior_landed_tip() {
        // Leaf composition (#646): leaf B forks off leaf A's LANDED tip, so it
        // SEES A's work and its branch consolidates both — the property that
        // makes a multi-leaf one-shot run produce one coherent change instead of
        // N scattered single-step branches. (Real-fs tier; migrate per #514.)
        let repo = git_repo();

        // Leaf A: edit a.txt off HEAD, land crew/a.
        let mut wsa = WorktreeWorkspace::create(repo.path(), "a", "HEAD", "true".into()).unwrap();
        wsa.apply(&[Edit {
            path: "a.txt".into(),
            new_content: "A\n".into(),
        }]);
        let (_a, sha_a) = wsa.commit_to_branch("crew/a", "n", "n@b", "a").unwrap();
        drop(wsa);

        // Leaf B forks off A's landed sha — it MUST see a.txt (the chain).
        let mut wsb = WorktreeWorkspace::create(repo.path(), "b", &sha_a, "true".into()).unwrap();
        assert!(
            wsb.read("a.txt").is_some(),
            "leaf B forked off A's tip sees A's file"
        );
        wsb.apply(&[Edit {
            path: "b.txt".into(),
            new_content: "B\n".into(),
        }]);
        wsb.commit_to_branch("crew/b", "n", "n@b", "b").unwrap();
        drop(wsb);

        // crew/b is the single CONSOLIDATED tip — it carries BOTH leaves' work.
        let files = git(repo.path(), &["ls-tree", "-r", "--name-only", "crew/b"]).unwrap();
        assert!(
            files.lines().any(|l| l == "a.txt"),
            "consolidated tip has a.txt: {files}"
        );
        assert!(
            files.lines().any(|l| l == "b.txt"),
            "consolidated tip has b.txt: {files}"
        );
    }

    #[test]
    fn commit_to_branch_errs_with_no_changes() {
        let repo = git_repo();
        let ws = WorktreeWorkspace::create(repo.path(), "land2", "HEAD", "true".into()).unwrap();
        assert!(
            ws.commit_to_branch("crew/land2", "n", "n@b", "noop")
                .is_err(),
            "no changes → nothing to land"
        );
    }

    #[test]
    fn worktree_isolates_reads_and_writes() {
        let repo = git_repo();
        let mut ws = WorktreeWorkspace::create(repo.path(), "t1", "HEAD", "true".into()).unwrap();

        // files() lists tracked files; read() reads them (line-ending-tolerant).
        assert!(ws.files().iter().any(|f| f == "hello.txt"));
        assert_eq!(
            ws.read("hello.txt").as_deref().map(str::trim_end),
            Some("world")
        );
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
        let ok = WorktreeWorkspace::create(repo.path(), "t2a", "HEAD", "test -f hello.txt".into())
            .unwrap();
        assert!(ok.run_test().0, "committed file present → pass");
        let bad = WorktreeWorkspace::create(repo.path(), "t2b", "HEAD", "exit 3".into()).unwrap();
        assert!(!bad.run_test().0, "non-zero exit → fail");
    }

    #[test]
    fn cleanup_removes_the_worktree() {
        let repo = git_repo();
        let path = {
            let ws = WorktreeWorkspace::create(repo.path(), "t3", "HEAD", "true".into()).unwrap();
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

    // -- #687: grounding surfaces the real definition, not an earlier-sorting decoy --

    #[test]
    fn grounding_surfaces_the_definition_even_when_decoys_sort_first() {
        // The #548 shape: newt-cli/crew.rs mentions of `help_lines` (decoys) sort
        // before the real `fn help_lines()` in newt-tui — they must NOT bury it.
        let blocks = vec![GroundingBlock {
            term: "help_lines".to_string(),
            defs: vec![
                "newt-tui/src/lib.rs:8273:fn help_lines() -> &'static [&'static str] {".to_string(),
            ],
            mentions: vec![
                "newt-cli/src/crew.rs:100:    // help_lines rolls up the dgx block".to_string(),
                "newt-cli/src/crew.rs:200:    assert!(out.contains(\"help_lines\"));".to_string(),
                "newt-cli/src/crew.rs:300:    let _ = help_lines_marker;".to_string(),
            ],
        }];
        let out = format_grounding_hits(&blocks);
        assert!(
            out.contains("newt-tui/src/lib.rs:8273") && out.contains("[def]"),
            "the real definition must be surfaced and marked: {out}"
        );
    }

    #[test]
    fn grounding_never_drops_a_definition_under_the_budget() {
        // Many mentions across many terms must not crowd a definition out.
        let blocks: Vec<GroundingBlock> = (0..30)
            .map(|i| GroundingBlock {
                term: format!("sym{i}"),
                defs: vec![format!("src/a.rs:{i}:fn sym{i}() {{")],
                mentions: (0..8)
                    .map(|j| format!("src/b.rs:{j}:// sym{i} mention {j}"))
                    .collect(),
            })
            .collect();
        let out = format_grounding_hits(&blocks);
        assert!(
            out.contains("[def]"),
            "definitions surface under the budget: {out}"
        );
        // the first hit is a definition, never a mention.
        let first = out
            .lines()
            .find(|l| l.trim_start().starts_with("sym"))
            .unwrap();
        assert!(
            first.contains("[def]"),
            "first hit must be a definition: {first}"
        );
    }

    #[test]
    fn empty_blocks_yield_empty_grounding() {
        assert!(format_grounding_hits(&[]).is_empty());
    }

    #[test]
    fn definition_grep_pattern_matches_defs_not_mentions() {
        let re = regex::Regex::new(&definition_grep_pattern("help_lines")).unwrap();
        assert!(re.is_match("fn help_lines() -> &'static [&'static str] {"));
        assert!(re.is_match("    pub async fn help_lines() {"));
        assert!(re.is_match("pub fn help_lines<T>(x: T) {"));
        assert!(!re.is_match("    // help_lines is the seam"));
        assert!(!re.is_match("    let v = self.help_lines();"));
        assert!(!re.is_match("fn help_lines_other() {")); // word-bounded, not a prefix
    }

    #[test]
    fn unresolved_symbol_parses_rustc_errors() {
        assert_eq!(
            unresolved_symbol("error[E0425]: cannot find function `help_lines` in this scope"),
            Some("help_lines".to_string())
        );
        assert_eq!(
            unresolved_symbol("error[E0599]: no method named `roll_up` found for struct"),
            Some("roll_up".to_string())
        );
        assert_eq!(unresolved_symbol("error: mismatched types"), None);
    }
}
