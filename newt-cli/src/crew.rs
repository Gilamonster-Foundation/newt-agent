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
    // Run-log lands in a SIBLING artifact — never modify the authored source.
    let log_path = args.file.with_extension("run.toml");
    execute_plan(&mut plan, args.dir, args.max_leaves, Some(log_path)).await
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
) -> anyhow::Result<i32> {
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
    let runner =
        crate::crew_runner::LocalCrewRunner::new(cfg, dir, newt_core::agentic::Presence::Prompt);
    let run = newt_core::agentic::run_plan(plan, &caveats, &runner).await;
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
    dependency-ordered plan of small, independently-verifiable engineering subtasks. Reply \
    with ONLY JSON: {\"goal\":\"<the goal>\",\"subtasks\":[{\"id\":\"<short-kebab-id>\",\
    \"instruction\":\"<imperative step>\",\"deps\":[\"<id of a step that must finish first>\"],\
    \"verify\":\"<shell command that exits 0 once THIS step is done; omit if none>\"}]}. Use \
    `deps` for ordering (a step lists the ids it waits on). Ids: short, stable, unique. \
    Smallest-first. Do NOT grant permissions or describe authority — only the work.";

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
    // Phase 1 (#646): READ any GitHub issue/PR the goal references, via `gh` (a
    // harness-available tool), so the planner decomposes the ACTUAL document
    // instead of a bare link it cannot fetch. Best-effort — a missing /
    // unauthenticated `gh` just leaves the goal text alone.
    let docs = fetch_referenced_docs(goal);
    let effective_goal = if docs.is_empty() {
        goal.to_string()
    } else {
        println!(
            "read referenced GitHub doc(s) via gh ({} chars)",
            docs.len()
        );
        format!(
            "{goal}\n\nThe referenced document(s) below are the TASK to implement — \
             decompose THESE into concrete engineering subtasks:{docs}"
        )
    };
    // Phase 2: decompose the (doc-augmented) goal into a plan.
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
) -> anyhow::Result<i32> {
    let mut plan = author_plan_to_plan(goal, max_leaves).await?;
    print!("{}", render_plan_preview(&plan));
    println!("\n--one-shot: executing the authored plan autonomously…");
    // No source file to shadow — write the run-log into the cwd.
    execute_plan(
        &mut plan,
        dir,
        max_leaves,
        Some(PathBuf::from("plan.run.toml")),
    )
    .await
}

pub async fn author_plan_cli(
    goal: String,
    output: Option<PathBuf>,
    max_subtasks: usize,
) -> anyhow::Result<i32> {
    let plan = author_plan_to_plan(&goal, max_subtasks).await?;
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
    let mut ws = WorktreeWorkspace::create(&dir, &worktree_id(), test_cmd)?;
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
        let ws = WorktreeWorkspace::create(repo.path(), &worktree_id(), "true".into()).unwrap();
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
        let mut ws = WorktreeWorkspace::create(repo.path(), "land1", "true".into()).unwrap();
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
    fn commit_to_branch_errs_with_no_changes() {
        let repo = git_repo();
        let ws = WorktreeWorkspace::create(repo.path(), "land2", "true".into()).unwrap();
        assert!(
            ws.commit_to_branch("crew/land2", "n", "n@b", "noop")
                .is_err(),
            "no changes → nothing to land"
        );
    }

    #[test]
    fn worktree_isolates_reads_and_writes() {
        let repo = git_repo();
        let mut ws = WorktreeWorkspace::create(repo.path(), "t1", "true".into()).unwrap();

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
