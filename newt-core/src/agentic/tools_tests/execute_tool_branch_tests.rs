use super::super::mcp::LeasedMcpCall;
use super::super::NoMcp;
use super::*;
use crate::agentic::{
    ArtifactReadContext, ArtifactReadRecord, PromptArtifactSink, SessionArtifactStore,
};
use crate::artifact::{ArtifactId, ArtifactKind, ArtifactRelation, NewPromptArtifact};
use crate::caveats::{Caveats, CountBound, Scope};
use crate::PromptId;
use std::sync::Mutex;

/// fs read everywhere, fs write scoped to the workspace (skips the y/N
/// confirm — the scoped preset is the consent), nothing else.
fn caveats_rw(ws: &std::path::Path) -> Caveats {
    Caveats {
        fs_read: Scope::All,
        fs_write: Scope::only([ws.to_string_lossy().into_owned()]),
        exec: Scope::none(),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

#[derive(Default)]
struct RecordingArtifactSink {
    artifacts: Mutex<Vec<NewPromptArtifact>>,
}

impl RecordingArtifactSink {
    fn only_artifact(&self) -> NewPromptArtifact {
        let artifacts = self.artifacts.lock().unwrap();
        assert_eq!(artifacts.len(), 1, "expected exactly one artifact");
        artifacts[0].clone()
    }

    // Matches its only caller (`physical_symlink_escape_write_is_denied_
    // object_bound`, Linux-only) — `cfg(unix)` left it dead-code on macOS.
    #[cfg(target_os = "linux")]
    fn is_empty(&self) -> bool {
        self.artifacts.lock().unwrap().is_empty()
    }
}

impl PromptArtifactSink for RecordingArtifactSink {
    fn append_artifact(
        &self,
        originating_prompt_id: PromptId,
        objective_root_id: PromptId,
        artifact: NewPromptArtifact,
    ) -> anyhow::Result<ArtifactReadRecord> {
        let mut artifacts = self.artifacts.lock().unwrap();
        artifacts.push(artifact.clone());
        Ok(ArtifactReadRecord {
            id: ArtifactId::new(),
            prompt_id: originating_prompt_id,
            root_prompt_id: objective_root_id,
            writer_fingerprint: "tool-test".to_string(),
            seq: artifacts.len() as u64,
            prev_hash: "previous".to_string(),
            kind: format!("{:?}", artifact.kind()),
            relation: format!("{:?}", artifact.relation()),
            locator: artifact.locator().map(str::to_string),
            body: artifact.body().map(str::to_string),
            metadata: artifact.metadata().clone(),
            ts_claim: 1,
            artifact_hash: "hash".to_string(),
        })
    }
}

fn artifact_context() -> ArtifactReadContext<'static> {
    let prompt = PromptId::new();
    ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), None)
}

#[allow(clippy::too_many_arguments)]
async fn run_artifact_tool(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    build_check: Option<&str>,
    sink: &RecordingArtifactSink,
) -> String {
    execute_tool_with_offload_and_prompt_and_artifacts(
        name,
        &args,
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        build_check,
        None, // note_sink
        None, // recall_source
        None, // memory_source
        None, // prompt_context
        Some(artifact_context()),
        Some(sink),
        None,  // permission_gate
        None,  // exec_floor
        None,  // git_tool
        None,  // crew_runner
        None,  // scratchpad_store
        None,  // code_search
        None,  // where_is
        None,  // experience_store
        None,  // step_ledger
        false, // tool_offload
        None,  // spill_store
        None,  // persona_tools
        PromptDisposition::Act,
    )
    .await
}

// --- PR4: the `git` tool arm in execute_tool ---------------------------

/// A stub GitTool: echoes the op, and refuses `commit` when the projected
/// GitCaveats deny it — exercises the arm's caveat projection without a repo.
struct StubGit;
impl crate::agentic::GitTool for StubGit {
    fn dispatch(
        &self,
        op: &str,
        _args: &serde_json::Value,
        caps: &crate::git_caveats::GitCaveats,
    ) -> Result<String, String> {
        match op {
            "status" => Ok("on branch main (HEAD abc123)".to_string()),
            "commit" if !caps.permits_commit() => {
                Err("capability denied: git commit not permitted".to_string())
            }
            "commit" => Ok("committed abc123: msg".to_string()),
            // #1191: data-loss ops the gate guards — if we reach here, the
            // gate ALLOWED (the refusal path returns before dispatch).
            "stash-drop" => Ok("dropped stash@{0}".to_string()),
            "branch-delete" => Ok("deleted branch feature".to_string()),
            other => Err(format!("unknown git op '{other}'")),
        }
    }
}

async fn run_scheduled_tool(
    name: &str,
    args: &serde_json::Value,
    ws: &tempfile::TempDir,
    ledger: &crate::agentic::scheduled::SessionStepLedger,
    plan_mode_control: &dyn crate::agentic::PlanModeControl,
) -> String {
    execute_tool_with_collaborators(
        name,
        args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats_rw(ws.path()),
        &mut NoMcp,
        ToolCollaborators {
            step_ledger: Some(ledger as &dyn crate::agentic::scheduled::StepLedger),
            plan_mode_control: Some(plan_mode_control),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .expect("test dispatch is not cancellable")
}

async fn run_git(op: &str, caveats: &Caveats, git: Option<&dyn crate::agentic::GitTool>) -> String {
    let ws = tempfile::TempDir::new().unwrap();
    execute_tool(
        "git",
        &serde_json::json!({ "op": op }),
        &ws.path().to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        git,
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await
}

#[tokio::test]
async fn git_arm_dispatches_when_injected() {
    let ws = tempfile::TempDir::new().unwrap();
    let out = run_git("status", &caveats_rw(ws.path()), Some(&StubGit)).await;
    assert!(out.contains("on branch main"), "got: {out}");
}

#[tokio::test]
async fn git_arm_surfaces_denials_from_projected_caveats() {
    // A session with no fs_write → from_session denies commit_local.
    let ws = tempfile::TempDir::new().unwrap();
    let read_only = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_git("commit", &read_only, Some(&StubGit)).await;
    assert!(
        out.contains("error:") && out.contains("commit"),
        "got: {out}"
    );
    // The same session can still run a read op.
    let out = run_git("status", &read_only, Some(&StubGit)).await;
    assert!(out.contains("on branch main"), "got: {out}");
}

/// Same as `run_git` but with a gate AND the git tool both injected — the
/// #1056 path where a denied git write consults the operator.
#[allow(clippy::too_many_arguments)]
async fn run_git_gated(
    op: &str,
    caveats: &Caveats,
    git: &dyn crate::agentic::GitTool,
    gate: &mut MockGate,
) -> String {
    let ws = tempfile::TempDir::new().unwrap();
    execute_tool(
        "git",
        &serde_json::json!({ "op": op }),
        &ws.path().to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        Some(gate), // permission_gate
        None,       // exec_floor
        Some(git),  // git_tool
        None,       // crew_runner
        None,       // scratchpad_store
        None,       // code_search
        None,       // where_is
        None,       // experience_store
        None,       // step_ledger
    )
    .await
}

/// #1056: a git WRITE the projected authority denies is no longer a dead end
/// — with a gate that ALLOWS, the arm re-dispatches under the local-write
/// surface and the commit lands (the deadlock fix). The gate is consulted for
/// a `git_write` capability.
#[test]
fn plan_phase_seam_and_clamp() {
    use crate::caveats::ScopeExt as _;
    // The clamp is read-only: reads yes, writes/exec/net no.
    let c = plan_phase_clamp();
    assert!(c.fs_read.permits(&"/anything".to_string()));
    assert!(!c.fs_write.permits(&"/anything".to_string()));
    assert!(!c.exec.permits(&"cargo".to_string()));
    assert!(!c.net.permits(&"github.com".to_string()));
    // MEETing it into a full grant yields read-only (never widens).
    let full = crate::caveats::Caveats::top();
    let planned = full.meet(&c);
    assert!(
        !planned.fs_write.permits(&"/x".to_string()),
        "writes denied in plan phase"
    );
    assert!(planned.fs_read.permits(&"/x".to_string()), "reads allowed");
}

#[tokio::test]
async fn enter_and_exit_plan_mode_are_session_local_and_immediate() {
    use crate::agentic::PlanModeControl as _;

    #[derive(Default)]
    struct TestPlanModeControl(std::sync::atomic::AtomicBool);

    impl crate::agentic::PlanModeControl for TestPlanModeControl {
        fn is_plan_mode(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::Acquire)
        }

        fn set_plan_mode(&self, active: bool) -> Result<(), String> {
            self.0.store(active, std::sync::atomic::Ordering::Release);
            Ok(())
        }
    }

    // enter_plan_mode / exit_plan_mode mutate only their injected session
    // control; there is no process-global flag shared with another session.
    let ws = tempfile::TempDir::new().unwrap();
    let ledger = crate::agentic::scheduled::SessionStepLedger::default();
    let control = TestPlanModeControl::default();
    let other_session = TestPlanModeControl::default();
    assert!(!control.is_plan_mode());
    let control_only = execute_tool_with_collaborators(
        "enter_plan_mode",
        &serde_json::json!({}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats_rw(ws.path()),
        &mut NoMcp,
        ToolCollaborators {
            plan_mode_control: Some(&control),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .expect("test dispatch is not cancellable");
    assert!(
        control_only.contains("scheduled planning"),
        "control-only fabricated call must fail honestly: {control_only}"
    );
    assert!(
        !control.is_plan_mode(),
        "a control without a plan ledger must not enter Plan"
    );
    let control_only_exit = execute_tool_with_collaborators(
        "exit_plan_mode",
        &serde_json::json!({}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats_rw(ws.path()),
        &mut NoMcp,
        ToolCollaborators {
            plan_mode_control: Some(&control),
            ..Default::default()
        },
        false,
        PromptDisposition::Plan,
        None,
    )
    .await
    .expect("test dispatch is not cancellable");
    assert!(
        control_only_exit.contains("exited the model-entered PLAN PHASE"),
        "exit must remain available when scheduled planning is off: {control_only_exit}"
    );
    let enter = run_scheduled_tool(
        "enter_plan_mode",
        &serde_json::json!({}),
        &ws,
        &ledger,
        &control,
    )
    .await;
    assert!(enter.contains("PLAN MODE"), "{enter}");
    assert!(control.is_plan_mode(), "enter_plan_mode set the phase");
    assert!(
        !other_session.is_plan_mode(),
        "one session must not change another"
    );
    let denied_write = run_scheduled_tool(
        "write_file",
        &serde_json::json!({
            "path": "must-not-write.txt",
            "content": "no",
        }),
        &ws,
        &ledger,
        &control,
    )
    .await;
    assert!(
        denied_write.contains("is not available for this request"),
        "a write later in the same tool round must hit the immediate Plan clamp: {denied_write}"
    );
    assert!(
        !ws.path().join("must-not-write.txt").exists(),
        "entering Plan must prevent a later call from mutating the workspace"
    );
    let exit = run_scheduled_tool(
        "exit_plan_mode",
        &serde_json::json!({}),
        &ws,
        &ledger,
        &control,
    )
    .await;
    assert!(
        exit.contains("exited the model-entered PLAN PHASE"),
        "{exit}"
    );
    assert!(!control.is_plan_mode(), "exit_plan_mode cleared the phase");
}

#[tokio::test]
async fn git_data_loss_ops_are_gated_even_under_full_write_authority() {
    // #1191: the exact catastrophe — a confused model tries to destroy
    // work (stash-drop / branch-delete). Even with FULL write authority
    // (the --full-access analogue), the op is refused without an explicit
    // operator confirmation, and proceeds only WITH it. Safe ops never
    // consult the data-loss gate.
    let ws = tempfile::TempDir::new().unwrap();
    let full = caveats_rw(ws.path());

    // Gate DECLINES → refused, StubGit never dropped the stash.
    let mut deny = MockGate::new(false, &full);
    let out = run_git_gated("stash-drop", &full, &StubGit, &mut deny).await;
    assert!(
        out.starts_with("refused:"),
        "must refuse without confirm: {out}"
    );
    assert!(
        !out.contains("dropped stash"),
        "the drop must NOT have run: {out}"
    );
    assert!(
        deny.asks
            .iter()
            .any(|(t, k)| t == "git" && k.contains("stash-drop")),
        "the data-loss confirmation was asked: {:?}",
        deny.asks
    );

    // Gate ALLOWS → proceeds.
    let mut allow = MockGate::new(true, &full);
    let out = run_git_gated("branch-delete", &full, &StubGit, &mut allow).await;
    assert!(
        out.contains("deleted branch"),
        "confirmed → proceeds: {out}"
    );

    // A SAFE op is never gated as data-loss.
    let mut g = MockGate::new(false, &full);
    let out = run_git_gated("status", &full, &StubGit, &mut g).await;
    assert!(out.contains("on branch main"), "safe op runs: {out}");
    assert!(
        !g.asks
            .iter()
            .any(|(_, k)| k.contains("stash-drop") || k.contains("branch-delete")),
        "status must not trip the data-loss gate: {:?}",
        g.asks
    );
}

#[tokio::test]
async fn git_data_loss_op_refused_headless_no_gate() {
    // No permission gate (headless) → a data-loss op is refused, never run.
    let ws = tempfile::TempDir::new().unwrap();
    let out = run_git("stash-drop", &caveats_rw(ws.path()), Some(&StubGit)).await;
    assert!(
        out.starts_with("refused:"),
        "headless data-loss refused: {out}"
    );
}

#[tokio::test]
async fn git_write_denial_routes_through_gate_and_commits_on_allow() {
    let ws = tempfile::TempDir::new().unwrap();
    let read_only = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(true, &read_only);
    let out = run_git_gated("commit", &read_only, &StubGit, &mut gate).await;
    assert!(
        out.contains("committed"),
        "gate-granted commit should land: {out}"
    );
    assert!(
        gate.asks
            .iter()
            .any(|(t, k)| t == "git" && k.starts_with("git_write:commit")),
        "a git_write grant was requested: {:?}",
        gate.asks
    );
}

/// Deny-by-default invariant: a gate that DECLINES keeps the git write denied.
#[tokio::test]
async fn git_write_denied_when_operator_declines() {
    let ws = tempfile::TempDir::new().unwrap();
    let read_only = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(false, &read_only);
    let out = run_git_gated("commit", &read_only, &StubGit, &mut gate).await;
    assert!(
        out.contains("capability denied: git commit not permitted"),
        "a declined git write stays denied: {out}"
    );
}

/// #1056: a git READ is never gated — the arm only routes WRITE denials, so a
/// read op never even consults the gate.
#[tokio::test]
async fn git_read_is_never_gated() {
    let ws = tempfile::TempDir::new().unwrap();
    let read_only = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(false, &read_only);
    let out = run_git_gated("status", &read_only, &StubGit, &mut gate).await;
    assert!(
        out.contains("on branch main"),
        "read op runs ungated: {out}"
    );
    assert!(gate.asks.is_empty(), "a read must not consult the gate");
}

#[tokio::test]
async fn git_arm_unknown_op_is_an_error_not_a_panic() {
    let ws = tempfile::TempDir::new().unwrap();
    let out = run_git("frobnicate", &caveats_rw(ws.path()), Some(&StubGit)).await;
    assert!(
        out.contains("error:") && out.contains("unknown git op"),
        "got: {out}"
    );
}

#[tokio::test]
async fn git_arm_without_injection_is_unknown_tool() {
    let ws = tempfile::TempDir::new().unwrap();
    let out = run_git("status", &caveats_rw(ws.path()), None).await;
    assert!(out.contains("unknown tool: git"), "got: {out}");
}

// #479: the agent-callable crew/compose_roster tools route through the
// injected CrewRunner — same presence-gating + dispatch shape as `git`.
struct StubCrew;
#[async_trait::async_trait]
impl crate::agentic::CrewRunner for StubCrew {
    async fn dispatch(
        &self,
        op: &str,
        _args: &serde_json::Value,
        _caveats: &Caveats,
    ) -> Result<String, String> {
        match op {
            "compose_roster" => Ok("proposed roster: planner <- qwen3-coder:30b".to_string()),
            "crew" => Ok("crew ran: diff +1/-0, status PASS".to_string()),
            other => Err(format!("unknown op: {other}")),
        }
    }
}

async fn run_crew_tool(
    name: &str,
    args: serde_json::Value,
    crew: Option<&dyn crate::agentic::CrewRunner>,
) -> String {
    let ws = tempfile::TempDir::new().unwrap();
    execute_tool(
        name,
        &args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats_rw(ws.path()),
        &mut NoMcp,
        None, // build_check_cmd
        None, // note_sink
        None, // recall_source
        None, // memory_source
        None, // permission_gate
        None, // exec_floor
        None, // git_tool
        crew,
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await
}

#[tokio::test]
async fn crew_arm_dispatches_when_injected() {
    let out = run_crew_tool(
        "crew",
        serde_json::json!({ "task": "do X" }),
        Some(&StubCrew),
    )
    .await;
    assert!(
        out.contains("crew ran") && out.contains("PASS"),
        "got: {out}"
    );
    let out = run_crew_tool(
        "compose_roster",
        serde_json::json!({ "mode": "crew" }),
        Some(&StubCrew),
    )
    .await;
    assert!(out.contains("proposed roster"), "got: {out}");
}

/// #479 (G4): with no `CrewRunner` injected (the OFF default), the dispatch
/// arm coaches recovery instead of the old flat `unknown tool` dead-end — it
/// names the operator gesture (`NEWT_TEAM`) and a real solo alternative, and
/// must NOT read as "unknown tool".
#[tokio::test]
async fn crew_arm_without_injection_coaches_recovery() {
    for name in ["crew", "compose_roster"] {
        let out = run_crew_tool(name, serde_json::json!({ "task": "x" }), None).await;
        assert!(out.contains("NEWT_TEAM"), "{name}: {out}");
        assert!(out.contains("read_file"), "{name}: {out}");
        assert!(!out.contains("unknown tool"), "{name}: {out}");
    }
}

/// #479 (G4): the factored coach helper names the gate + a real alternative
/// and never reads as "unknown tool" — the regression point for the wording.
#[test]
fn crew_off_recovery_result_names_gate_and_alternative() {
    let out = crew_off_recovery_result("crew");
    assert!(out.contains("'crew'"), "{out}");
    assert!(out.contains("NEWT_TEAM"), "{out}");
    // A real, always-available solo alternative is offered.
    assert!(out.contains("write_file"), "{out}");
    assert!(!out.contains("unknown tool"), "{out}");
}

/// #479 (G4): the gated-off telemetry seam. A `crew`/`compose_roster` reach
/// with the surface OFF records a `GatedOff` phantom; the same names with the
/// surface ON record nothing (they dispatch normally), and a non-crew name is
/// never gated-off.
#[test]
fn classify_gated_off_reach_only_fires_for_off_crew_names() {
    for name in ["crew", "compose_roster"] {
        assert_eq!(
            classify_gated_off_reach(name, false),
            Some(crate::PhantomResolution::GatedOff(
                "crew/team surface off (NEWT_TEAM)".into()
            )),
            "{name} OFF should record GatedOff"
        );
        assert_eq!(
            classify_gated_off_reach(name, true),
            None,
            "{name} ON dispatches normally — no phantom"
        );
    }
    // A non-crew tool is never a gated-off reach, OFF or ON.
    assert_eq!(classify_gated_off_reach("read_file", false), None);
    assert_eq!(classify_gated_off_reach("read_file", true), None);
}

/// #479 (G4) guard: the OFF-state changes do not touch `is_hallucination`
/// (crew/compose_roster stay real names) or `classify_phantom_reach` for the
/// crew names — both kept exactly so the ON path stays a normal dispatch.
#[test]
fn crew_names_stay_real_and_unflagged_by_existing_seams() {
    for name in ["crew", "compose_roster"] {
        assert!(
            !is_hallucination(name, &serde_json::json!({ "task": "x" })),
            "{name} must stay a real tool name"
        );
        assert_eq!(
            classify_phantom_reach(name, &serde_json::json!({ "task": "x" }), "ok", true),
            None,
            "{name} must not be flagged by classify_phantom_reach"
        );
    }
}

// --- #496: the embedded `find` tool -----------------------------------

/// Convenience for `find` calls through the real dispatch under a
/// read-everything session.
async fn run_find(args: serde_json::Value, ws: &std::path::Path) -> String {
    run_tool("find", args, ws, &caveats_rw(ws), None).await
}

fn touch(root: &std::path::Path, rel: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, b"x").unwrap();
}

/// Regression for #496: an agent needed `find . -name pyo3_module.rs` but
/// the build's shell tool was unavailable. The embedded tool must locate the
/// file by basename, ignoring decoys, and return its workspace-relative path
/// (no shell, no `| sort`). Fails before this tool existed (`unknown tool:
/// find`).
#[tokio::test]
async fn find_locates_file_by_name_issue_496() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "newt-core/src/pyo3_module.rs");
    touch(ws.path(), "newt-data/src/other.rs");
    touch(ws.path(), "docs/pyo3_module.md"); // decoy: wrong extension
    let out = run_find(serde_json::json!({ "name": "pyo3_module.rs" }), ws.path()).await;
    assert_eq!(out, "newt-core/src/pyo3_module.rs", "got: {out}");
}

/// 2026-07-26 regression: "code files with the highest line counts" must
/// NOT rank AGENTS.md / Cargo.lock. `code: true` keeps language-pack
/// source only (same allowlist as nav gather).
#[tokio::test]
async fn find_code_true_excludes_docs_and_lockfiles_from_line_ranking() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("tall.rs"), "x\n".repeat(20)).unwrap();
    std::fs::write(ws.path().join("short.rs"), "x\n".repeat(5)).unwrap();
    std::fs::write(ws.path().join("AGENTS.md"), "d\n".repeat(200)).unwrap();
    std::fs::write(ws.path().join("Cargo.lock"), "l\n".repeat(100)).unwrap();
    std::fs::write(ws.path().join("LICENSE"), "L\n".repeat(50)).unwrap();
    let out = run_find(
        serde_json::json!({
            "path": ".",
            "type": "f",
            "code": true,
            "sort": "lines",
            "show_lines": true,
            "max_results": 10
        }),
        ws.path(),
    )
    .await;
    assert!(
        out.contains("20\ttall.rs") && out.contains("5\tshort.rs"),
        "code sources with line counts: {out}"
    );
    assert!(
        !out.contains("AGENTS.md") && !out.contains("Cargo.lock") && !out.contains("LICENSE"),
        "docs/lockfiles/LICENSE must be excluded: {out}"
    );
    let tall = out.find("20\ttall.rs").expect("tall first");
    let short = out.find("5\tshort.rs").expect("short second");
    assert!(tall < short, "lines descending: {out}");
}

/// The other call the blocked agent reached for:
/// `find examples -maxdepth 2 -type f -name '*.py'`. Exercises glob + type
/// filter + max_depth together, and confirms output is pre-sorted.
#[tokio::test]
async fn find_glob_type_and_maxdepth_together() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "examples/a.py"); // depth 1 — match
    touch(ws.path(), "examples/sub/b.py"); // depth 2 — match
    touch(ws.path(), "examples/sub/deep/c.py"); // depth 3 — too deep
    touch(ws.path(), "examples/readme.md"); // wrong extension
    std::fs::create_dir_all(ws.path().join("examples/empty_dir")).unwrap();
    let out = run_find(
        serde_json::json!({
            "path": "examples", "name": "*.py", "type": "f", "max_depth": 2
        }),
        ws.path(),
    )
    .await;
    // Pre-sorted, exactly the two in-depth .py files, no dir, no .md, no
    // depth-3 file — and no shell `| sort` needed.
    assert_eq!(out, "examples/a.py\nexamples/sub/b.py", "got: {out}");
}

/// `code` is a harness-owned semantic category: it includes source files
/// from every registered language pack and excludes docs/manifests/locks.
/// This real-filesystem test grounds the pure language-registry classifier.
#[tokio::test]
async fn find_source_category_filters_repository_metadata_across_languages() {
    let ws = tempfile::TempDir::new().unwrap();
    for file in [
        "src/main.rs",
        "src/app.py",
        "web/app.ts",
        "java/App.java",
        "native/app.cpp",
        "dotnet/App.cs",
        "ruby/app.rb",
        "scripts/build.sh",
        "AGENTS.md",
        "Cargo.toml",
        "Cargo.lock",
    ] {
        touch(ws.path(), file);
    }

    let out = run_find(
        serde_json::json!({ "category": "source", "type": "f" }),
        ws.path(),
    )
    .await;

    for source in [
        "src/main.rs",
        "src/app.py",
        "web/app.ts",
        "java/App.java",
        "native/app.cpp",
        "dotnet/App.cs",
        "ruby/app.rb",
        "scripts/build.sh",
    ] {
        assert!(
            out.lines().any(|line| line == source),
            "missing {source}: {out}"
        );
    }
    for metadata in ["AGENTS.md", "Cargo.toml", "Cargo.lock"] {
        assert!(
            !out.lines().any(|line| line == metadata),
            "metadata is not source code ({metadata}): {out}"
        );
    }
}

/// A named language narrows the generic source category through pack
/// aliases. The mocked tool schema and pure registry tests sit underneath;
/// this real walk proves the filter reaches filesystem behavior.
#[tokio::test]
async fn find_language_alias_narrows_source_files() {
    let ws = tempfile::TempDir::new().unwrap();
    for file in ["native/a.c", "native/b.cpp", "dotnet/App.cs", "src/main.rs"] {
        touch(ws.path(), file);
    }

    let cpp = run_find(serde_json::json!({ "language": "C++" }), ws.path()).await;
    assert_eq!(cpp, "native/a.c\nnative/b.cpp");
    let csharp = run_find(serde_json::json!({ "language": "C#" }), ws.path()).await;
    assert_eq!(csharp, "dotnet/App.cs");
}

/// Output is sorted ascending regardless of filesystem/creation order.
#[tokio::test]
async fn find_output_is_sorted() {
    let ws = tempfile::TempDir::new().unwrap();
    for f in ["m.txt", "a.txt", "z.txt", "c.txt"] {
        touch(ws.path(), f);
    }
    let out = run_find(serde_json::json!({ "name": "*.txt" }), ws.path()).await;
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        vec!["a.txt", "c.txt", "m.txt", "z.txt"],
        "got: {out}"
    );
}

/// #1235: every tool invocation goes through one display boundary. The
/// operator sees the command plus a bounded head+tail, while the
/// model-facing result remains complete.
///
/// #1973 declared amendment: this golden MOVED from tail-only
/// (c.rs/d.rs/e.rs) to head+tail (a.rs .. e.rs) — see the module doc on
/// `display::spill_view_lines`. This test's own property (the operator's
/// bounded excerpt vs. the model's complete result) is unaffected.
#[tokio::test]
async fn find_command_and_full_result_share_the_spill_boundary() {
    let ws = tempfile::TempDir::new().unwrap();
    for f in ["e.rs", "b.rs", "d.rs", "a.rs", "c.rs"] {
        touch(ws.path(), f);
    }
    let args = serde_json::json!({
        "path": ".",
        "name": "*.rs",
        "type": "f",
    });
    let caveats = caveats_rw(ws.path());
    let (out, rendered) = run_tool_captured("find", args, ws.path(), &caveats, &mut NoMcp).await;

    assert_eq!(out, "a.rs\nb.rs\nc.rs\nd.rs\ne.rs");
    assert_eq!(
        rendered,
        "⚙  find: . (name=*.rs, type=f)\n\
             ▒ a.rs\n\
             ▲ 3 lines hidden  [/spill N raises this view]\n\
             ▓ e.rs\n\
             …\n"
    );
}

#[tokio::test]
async fn routed_find_uses_the_governed_tool_in_the_audit_header() {
    let ws = tempfile::TempDir::new().unwrap();
    for f in ["b.rs", "a.rs"] {
        touch(ws.path(), f);
    }
    let caveats = caveats_rw(ws.path());
    let (out, rendered) = run_tool_captured(
        "run_command",
        serde_json::json!({"command": "find . -name '*.rs' -type f"}),
        ws.path(),
        &caveats,
        &mut NoMcp,
    )
    .await;

    assert_eq!(out, "a.rs\nb.rs");
    assert!(
        rendered.starts_with("⚙  find: . (name=*.rs, type=f)\n"),
        "routed action was not audited canonically: {rendered}"
    );
    assert!(!rendered.contains("⚙  run_command:"));
}

#[tokio::test]
async fn correction_alias_header_never_echoes_file_content() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let secret = "PRIVATE_BODY_MUST_NOT_APPEAR_IN_HEADER";
    let (out, rendered) = run_tool_captured(
        "create_file",
        serde_json::json!({"path": "secret.txt", "content": secret}),
        ws.path(),
        &caveats,
        &mut NoMcp,
    )
    .await;

    assert!(out.contains("write_file"), "got: {out}");
    assert!(
        rendered.starts_with(&format!(
            "⚙  create_file: secret.txt ({} bytes)\n",
            secret.len()
        )),
        "unsafe or unhelpful alias audit: {rendered}"
    );
    assert!(!rendered.contains(secret));
}

#[test]
fn lifecycle_audit_names_the_resolved_command() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(
        ws.path().join("Cargo.toml"),
        "[package]\nname='audit-fixture'\n",
    )
    .unwrap();
    let (name, detail) = tool_presentation(
        "lifecycle",
        &serde_json::json!({"phase": "test", "action": "run"}),
        ws.path(),
    );
    let resolved = crate::tooling::resolved_phase_commands(ws.path(), crate::tooling::Phase::Test);

    assert_eq!(name, "lifecycle");
    assert!(!resolved.is_empty());
    assert_eq!(detail, format!("test (run) → {}", resolved.join(" && ")));
}

#[test]
fn audit_preserves_whitespace_in_real_paths() {
    let ws = tempfile::TempDir::new().unwrap();
    let (name, detail) = tool_presentation(
        "read_file",
        &serde_json::json!({"path": " leading and trailing "}),
        ws.path(),
    );

    assert_eq!(name, "read_file");
    assert_eq!(detail, " leading and trailing ");

    let (name, detail) = tool_presentation(
        "run_command",
        &serde_json::json!({"command": "cd nested && printf exact-command"}),
        ws.path(),
    );
    assert_eq!(name, "run_command");
    assert_eq!(detail, "cd nested && printf exact-command");
}

#[tokio::test]
async fn find_error_uses_the_same_spill_boundary() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let (out, rendered) = run_tool_captured(
        "find",
        serde_json::json!({"path": "missing", "name": "*.rs", "type": "f"}),
        ws.path(),
        &caveats,
        &mut NoMcp,
    )
    .await;

    assert_eq!(out, "error: no such path 'missing'");
    assert_eq!(
        rendered,
        "⚙  find: missing (name=*.rs, type=f)\n\
             ▒ error: no such path 'missing'\n\
             …\n"
    );
}

struct EmptyRemote;

#[async_trait::async_trait]
impl McpTools for EmptyRemote {
    fn handles(&self, name: &str) -> bool {
        name == "test__get_empty"
    }

    fn tool_defs(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }

    async fn call(&mut self, _leased: &LeasedMcpCall<'_>) -> String {
        String::new()
    }
}

#[tokio::test]
async fn empty_tool_result_still_commits_a_complete_spill_block() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let (out, rendered) = run_tool_captured(
        "test__get_empty",
        serde_json::json!({}),
        ws.path(),
        &caveats,
        &mut EmptyRemote,
    )
    .await;

    assert!(out.is_empty());
    assert_eq!(
        rendered,
        "⚙  test__get_empty: {}\n\
             ▒ (no output)\n\
             …\n"
    );
}

#[tokio::test]
async fn unknown_tool_has_exactly_one_complete_audit_block() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let (out, rendered) = run_tool_captured(
        "definitely_unknown",
        serde_json::json!({}),
        ws.path(),
        &caveats,
        &mut NoMcp,
    )
    .await;

    assert!(out.contains("unknown tool"), "got: {out}");
    assert_eq!(rendered.matches("⚙  definitely_unknown:").count(), 1);
    assert_eq!(rendered.matches("…\n").count(), 1);
    assert_eq!(rendered.matches("▒ unknown tool:").count(), 1);
}

#[tokio::test]
async fn pre_set_cancellation_closes_the_block_without_polling_a_mutation() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let args = serde_json::json!({"path": "must-not-exist.txt", "content": "blocked"});
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 3, false);

    let out = execute_tool_with_display_cancellable(
        &mut display,
        "write_file",
        &args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        ToolCollaborators::default(),
        false,
        PromptDisposition::Act,
        Some(&cancel),
    )
    .await;

    assert!(out.is_none());
    assert!(!ws.path().join("must-not-exist.txt").exists());
    assert_eq!(
        String::from_utf8(display.into_inner()).unwrap(),
        "⚙  write_file: must-not-exist.txt (7 bytes)\n\
             ▒ error: write_file interrupted — tool cancelled before completion\n\
             …\n"
    );
}

#[tokio::test]
async fn prompt_read_central_display_never_echoes_recovered_prompt_text() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let exact = "operator secret that must reach only the model";
    let context = PromptReadContext::new(None, exact, None);
    let (out, rendered) = run_tool_captured_with_context(
        "prompt_read",
        serde_json::json!({}),
        ws.path(),
        &caveats,
        &mut NoMcp,
        Some(context),
        None,
    )
    .await;

    let model: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(model["model_text"], exact);
    assert_eq!(rendered.matches("⚙  prompt_read:").count(), 1);
    assert!(rendered.contains("ephemeral prompt: returned"));
    assert!(!rendered.contains(exact));
}

#[tokio::test]
async fn artifact_read_central_display_never_echoes_recovered_body() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let prompt = PromptId::new();
    let secret = "artifact body that must reach only the model";
    let store = SessionArtifactStore::new("central-display-test").unwrap();
    let record = store
        .append_artifact(
            prompt,
            prompt,
            NewPromptArtifact::new(ArtifactKind::Decision, ArtifactRelation::DerivedFrom)
                .with_body(secret),
        )
        .unwrap();
    let context = ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), Some(&store));
    let (out, rendered) = run_tool_captured_with_context(
        "artifact_read",
        serde_json::json!({"address": record.id.to_string()}),
        ws.path(),
        &caveats,
        &mut NoMcp,
        None,
        Some(context),
    )
    .await;

    let model: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(model["artifact"]["body"], secret);
    assert_eq!(rendered.matches("⚙  artifact_read:").count(), 1);
    assert!(rendered.contains(&format!(
        "returned {} of {} body characters",
        secret.chars().count(),
        secret.chars().count()
    )));
    assert!(!rendered.contains(secret));
}

#[tokio::test]
async fn render_report_has_one_header_document_and_ack_block() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let (out, rendered) = run_tool_captured(
        "render_report",
        serde_json::json!({
            "title": "Build status",
            "body": "All required checks passed."
        }),
        ws.path(),
        &caveats,
        &mut NoMcp,
    )
    .await;

    assert!(out.starts_with("report rendered:"), "got: {out}");
    assert_eq!(rendered.matches("⚙  render_report:").count(), 1);
    assert_eq!(rendered.matches("All required checks passed.").count(), 1);
    assert_eq!(rendered.matches("▒ report rendered:").count(), 1);
}

/// `type` restricts to files or directories.
#[tokio::test]
async fn find_type_filter() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "pkg/file.rs");
    std::fs::create_dir_all(ws.path().join("pkg/sub")).unwrap();
    let dirs = run_find(serde_json::json!({ "type": "d" }), ws.path()).await;
    assert!(
        dirs.contains("pkg") && dirs.contains("pkg/sub"),
        "got: {dirs}"
    );
    assert!(!dirs.contains("file.rs"), "dirs-only leaked a file: {dirs}");
    let files = run_find(serde_json::json!({ "type": "f" }), ws.path()).await;
    assert!(files.contains("pkg/file.rs"), "got: {files}");
    assert!(
        !files.lines().any(|l| l == "pkg" || l == "pkg/sub"),
        "files-only leaked a dir: {files}"
    );
}

/// .gitignore + the default build/dep skips are honoured by default and
/// can be disabled with `respect_gitignore=false`.
#[tokio::test]
async fn find_gitignore_and_default_skips() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join(".gitignore"), "ignored.txt\n").unwrap();
    touch(ws.path(), "kept.txt");
    touch(ws.path(), "ignored.txt");
    touch(ws.path(), "target/build_artifact.txt");
    touch(ws.path(), "node_modules/dep.txt");

    let on = run_find(serde_json::json!({ "name": "*.txt" }), ws.path()).await;
    assert!(on.contains("kept.txt"), "got: {on}");
    assert!(!on.contains("ignored.txt"), "gitignore not honoured: {on}");
    assert!(!on.contains("target/"), "target not skipped: {on}");
    assert!(
        !on.contains("node_modules/"),
        "node_modules not skipped: {on}"
    );

    let off = run_find(
        serde_json::json!({ "name": "*.txt", "respect_gitignore": false }),
        ws.path(),
    )
    .await;
    assert!(off.contains("ignored.txt"), "opt-out should show it: {off}");
    assert!(off.contains("target/build_artifact.txt"), "got: {off}");
}

/// `max_results` caps output and the result notes the truncation.
#[tokio::test]
async fn find_max_results_caps_and_notes_truncation() {
    let ws = tempfile::TempDir::new().unwrap();
    for i in 0..10 {
        touch(ws.path(), &format!("f{i}.txt"));
    }
    let out = run_find(
        serde_json::json!({ "name": "*.txt", "max_results": 3 }),
        ws.path(),
    )
    .await;
    let body: Vec<&str> = out.lines().filter(|l| l.ends_with(".txt")).collect();
    assert_eq!(body.len(), 3, "should cap at 3: {out}");
    assert!(out.contains("truncated at 3"), "got: {out}");
}

/// A missing root is a clear error, and an empty match set says so.
#[tokio::test]
async fn find_missing_root_and_no_matches() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.txt");
    let missing = run_find(serde_json::json!({ "path": "does/not/exist" }), ws.path()).await;
    assert!(missing.starts_with("error:"), "got: {missing}");
    let empty = run_find(serde_json::json!({ "name": "*.nope" }), ws.path()).await;
    assert_eq!(empty, "no matches", "got: {empty}");
}

/// fs_read denial: no scope + no prompt gate ⇒ capability denied (same UX
/// as list_dir/read_file).
#[tokio::test]
async fn find_denied_without_fs_read() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "secret.txt");
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "find",
        serde_json::json!({ "name": "*" }),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert!(out.starts_with("capability denied"), "got: {out}");
}

/// A `..` root that escapes the workspace is refused even when the session
/// grants fs_read everywhere (defence-in-depth for a recursive read).
#[tokio::test]
async fn find_refuses_root_outside_workspace() {
    let parent = tempfile::TempDir::new().unwrap();
    std::fs::write(parent.path().join("outside.txt"), b"x").unwrap();
    let ws = parent.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // fs_read: All, so the only thing that can stop the escape is the
    // canonical-root containment check.
    let out = run_find(serde_json::json!({ "path": ".." }), &ws).await;
    assert!(out.starts_with("capability denied"), "got: {out}");
}

/// An empty `name` is treated as "match everything" (the `!g.is_empty()`
/// guard routes `Some("")` to the no-filter path; without it the glob would
/// compile to `^$` and match nothing).
#[tokio::test]
async fn find_empty_name_matches_everything() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.txt");
    touch(ws.path(), "sub/b.rs");
    let out = run_find(serde_json::json!({ "name": "" }), ws.path()).await;
    for expected in ["a.txt", "sub", "sub/b.rs"] {
        assert!(
            out.lines().any(|l| l == expected),
            "empty name should match `{expected}`: {out}"
        );
    }
}

/// Hidden entries (dotfiles / dotdirs) are pruned by default and surface
/// only when `respect_gitignore=false` — relevant because dotfiles can hold
/// secrets (.env, .ssh). Pins the `.hidden(respect_gitignore)` branch.
#[tokio::test]
async fn find_hidden_entries_gated_by_respect_gitignore() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "visible.txt");
    touch(ws.path(), ".hidden.txt");
    touch(ws.path(), ".config/secret.txt");

    let default = run_find(serde_json::json!({ "name": "*" }), ws.path()).await;
    assert!(
        default.lines().any(|l| l == "visible.txt"),
        "got: {default}"
    );
    assert!(
        !default.contains(".hidden") && !default.contains(".config"),
        "hidden entries must be skipped by default: {default}"
    );

    let all = run_find(
        serde_json::json!({ "name": "*", "respect_gitignore": false }),
        ws.path(),
    )
    .await;
    assert!(all.contains(".hidden.txt"), "opt-out should show it: {all}");
    assert!(all.contains(".config/secret.txt"), "got: {all}");
}

/// Security boundary: `find` never follows symlinked directories, so a link
/// pointing outside the workspace cannot leak the target's contents (pins
/// `.follow_links(false)`). Unix-only — Windows symlinks need privileges.
#[cfg(unix)]
#[tokio::test]
async fn find_does_not_follow_symlinks_out_of_workspace() {
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"x").unwrap();
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "inside.txt");
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    // The symlink is present but is NOT descended into.
    let leaked = run_find(serde_json::json!({ "name": "secret.txt" }), ws.path()).await;
    assert_eq!(
        leaked, "no matches",
        "symlink was followed out of ws: {leaked}"
    );
    // Sanity: a real in-workspace file is still found.
    let found = run_find(serde_json::json!({ "name": "inside.txt" }), ws.path()).await;
    assert_eq!(found, "inside.txt", "got: {found}");
}

#[test]
fn glob_to_regex_anchors_and_escapes() {
    // '*' is a wildcard; '.' is literal (not "any char").
    let re = glob_to_regex("*.py", true).unwrap();
    assert!(re.is_match("foo.py"));
    assert!(!re.is_match("foo.pyc")); // anchored at end
    assert!(!re.is_match("fooxpy")); // '.' is literal
                                     // Exact basename, '?' = single char, case-sensitivity honoured.
    assert!(glob_to_regex("a?c", true).unwrap().is_match("abc"));
    assert!(!glob_to_regex("a?c", true).unwrap().is_match("ac"));
    assert!(glob_to_regex("readme.md", false)
        .unwrap()
        .is_match("README.MD"));
    assert!(!glob_to_regex("readme.md", true)
        .unwrap()
        .is_match("README.MD"));
}

async fn run_tool(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    build_check: Option<&str>,
) -> String {
    execute_tool(
        name,
        &args,
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        build_check,
        None,
        None,
        None, // memory_source
        None,
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await
}

async fn run_tool_captured(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    mcp: &mut dyn McpTools,
) -> (String, String) {
    run_tool_captured_with_context(name, args, ws, caveats, mcp, None, None).await
}

async fn run_tool_captured_with_context(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    mcp: &mut dyn McpTools,
    prompt_context: Option<PromptReadContext<'_>>,
    artifact_context: Option<ArtifactReadContext<'_>>,
) -> (String, String) {
    run_tool_captured_with_context_and_live(
        name,
        args,
        ws,
        caveats,
        mcp,
        prompt_context,
        artifact_context,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_tool_captured_with_context_and_live(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    mcp: &mut dyn McpTools,
    prompt_context: Option<PromptReadContext<'_>>,
    artifact_context: Option<ArtifactReadContext<'_>>,
    live_tool_output: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
) -> (String, String) {
    let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 3, false);
    // Mechanics helper: authorize the tool it is told to run (the MCP-auth
    // tests use `run_remote_gated` instead). Post the `mcp-under-leash`
    // name-grant closure an MCP call needs a structural grant; for a built-in
    // tool `persona_tools` doesn't gate dispatch, so this is a no-op there.
    let persona_grant = [name.to_string()];
    let out = execute_tool_with_display_cancellable(
        &mut display,
        name,
        &args,
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        mcp,
        ToolCollaborators {
            prompt_context,
            artifact_context,
            live_tool_output,
            persona_tools: Some(&persona_grant),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .expect("uncancelled test dispatch should complete");
    let rendered = String::from_utf8(display.into_inner()).unwrap();
    (out, rendered)
}

#[cfg(not(windows))]
#[tokio::test]
async fn live_shell_observation_does_not_change_headless_completion_bytes() {
    #[derive(Default)]
    struct CapturedLiveOutput {
        events: std::sync::Mutex<Vec<String>>,
    }
    impl crate::agentic::LiveToolOutput for CapturedLiveOutput {
        fn start(&self, _generation: u64) {
            self.events.lock().unwrap().push("start".into());
        }
        fn write(&self, _generation: u64, _stream: crate::agentic::ToolOutputStream, chunk: &[u8]) {
            self.events
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(chunk).into_owned());
        }
        fn finish(&self, _generation: u64) {
            self.events.lock().unwrap().push("finish".into());
        }
        fn abandon(&self, _generation: u64) {
            self.events.lock().unwrap().push("abandon".into());
        }
    }

    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats {
        exec: crate::caveats::Scope::only(["echo".to_string()]),
        ..caveats_rw(ws.path())
    };
    let args = serde_json::json!({"command": "echo byte-stable"});
    let (headless_out, headless_rendered) = run_tool_captured_with_context_and_live(
        "run_command",
        args.clone(),
        ws.path(),
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
    )
    .await;
    let sink = std::sync::Arc::new(CapturedLiveOutput::default());
    let (live_out, live_rendered) = run_tool_captured_with_context_and_live(
        "run_command",
        args,
        ws.path(),
        &caveats,
        &mut NoMcp,
        None,
        None,
        Some(sink.clone()),
    )
    .await;

    assert_eq!(live_out, headless_out);
    assert_eq!(live_rendered.as_bytes(), headless_rendered.as_bytes());
    assert!(
        !headless_rendered.as_bytes().contains(&0x1b),
        "headless completion emitted cursor-control bytes: {headless_rendered:?}"
    );
    let events = sink.events.lock().unwrap();
    assert_eq!(events.first().map(String::as_str), Some("start"));
    assert_eq!(events.last().map(String::as_str), Some("finish"));
    assert!(
        events.iter().any(|event| event.contains("byte-stable")),
        "live events: {events:?}; model output: {live_out:?}"
    );
}

#[tokio::test]
async fn edit_file_replaces_unique_match_and_reports_delta() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("f.txt"), "hello world\nsecond line\n").unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "edit_file",
        serde_json::json!({
            "path": "f.txt",
            "old_string": "world",
            "new_string": "rust\nand more"
        }),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.starts_with("edited f.txt (+1 lines"), "got: {out}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("f.txt")).unwrap(),
        "hello rust\nand more\nsecond line\n"
    );
}

#[tokio::test]
async fn edit_file_rejects_empty_missing_and_ambiguous_old_string() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("f.txt"), "dup\ndup\n").unwrap();
    let caveats = caveats_rw(ws.path());

    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "", "new_string": "x"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("old_string must not be empty"), "got: {out}");

    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "absent", "new_string": "x"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("old_string not found in f.txt"), "got: {out}");
    // The miss error now shows the file's actual contents so the model can
    // copy the exact text instead of blind-guessing old_string again.
    assert!(out.contains("do not guess again"), "got: {out}");
    assert!(
        out.contains("dup"),
        "miss error must include the file content: {out}"
    );

    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "dup", "new_string": "x"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("matches 2 locations"), "got: {out}");
    // The ambiguous edit must NOT have touched the file.
    assert_eq!(
        std::fs::read_to_string(ws.path().join("f.txt")).unwrap(),
        "dup\ndup\n"
    );
}

#[tokio::test]
async fn edit_file_denied_outside_fs_write_scope_and_missing_file() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "a", "new_string": "b"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(
        out.contains("capability denied: fs_write"),
        "denied before any fs access, got: {out}"
    );

    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "missing.txt", "old_string": "a", "new_string": "b"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("error reading missing.txt"), "got: {out}");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn edit_file_symlink_under_workspace_escaping_is_denied() {
    // step-52.5: under a CONFINED fs_write, a symlink UNDER the workspace
    // pointing outside must not let edit_file read OR write the outside file.
    // Both the read of `existing` (which could leak the outside head on a
    // no-match) and the write are object-bound; the outside file is unchanged
    // and its contents never appear in the output. Verified red→green.
    let ws = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "OUTSIDE SECRET\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    let out = run_tool(
        "edit_file",
        serde_json::json!({
            "path": "link/secret.txt",
            "old_string": "OUTSIDE",
            "new_string": "EDITED",
        }),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
    )
    .await;

    assert!(
        !out.contains("OUTSIDE SECRET"),
        "object-bound edit must not leak the outside file: {out}"
    );
    assert_eq!(
        out,
        denied_fs_result("fs_write", "link/secret.txt"),
        "the symlink-escape edit must be denied: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
        "OUTSIDE SECRET\n",
        "the outside file must be UNCHANGED"
    );
}

#[tokio::test]
async fn edit_file_appends_build_check_result() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("f.txt"), "old\n").unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "old", "new_string": "new"}),
        ws.path(),
        &caveats,
        Some(passing_build_check_cmd()),
    )
    .await;
    // build_check runs CONFINED (P4). On Linux+Landlock the check runs and
    // its outcome is reflected; off it (e.g. Windows without the AppContainer
    // launcher) it fails closed — either way the tool APPENDS a build-check
    // line, which is what this test guards.
    let confinable = crate::confined_exec::kernel_fs_fence_available();
    if confinable {
        assert!(out.contains("✓ build check passed"), "got: {out}");
    } else {
        assert!(
            out.contains("build check"),
            "build-check line appended: {out}"
        );
    }

    let failing_check = failing_build_check_cmd("broke");
    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "new", "new_string": "newer"}),
        ws.path(),
        &caveats,
        Some(&failing_check),
    )
    .await;
    if confinable {
        assert!(out.contains("✗ build check failed"), "got: {out}");
        assert!(out.contains("broke"), "model sees the failure text: {out}");
    } else {
        assert!(
            out.contains("build check"),
            "build-check line appended: {out}"
        );
    }
}

#[tokio::test]
async fn write_file_shrink_guard_refuses_large_deletion() {
    let ws = tempfile::TempDir::new().unwrap();
    let big: String = (0..100).map(|i| format!("line {i}\n")).collect();
    std::fs::write(ws.path().join("big.txt"), &big).unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "write_file",
        serde_json::json!({"path": "big.txt", "content": "tiny\n"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(
        out.contains("would shrink big.txt from 100 → 1 lines"),
        "got: {out}"
    );
    assert!(out.contains("edit_file"), "points at the safer tool: {out}");
    // The guard refused — the original file must be intact.
    assert_eq!(
        std::fs::read_to_string(ws.path().join("big.txt")).unwrap(),
        big
    );
}

#[tokio::test]
async fn write_file_creates_parent_directories() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "write_file",
        serde_json::json!({"path": "a/b/c.txt", "content": "nested"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.starts_with("wrote a/b/c.txt"), "got: {out}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("a/b/c.txt")).unwrap(),
        "nested"
    );
}

#[tokio::test]
async fn delete_file_removes_one_file_and_appends_build_check() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("old.rs"), "fn main() {}\n").unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "old.rs"}),
        ws.path(),
        &caveats,
        Some(passing_build_check_cmd()),
    )
    .await;
    assert!(out.starts_with("deleted old.rs"), "got: {out}");
    // Confined build_check (P4): outcome-checked on Linux+Landlock, else the
    // fail-closed line still counts as an appended build-check result.
    if crate::confined_exec::kernel_fs_fence_available() {
        assert!(out.contains("✓ build check passed"), "got: {out}");
    } else {
        assert!(
            out.contains("build check"),
            "build-check line appended: {out}"
        );
    }
    assert!(
        !ws.path().join("old.rs").exists(),
        "delete_file must remove the target file"
    );
}

#[tokio::test]
async fn delete_file_records_digest_to_absent_transition() {
    let ws = tempfile::TempDir::new().unwrap();
    let original = b"retired implementation\n";
    std::fs::write(ws.path().join("old.rs"), original).unwrap();
    let sink = RecordingArtifactSink::default();

    let out = run_artifact_tool(
        "delete_file",
        serde_json::json!({"path": "old.rs"}),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
        &sink,
    )
    .await;

    assert!(out.starts_with("deleted old.rs"), "got: {out}");
    let artifact = sink.only_artifact();
    assert_eq!(artifact.kind(), ArtifactKind::FileChange);
    assert_eq!(artifact.locator(), Some("old.rs"));
    assert_eq!(artifact.metadata()["operation"], "delete_file");
    assert_eq!(artifact.metadata()["before"]["available"], true);
    assert_eq!(artifact.metadata()["before"]["exists"], true);
    assert_eq!(
        artifact.metadata()["before"]["digest"],
        blake3::hash(original).to_hex().to_string()
    );
    assert_eq!(artifact.metadata()["after"]["available"], true);
    assert_eq!(artifact.metadata()["after"]["exists"], false);
    assert!(artifact.metadata()["after"]["digest"].is_null());
}

#[tokio::test]
async fn write_only_authority_does_not_record_a_preimage_digest() {
    let ws = tempfile::TempDir::new().unwrap();
    let original = b"secret preimage\n";
    let replacement = b"public result\n";
    std::fs::write(ws.path().join("state.txt"), original).unwrap();
    let caveats = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let sink = RecordingArtifactSink::default();

    let out = run_artifact_tool(
        "write_file",
        serde_json::json!({
            "path": "state.txt",
            "content": std::str::from_utf8(replacement).unwrap(),
        }),
        ws.path(),
        &caveats,
        None,
        &sink,
    )
    .await;

    assert!(out.starts_with("wrote state.txt"), "got: {out}");
    let artifact = sink.only_artifact();
    assert_eq!(artifact.metadata()["before"]["available"], false);
    assert_eq!(
        artifact.metadata()["before"]["reason"],
        "fs_read_not_granted"
    );
    assert!(artifact.metadata()["before"].get("digest").is_none());
    assert_eq!(
        artifact.metadata()["after"]["digest"],
        blake3::hash(replacement).to_hex().to_string()
    );
    assert!(
        !artifact
            .metadata()
            .to_string()
            .contains(&blake3::hash(original).to_hex().to_string()),
        "the preimage digest must not become a persistent read oracle"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn build_check_mutation_is_not_recorded_as_the_governed_write_postimage() {
    let ws = tempfile::TempDir::new().unwrap();
    let governed = b"governed bytes\n";
    let build_hook = b"build-hook bytes\n";
    let sink = RecordingArtifactSink::default();

    let out = run_artifact_tool(
        "write_file",
        serde_json::json!({
            "path": "target.txt",
            "content": std::str::from_utf8(governed).unwrap(),
        }),
        ws.path(),
        &caveats_rw(ws.path()),
        Some("printf 'build-hook bytes\\n' > target.txt"),
        &sink,
    )
    .await;

    assert!(out.contains("build check passed"), "got: {out}");
    assert_eq!(
        std::fs::read(ws.path().join("target.txt")).unwrap(),
        build_hook
    );
    let artifact = sink.only_artifact();
    assert_eq!(artifact.metadata()["operation"], "write_file");
    assert_eq!(
        artifact.metadata()["after"]["digest"],
        blake3::hash(governed).to_hex().to_string(),
        "the artifact must describe the tool's immediate verified write"
    );
    assert_ne!(
        artifact.metadata()["after"]["digest"],
        blake3::hash(build_hook).to_hex().to_string(),
        "a later build hook mutation must not be attributed to write_file"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn physical_symlink_escape_write_is_denied_object_bound() {
    // step-52.4 (#522 closure for write_file): a symlink UNDER the workspace
    // pointing outside no longer lets a CONFINED write escape. Object-bound
    // via openat2(RESOLVE_BENEATH), so the create is refused, the outside file
    // is untouched, and no artifact is minted. BEFORE object-binding this
    // mutated the outside file under the lexical policy — the named residual;
    // this test is that residual, flipped from "mutates" to "denied".
    let ws = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("target.txt"), "outside before\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();
    let sink = RecordingArtifactSink::default();

    let out = run_artifact_tool(
        "write_file",
        serde_json::json!({
            "path": "link/target.txt",
            "content": "outside after\n",
        }),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
        &sink,
    )
    .await;

    assert_eq!(
        out,
        denied_fs_result("fs_write", "link/target.txt"),
        "the symlink-escape write must be denied by the object fence: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(outside.path().join("target.txt")).unwrap(),
        "outside before\n",
        "the outside file must be UNCHANGED — the write never escaped"
    );
    assert!(sink.is_empty(), "a denied write records no artifact");
}

#[tokio::test]
async fn delete_file_denies_missing_files_directories_and_fs_write_misses() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
    std::fs::create_dir(ws.path().join("dir")).unwrap();

    let denied = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert!(out.contains("capability denied: fs_write"), "got: {out}");
    assert!(
        ws.path().join("secret.txt").exists(),
        "denied delete must not remove the file"
    );

    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "missing.txt"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("file does not exist"), "got: {out}");

    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "dir"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("refuses directories"), "got: {out}");
    assert!(ws.path().join("dir").is_dir(), "directory must remain");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn delete_file_symlink_under_workspace_escaping_is_denied() {
    // step-52.6: under a CONFINED fs_write, a symlink UNDER the workspace
    // pointing outside must not let delete_file remove the outside file.
    // Object-bound via `unlinkat` on the resolved parent — the escape is
    // refused and the outside file survives. Before the rewire `remove_file`
    // followed the intermediate symlink and deleted outside. Verified
    // red→green.
    let ws = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("victim.txt"), "keep me\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "link/victim.txt"}),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
    )
    .await;

    assert_eq!(
        out,
        denied_fs_result("fs_write", "link/victim.txt"),
        "the symlink-escape delete must be denied: {out}"
    );
    assert!(
        outside.path().join("victim.txt").exists(),
        "the outside file must survive — the delete never escaped"
    );
}

#[tokio::test]
async fn read_file_denial_and_missing_file_errors() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert!(out.contains("capability denied: fs_read"), "got: {out}");

    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "nope.txt"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("error reading nope.txt"), "got: {out}");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn read_file_symlink_under_workspace_escaping_is_denied() {
    // step-52.2 (fs-canonical-containment / #522): under a CONFINED fs_read
    // (Only{ws}, not All), a symlink UNDER the workspace whose target is
    // outside it must not let read_file exfiltrate the outside file — even
    // though the lexical gate admits the name `link/secret.txt`. The read is
    // object-bound through `WorkspaceDir` (openat2 RESOLVE_BENEATH), so the
    // escape is refused by the kernel. Before the rewire this returned the
    // secret — the named residual. Real-fs tier (grounds the object gate);
    // Linux-only (openat2).
    let ws = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "TOP SECRET").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    let confined = Caveats {
        fs_read: Scope::only([ws.path().to_string_lossy().into_owned()]),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "link/secret.txt"}),
        ws.path(),
        &confined,
        None,
    )
    .await;

    assert!(
        !out.contains("TOP SECRET"),
        "object-bound read must not follow a symlink out of the workspace: {out}"
    );
    assert_eq!(
        out,
        denied_fs_result("fs_read", "link/secret.txt"),
        "a contained-read escape must surface as an fs_read denial: {out}"
    );
}

#[tokio::test]
async fn list_dir_denial_and_missing_dir_errors() {
    let ws = tempfile::TempDir::new().unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "list_dir",
        serde_json::json!({"path": "."}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert!(out.contains("capability denied: fs_read"), "got: {out}");

    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "list_dir",
        serde_json::json!({"path": "not-a-dir"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.starts_with("error:"), "got: {out}");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn list_dir_symlink_under_workspace_escaping_is_denied() {
    // step-52.3: object-bound listing. Under a CONFINED fs_read (Only{ws}), a
    // symlink UNDER the workspace pointing to an outside directory must not
    // let list_dir enumerate the outside dir — even though the lexical gate
    // admits the name `link`. Before the rewire the outside entries were
    // listed (the #522 residual). Real-fs tier; Linux-only.
    let ws = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("outside_secret.txt"), "x").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    let confined = Caveats {
        fs_read: Scope::only([ws.path().to_string_lossy().into_owned()]),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "list_dir",
        serde_json::json!({"path": "link"}),
        ws.path(),
        &confined,
        None,
    )
    .await;

    assert!(
        !out.contains("outside_secret.txt"),
        "object-bound list_dir must not enumerate a directory outside the workspace: {out}"
    );
    assert_eq!(out, denied_fs_result("fs_read", "link"), "got: {out}");
}

#[tokio::test]
async fn unknown_tool_name_is_reported_not_executed() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "definitely_not_a_tool",
        serde_json::json!({}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    // Step 27.1: the bare "unknown tool: X" is now a corrective message that
    // still leads with the same prefix but also names the real catalog.
    assert!(
        out.starts_with("unknown tool: definitely_not_a_tool"),
        "got: {out}"
    );
    assert!(out.contains("Available tools include:"), "got: {out}");
}

// -- Step 27.1: tool-alias resolution + corrective feedback -------------

#[test]
fn alias_rewrites_shell_names_to_run_command() {
    for n in [
        "execute",
        "exec",
        "bash",
        "shell",
        "sh",
        "zsh",
        "terminal",
        "run_shell_command",
        "shell_command",
        "system",
    ] {
        assert!(
            matches!(
                resolve_tool_alias(n),
                Some(AliasOutcome::Rewrite("run_command"))
            ),
            "{n} should rewrite to run_command"
        );
    }
}

#[test]
fn alias_corrects_edit_and_create_names() {
    for n in [
        "str_replace_editor",
        "str_replace",
        "apply_patch",
        "edit",
        "replace_in_file",
    ] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("edit_file"), "{n}: {msg}");
        assert!(msg.contains("write_file"), "{n}: {msg}");
    }
    for n in ["create_file", "new_file", "touch"] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("write_file"), "{n}: {msg}");
    }
    for n in ["remove_file", "delete", "remove", "unlink", "rm_file"] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("delete_file"), "{n}: {msg}");
        assert!(msg.contains("fs_write"), "{n}: {msg}");
    }
}

#[test]
fn alias_coaches_mkdir_to_write_file() {
    // #721: newt has no directory-creation tool — coach to write_file, which
    // does create_dir_all on the parent. Turns the issue's `mkdir -p …/src`
    // dead-end into a self-correcting tool call.
    for n in [
        "mkdir",
        "make_dir",
        "makedirs",
        "mkdirs",
        "create_dir",
        "create_directory",
    ] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("write_file"), "{n}: {msg}");
        assert!(msg.contains("create_dir_all"), "{n}: {msg}");
    }
    // `touch` is intentionally NOT in the mkdir arm — it stays a create-file
    // alias (→ write_file), so there is no duplicate match arm / collision.
    let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias("touch") else {
        panic!("touch should still be a create-file Correct outcome");
    };
    assert!(msg.contains("write_file"), "touch: {msg}");
}

#[test]
fn alias_passes_through_real_and_mcp_names() {
    for n in [
        "run_command",
        "read_file",
        "write_file",
        "edit_file",
        "delete_file",
        "git",
        "update_plan",
        "plan_get",
        "server__do_thing",
    ] {
        assert!(
            resolve_tool_alias(n).is_none(),
            "{n} must dispatch unchanged"
        );
    }
}

// -- #716: plan / plan-read / crew / workflow alias families --------------

#[test]
fn alias_corrects_plan_names_to_update_plan() {
    // #1193: enter_plan_mode / exit_plan_mode are now REAL tools (a
    // read-only plan phase), so they no longer coach to update_plan — they
    // dispatch. The plan-CONTENT verbs still coach to update_plan.
    for n in [
        "make_plan",
        "create_plan",
        "plan",
        "planning",
        "todo",
        "todos",
        "todo_write",
    ] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("update_plan"), "{n}: {msg}");
    }
    // The phase verbs are real tools now — NOT aliases.
    for n in ["enter_plan_mode", "exit_plan_mode"] {
        assert!(
            resolve_tool_alias(n).is_none(),
            "{n} is a real tool, not an alias"
        );
    }
    // #715 PR2: the advance-ish verbs coach update_plan + "completed" too.
    for n in [
        "next_step",
        "complete_step",
        "finish_step",
        "mark_done",
        "step_done",
    ] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("update_plan"), "{n}: {msg}");
        assert!(msg.contains("completed"), "{n}: {msg}");
    }
    // #715 PR2: update_plan is the REAL tool now → not an alias (returns None),
    // exactly like the resume_context fix; the old set_plan name is gone too.
    assert!(
        resolve_tool_alias("update_plan").is_none(),
        "update_plan must dispatch as the real tool, not a self-alias"
    );
}

#[test]
fn alias_rewrites_plan_read_names_to_plan_get() {
    for n in [
        "get_plan",
        "show_plan",
        "read_plan",
        "current_plan",
        "what_was_i_doing",
    ] {
        assert!(
            matches!(
                resolve_tool_alias(n),
                Some(AliasOutcome::Rewrite("plan_get"))
            ),
            "{n} should rewrite to plan_get"
        );
    }
}

#[test]
fn alias_rewrites_resume_reaches_to_resume_context() {
    // #714: the instinctive "where did we leave off" reaches redirect to the
    // self-recovery tool, not plan_get.
    for n in [
        "resume",
        "where_were_we",
        "where_did_we_leave_off",
        "catch_me_up",
        "recap",
    ] {
        assert!(
            matches!(
                resolve_tool_alias(n),
                Some(AliasOutcome::Rewrite("resume_context"))
            ),
            "{n} should rewrite to resume_context"
        );
    }
    // The REAL tool name is not an alias: it returns None so a direct
    // resume_context call dispatches as a real tool and is NOT logged as a
    // phantom Rewrite by #717 telemetry (real names must return None).
    assert!(
        resolve_tool_alias("resume_context").is_none(),
        "the real tool name must return None, not a self-Rewrite"
    );
    // No regression: `what_was_i_doing` still asks specifically for the plan.
    assert!(
        matches!(
            resolve_tool_alias("what_was_i_doing"),
            Some(AliasOutcome::Rewrite("plan_get"))
        ),
        "what_was_i_doing must stay → plan_get"
    );
}

#[test]
fn alias_corrects_crew_names_and_flags_team_gating() {
    for n in [
        "delegate",
        "spawn_agent",
        "subagent",
        "sub_agent",
        "crew_dispatch",
        "run_crew",
        "dispatch_crew",
        "fork_agent",
        "assign",
        "team",
    ] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        // Names the real targets...
        assert!(msg.contains("compose_roster"), "{n}: {msg}");
        assert!(msg.contains("crew"), "{n}: {msg}");
        // ...but makes clear the model cannot self-enable the /team surface.
        assert!(msg.contains("/team"), "{n}: {msg}");
        assert!(
            msg.contains("human enables") || msg.contains("cannot turn it on yourself"),
            "crew correction must not imply the model can invoke it: {msg}"
        );
    }
}

#[test]
fn alias_corrects_workflow_names_to_plan_plus_crew() {
    for n in ["workflow", "run_workflow", "start_workflow", "pipeline"] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("no workflow tool"), "{n}: {msg}");
        assert!(msg.contains("update_plan"), "{n}: {msg}");
    }
}

#[test]
fn levenshtein_matches_known_distances() {
    assert_eq!(levenshtein("kitten", "sitting"), 3);
    assert_eq!(levenshtein("read_file", "read_file"), 0);
    assert_eq!(levenshtein("read_fil", "read_file"), 1);
    assert_eq!(levenshtein("", "abc"), 3);
}

#[test]
fn nearest_tool_name_suggests_close_only() {
    assert_eq!(nearest_tool_name("read_fil"), Some("read_file"));
    assert_eq!(nearest_tool_name("edit_fil"), Some("edit_file"));
    assert_eq!(nearest_tool_name("memory_fetchh"), Some("memory_fetch"));
    assert_eq!(nearest_tool_name("definitely_not_a_tool"), None);
}

#[test]
fn unknown_tool_message_names_catalog_and_suggestion() {
    let m = unknown_tool_message("read_fil");
    assert!(m.starts_with("unknown tool: read_fil"), "{m}");
    assert!(m.contains("Did you mean 'read_file'"), "{m}");
    assert!(m.contains("Available tools include:"), "{m}");

    let m2 = unknown_tool_message("zzzzzzzzzzzz");
    assert!(m2.starts_with("unknown tool: zzzzzzzzzzzz"), "{m2}");
    assert!(!m2.contains("Did you mean"), "{m2}");
    assert!(m2.contains("Available tools include:"), "{m2}");
}

/// An incompatible-arg alias is corrected (not dead-ended) by execute_tool:
/// a model that emits `str_replace_editor` is told to use edit_file. The
/// correction returns before any fs/caveat work, so this is deterministic.
#[tokio::test]
async fn execute_tool_corrects_str_replace_editor_alias() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "str_replace_editor",
        serde_json::json!({"command": "str_replace", "path": "f.txt"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("edit_file"), "got: {out}");
    assert!(!out.starts_with("unknown tool"), "got: {out}");
}

// -- #263 prompted permission grants through execute_tool ---------------

/// Scripted gate: records every request it is asked about and answers
/// allow (with caveats widened by exactly the requested grants) or deny.
struct MockGate {
    allow: bool,
    base: Caveats,
    asks: Vec<(String, String)>,
}

impl MockGate {
    fn new(allow: bool, base: &Caveats) -> Self {
        Self {
            allow,
            base: base.clone(),
            asks: Vec::new(),
        }
    }
}

impl super::PermissionGate for MockGate {
    fn ask(&mut self, requests: &[super::PermissionRequest]) -> super::PermissionDecision {
        for r in requests {
            self.asks
                .push((r.tool.clone(), format!("{}:{}", r.kind.as_str(), r.target)));
        }
        if self.allow {
            let grants: Vec<_> = requests
                .iter()
                .map(|r| (r.kind, r.target.clone()))
                .collect();
            super::PermissionDecision::Allow(crate::agentic::widen_caveats(&self.base, &grants))
        } else {
            super::PermissionDecision::Deny
        }
    }
    // #728: this gate exercises the GRANT path only; it has no human to
    // answer free-text questions, so it reports no operator available.
    fn ask_question(&mut self, _question: &str) -> HumanQuestionOutcome {
        HumanQuestionOutcome::Unavailable
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_tool_gated(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    gate: &mut MockGate,
) -> String {
    execute_tool(
        name,
        &args,
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None, // memory_source
        Some(gate),
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await
}

/// FR-2 (#1001): a one-tool remote MCP server for testing the remote-tool
/// leash — records whether `call` actually dispatched.
struct OneRemoteTool {
    name: &'static str,
    called: bool,
    resource_url_prefixes: &'static [&'static str],
}
impl OneRemoteTool {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            called: false,
            resource_url_prefixes: &[],
        }
    }

    fn with_resource_url_prefixes(
        mut self,
        resource_url_prefixes: &'static [&'static str],
    ) -> Self {
        self.resource_url_prefixes = resource_url_prefixes;
        self
    }
}
#[async_trait::async_trait]
impl McpTools for OneRemoteTool {
    fn handles(&self, name: &str) -> bool {
        name == self.name
    }
    fn tool_defs(&self) -> Vec<serde_json::Value> {
        let mut definition = serde_json::json!({
            "type": "function",
            "function": { "name": self.name, "description": "", "parameters": {} }
        });
        preserve_mcp_resource_url_affinity(
            &mut definition,
            Some(&serde_json::json!({
                "newt/resourceUrlPrefixes": self.resource_url_prefixes
            })),
        );
        vec![definition]
    }
    async fn call(&mut self, _leased: &LeasedMcpCall<'_>) -> String {
        self.called = true;
        "remote-tool-ran".to_string()
    }
}

struct CatalogOnlyMcp(Vec<serde_json::Value>);

#[async_trait::async_trait]
impl McpTools for CatalogOnlyMcp {
    fn handles(&self, _name: &str) -> bool {
        false
    }

    fn tool_defs(&self) -> Vec<serde_json::Value> {
        self.0.clone()
    }

    async fn call(&mut self, _leased: &LeasedMcpCall<'_>) -> String {
        "catalog-only MCP must not be called".to_string()
    }
}

async fn run_remote_gated(
    name: &str,
    ws: &std::path::Path,
    caveats: &Caveats,
    persona_tools: Option<&[String]>,
    mcp: &mut dyn McpTools,
    gate: Option<&mut MockGate>,
) -> String {
    let gate = gate.map(|g| g as &mut dyn super::PermissionGate);
    execute_tool_with_offload(
        name,
        &serde_json::json!({}),
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        mcp,
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
        false,
        None,
        persona_tools,
    )
    .await
}

/// Directly exercise the disposition-aware dispatcher. Unlike
/// [`execute_tool_with_offload`], this reaches the new required boundary
/// argument while fixing all unrelated optional seams to their inert shape.
#[allow(clippy::too_many_arguments)]
async fn run_tool_with_disposition(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    mcp: &mut dyn McpTools,
    gate: Option<&mut dyn PermissionGate>,
    step_ledger: Option<&dyn crate::agentic::scheduled::StepLedger>,
    disposition: PromptDisposition,
) -> String {
    execute_tool_with_offload_and_prompt_and_artifacts(
        name,
        &args,
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        mcp,
        None, // build_check_cmd
        None, // note_sink
        None, // recall_source
        None, // memory_source
        None, // prompt_context
        None, // artifact_context
        None, // artifact_sink
        gate,
        None, // exec_floor
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        step_ledger,
        false, // tool_offload
        None,  // spill_store
        None,  // persona_tools
        disposition,
    )
    .await
}

/// A non-Act disposition is an executor boundary, not just a reduced tool
/// schema: fabricated mutations, exec, capability requests, and remote MCP
/// calls must be refused before they reach their own dispatch logic.
#[tokio::test]
async fn non_act_disposition_denies_mutation_exec_grants_and_generic_mcp() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top(); // prove disposition wins over ambient authority

    let mut no_mcp = NoMcp;
    let write = run_tool_with_disposition(
        "write_file",
        serde_json::json!({ "path": "must-not-write.txt", "content": "no" }),
        ws.path(),
        &caveats,
        &mut no_mcp,
        None,
        None,
        PromptDisposition::Research,
    )
    .await;
    assert!(
        write.contains("is not available for this request"),
        "got: {write}"
    );
    assert!(
        !ws.path().join("must-not-write.txt").exists(),
        "disposition rejection must precede the write handler"
    );

    let exec = run_tool_with_disposition(
        "run_command",
        serde_json::json!({ "command": "touch must-not-exec.txt" }),
        ws.path(),
        &caveats,
        &mut no_mcp,
        None,
        None,
        PromptDisposition::Explain,
    )
    .await;
    assert!(
        exec.contains("is not available for this request"),
        "got: {exec}"
    );
    assert!(
        !ws.path().join("must-not-exec.txt").exists(),
        "disposition rejection must precede the shell handler"
    );

    let mut gate = MockGate::new(true, &caveats);
    let grant = run_tool_with_disposition(
        "request_permissions",
        serde_json::json!({
            "capability": "fs_write",
            "target": "/tmp/should-not-be-granted",
            "reason": "test",
        }),
        ws.path(),
        &caveats,
        &mut no_mcp,
        Some(&mut gate),
        None,
        PromptDisposition::Research,
    )
    .await;
    assert!(
        grant.contains("is not available for this request"),
        "got: {grant}"
    );
    assert!(
        gate.asks.is_empty(),
        "non-Act must not consult a grant gate"
    );

    let mut mcp = OneRemoteTool::new("incident__read");
    let remote = run_tool_with_disposition(
        "incident__read",
        serde_json::json!({}),
        ws.path(),
        &caveats,
        &mut mcp,
        None,
        None,
        PromptDisposition::Research,
    )
    .await;
    assert!(
        remote.contains("is not available for this request"),
        "got: {remote}"
    );
    assert!(
        !mcp.called,
        "generic MCP must be denied before remote routing in non-Act"
    );

    std::fs::write(ws.path().join("evidence.txt"), "durable evidence\n").unwrap();
    let read = run_tool_with_disposition(
        "read_file",
        serde_json::json!({ "path": "evidence.txt" }),
        ws.path(),
        &caveats,
        &mut no_mcp,
        None,
        None,
        PromptDisposition::Research,
    )
    .await;
    assert!(
        read.contains("durable evidence"),
        "safe read must remain usable: {read}"
    );
}

/// Plan is a read-only workspace disposition with one explicit
/// control-plane write: the harness-owned step ledger.
#[tokio::test]
async fn plan_disposition_updates_ledger_but_still_denies_workspace_mutation() {
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};

    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    let ledger = SessionStepLedger::default();
    let mut no_mcp = NoMcp;
    let plan = run_tool_with_disposition(
        "update_plan",
        serde_json::json!({ "plan": [
                { "step": "inspect", "status": "completed" },
                { "step": "repair", "status": "in_progress" }
            ] }),
        ws.path(),
        &caveats,
        &mut no_mcp,
        None,
        Some(&ledger),
        PromptDisposition::Plan,
    )
    .await;
    assert!(plan.starts_with("<plan>\n"), "{plan}");
    assert_eq!(ledger.count(), 2);

    let write = run_tool_with_disposition(
        "write_file",
        serde_json::json!({ "path": "must-not-write.txt", "content": "no" }),
        ws.path(),
        &caveats,
        &mut no_mcp,
        None,
        Some(&ledger),
        PromptDisposition::Plan,
    )
    .await;
    assert!(
        write.contains("is not available for this request"),
        "{write}"
    );
    assert!(!ws.path().join("must-not-write.txt").exists());
}

#[tokio::test]
async fn auto_mode_selector_dispatches_through_session_control_without_current_widening() {
    #[derive(Default)]
    struct RecordingControl(std::sync::Mutex<Vec<String>>);

    impl crate::agentic::OperatingModeControl for RecordingControl {
        fn select_operating_mode(&self, mode: &str) -> Result<String, String> {
            self.0.lock().unwrap().push(mode.to_string());
            Ok(format!("scheduled {mode}; current turn unchanged"))
        }
    }

    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    let control = RecordingControl::default();
    let mut no_mcp = NoMcp;
    let result = execute_tool_with_collaborators(
        "select_operating_mode",
        &serde_json::json!({ "mode": "dev" }),
        ws.path().to_str().unwrap(),
        false,
        20,
        &caveats,
        &mut no_mcp,
        ToolCollaborators {
            operating_mode_control: Some(&control),
            ..Default::default()
        },
        false,
        PromptDisposition::Research,
        None,
    )
    .await
    .unwrap();
    assert!(result.contains("current turn unchanged"), "{result}");
    assert_eq!(*control.0.lock().unwrap(), vec!["dev"]);

    let unavailable = execute_tool_with_collaborators(
        "select_operating_mode",
        &serde_json::json!({ "mode": "dev" }),
        ws.path().to_str().unwrap(),
        false,
        20,
        &caveats,
        &mut no_mcp,
        ToolCollaborators::default(),
        false,
        PromptDisposition::Research,
        None,
    )
    .await
    .unwrap();
    assert!(unavailable.contains("/mode auto"), "{unavailable}");
}

/// Permitted non-Act reads still honor their caveats, but they must not
/// silently turn a denial into an interactive authority grant.
#[tokio::test]
async fn non_act_read_tools_do_not_consult_permission_gate() {
    let ws = tempfile::TempDir::new().unwrap();
    let mut caveats = Caveats::top();
    caveats.net = crate::caveats::Scope::none();
    let mut gate = MockGate::new(true, &caveats);
    let mut mcp = NoMcp;

    let _ = run_tool_with_disposition(
        "web_fetch",
        serde_json::json!({ "url": "https://example.com" }),
        ws.path(),
        &caveats,
        &mut mcp,
        Some(&mut gate),
        None,
        PromptDisposition::Research,
    )
    .await;
    assert!(
        gate.asks.is_empty(),
        "a non-Act web read may be caveat-denied but must never mint net authority"
    );
}

/// FR-2 (#1001): a remote MCP tool OUTSIDE the persona's allow-list is
/// `mcp-under-leash`: with NO active persona, a MUTATING remote tool must NOT
/// dispatch unleashed — "no persona" is not "unrestricted". Headless (no
/// gate) → fail-closed. Regression for the pre-leash hole where a no-persona
/// session dispatched every remote tool with zero mediation.
#[tokio::test]
async fn no_persona_does_not_dispatch_a_mutating_mcp_tool_unleashed() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = crate::caveats::Caveats::top();

    let mut mcp = OneRemoteTool::new("incident__delete"); // mutating verb
    let out = run_remote_gated(
        "incident__delete",
        ws.path(),
        &caveats,
        None, // NO persona
        &mut mcp,
        None, // headless: no gate to consult
    )
    .await;
    assert!(
        !mcp.called,
        "a no-persona mutating remote tool must NOT dispatch unleashed"
    );
    assert!(out.contains("persona") || out.contains("denied") || out.contains("leash"));
}

/// `mcp-under-leash` (name-classification vector): a no-persona READ-verb
/// remote tool is NOT auto-granted by its name. A hostile admitted server
/// that names a destructive operation with a read verb (`get_…`) earns
/// nothing — headless it is DENIED (fail-closed), and it dispatches only on
/// an explicit human grant. (Was `..._still_dispatches`, which asserted the
/// now-closed name-auto-grant; flipped red→green with the leash slice.)
#[tokio::test]
async fn no_persona_read_verb_tool_is_not_name_granted() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = crate::caveats::Caveats::top();

    // A hostile server named a destructive op with a read verb. Headless, no
    // persona: the read verb does NOT grant — fail-closed.
    let mut mcp = OneRemoteTool::new("evil__get_wipe_everything");
    let out = run_remote_gated(
        "evil__get_wipe_everything",
        ws.path(),
        &caveats,
        None, // no persona
        &mut mcp,
        None, // headless: no human to consult
    )
    .await;
    assert!(
        !mcp.called,
        "a no-persona read-verb-named remote tool must NOT dispatch on the name alone"
    );
    assert!(
        out.contains("persona") || out.contains("grant") || out.contains("leash"),
        "{out}"
    );

    // The SAME tool dispatches only when a present human explicitly grants it.
    let mut mcp = OneRemoteTool::new("evil__get_wipe_everything");
    let mut gate = MockGate::new(true, &caveats);
    let out = run_remote_gated(
        "evil__get_wipe_everything",
        ws.path(),
        &caveats,
        None,
        &mut mcp,
        Some(&mut gate),
    )
    .await;
    assert!(mcp.called, "an explicitly human-granted call dispatches");
    assert_eq!(
        gate.asks.len(),
        1,
        "the human WAS prompted (no silent name-grant)"
    );
    assert_eq!(out, "remote-tool-ran");
}

/// A human can still grant a no-persona MUTATING tool through the gate.
#[tokio::test]
async fn no_persona_mutating_mcp_tool_dispatches_when_human_grants() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = crate::caveats::Caveats::top();

    let mut mcp = OneRemoteTool::new("incident__delete");
    let mut gate = MockGate::new(true, &caveats); // human allows
    let out = run_remote_gated(
        "incident__delete",
        ws.path(),
        &caveats,
        None,
        &mut mcp,
        Some(&mut gate),
    )
    .await;
    assert!(
        mcp.called,
        "a human-granted mutating remote tool dispatches"
    );
    assert_eq!(
        gate.asks.len(),
        1,
        "the human was prompted for the mutating op"
    );
    assert_eq!(out, "remote-tool-ran");
}

/// PROMPTED (not hard-vetoed like a built-in). Deny → withheld and `call`
/// never runs; Allow → dispatched; a tool the persona already grants
/// dispatches with NO prompt; headless (no gate) fails closed.
#[tokio::test]
async fn remote_tool_outside_allow_list_is_prompted_not_hard_vetoed() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = crate::caveats::Caveats::top();
    let coach = vec!["read_file".to_string()]; // no incident__create

    // Gate DENIES → withheld; `call` never invoked; the human WAS prompted,
    // and prompted as a remote-tool grant (not an fs/exec/net axis).
    let mut mcp = OneRemoteTool::new("incident__create");
    let mut gate = MockGate::new(false, &caveats);
    let out = run_remote_gated(
        "incident__create",
        ws.path(),
        &caveats,
        Some(&coach),
        &mut mcp,
        Some(&mut gate),
    )
    .await;
    assert!(!mcp.called, "denied remote tool must NOT dispatch");
    assert_eq!(gate.asks.len(), 1, "the human was prompted");
    assert_eq!(gate.asks[0].1, "remote_tool:incident__create");
    assert!(out.contains("persona"), "returns a denial: {out}");

    // Gate ALLOWS → dispatched.
    let mut mcp = OneRemoteTool::new("incident__create");
    let mut gate = MockGate::new(true, &caveats);
    let out = run_remote_gated(
        "incident__create",
        ws.path(),
        &caveats,
        Some(&coach),
        &mut mcp,
        Some(&mut gate),
    )
    .await;
    assert!(mcp.called, "granted remote tool dispatches");
    assert_eq!(out, "remote-tool-ran");

    // A remote tool the persona GRANTS dispatches with NO prompt.
    let granted = vec!["incident__create".to_string()];
    let mut mcp = OneRemoteTool::new("incident__create");
    let mut gate = MockGate::new(false, &caveats); // would deny if asked
    run_remote_gated(
        "incident__create",
        ws.path(),
        &caveats,
        Some(&granted),
        &mut mcp,
        Some(&mut gate),
    )
    .await;
    assert!(mcp.called, "allow-listed remote tool dispatches");
    assert!(
        gate.asks.is_empty(),
        "no prompt when the persona already grants it"
    );

    // Headless (no gate) → fail-closed: withheld, `call` never runs.
    let mut mcp = OneRemoteTool::new("incident__create");
    let out = run_remote_gated(
        "incident__create",
        ws.path(),
        &caveats,
        Some(&coach),
        &mut mcp,
        None,
    )
    .await;
    assert!(
        !mcp.called,
        "headless must fail closed for an ungranted remote tool"
    );
    assert!(out.contains("persona"), "headless denial: {out}");
}

// -- #721 recoverable denials + request_permissions ---------------------

#[test]
fn exec_denial_is_recoverable_not_a_dead_end() {
    // #721 + #775: the exec denial the MODEL sees is ONE clean level —
    // `capability denied: <bare reason>. <recovery hint>` — leading to the
    // model-actionable request_permissions path, NOT the stale `extra_exec`
    // config edit (which #721 superseded and the model cannot perform
    // mid-turn).
    let envelope = serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "exec",
            "target": "mkdir",
            "reason": "exec of \"mkdir\" is not within the granted authority"
        }]
    });
    let out = denied_run_command_result(&envelope, false);
    assert!(out.starts_with("capability denied:"), "got: {out}");
    assert!(out.contains("request_permissions"), "got: {out}");
    // #775: the stale `extra_exec` config hint is GONE from the model-facing
    // message (it leaked in before).
    assert!(
        !out.contains("extra_exec"),
        "the model message must not carry the stale config hint: {out}"
    );
}

/// #775 (§2.5) regression: the model-facing `run_command` denial is ONE
/// clean level and never a denial sentence NESTED inside another. Before
/// the fix, `denied_run_command_result` appended the `extra_exec` config
/// hint to the reason (and the former notice stuffed that whole sentence into
/// its bare `'{target}'` slot), yielding `capability denied: exec does not
/// permit '<reason> - add it via …>'`. The model-facing return now carries
/// exactly one `capability denied:`, the bare reason, and the recovery hint.
#[test]
fn run_command_denial_is_single_level_not_nested() {
    let envelope = serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "exec",
            "target": "export",
            "reason": "exec of \"export\" is not within the granted authority"
        }]
    });
    let out = denied_run_command_result(&envelope, false);
    // Exactly one denial prefix — never a `capability denied:` inside another.
    assert_eq!(
        out.matches("capability denied:").count(),
        1,
        "exactly one denial level: {out}"
    );
    // RED on today: the stale config hint was glued onto the model message.
    assert!(!out.contains("add it via"), "stale config hint: {out}");
    assert!(!out.contains("extra_exec"), "stale config hint: {out}");
    // No reason sentence nested inside a `does not permit '…'` slot.
    assert!(
        !out.contains("does not permit 'exec of"),
        "nested denial sentence: {out}"
    );
    // The bare reason and the #721 recovery hint are both present.
    assert!(
        out.contains("exec of \"export\" is not within the granted authority"),
        "got: {out}"
    );
    assert!(out.contains("request_permissions"), "got: {out}");
}

#[test]
fn parse_capability_maps_synonyms_and_rejects_unknown() {
    assert_eq!(parse_capability("exec"), Some(DenialKind::Exec));
    assert_eq!(parse_capability("shell"), Some(DenialKind::Exec));
    assert_eq!(parse_capability("FS_READ"), Some(DenialKind::FsRead));
    assert_eq!(parse_capability("write"), Some(DenialKind::FsWrite));
    assert_eq!(parse_capability("network"), Some(DenialKind::Net));
    assert_eq!(parse_capability("gpu"), None);
    assert_eq!(parse_capability(""), None);
}

#[test]
fn request_permissions_grant_deny_and_no_gate() {
    let base = Caveats::top();

    // Mock gate ALLOWS → "granted" + the retry coaching; the gate was asked
    // with the parsed axis + target.
    let mut gate = MockGate::new(true, &base);
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "exec", "target": "mkdir", "reason": "make a dir"}),
        Some(&mut gate),
        false,
        20,
    );
    assert!(out.starts_with("granted:"), "got: {out}");
    assert!(out.contains("Retry the original operation"), "got: {out}");
    assert_eq!(gate.asks.len(), 1);
    assert_eq!(
        gate.asks[0],
        ("request_permissions".to_string(), "exec:mkdir".to_string())
    );

    // Mock gate DENIES → "denied" + don't-retry coaching.
    let mut gate = MockGate::new(false, &base);
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "fs_write", "target": "/tmp/x", "reason": "w"}),
        Some(&mut gate),
        false,
        20,
    );
    assert!(out.starts_with("denied:"), "got: {out}");
    assert!(out.contains("different approach"), "got: {out}");

    // NO gate (headless / eval) → "no operator available" — recoverable,
    // never a hang or a config-only dead end.
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "net", "target": "docs.rs", "reason": "fetch"}),
        None,
        false,
        20,
    );
    assert!(out.contains("no operator available"), "got: {out}");
}

/// #1547: the headless `request_permissions` answer must be ACTIONABLE, not
/// a dead-end. With no gate, authority cannot be widened mid-run, so the
/// model must be told to (a) stop re-asking and (b) proceed within the
/// authority it already holds — NOT that "the owner must configure it"
/// (there is no owner mid-run) or to "take a different approach for now"
/// (which abandons a task the confined bench lane already authorizes and
/// burns tool-call rounds). Would fail on the old dead-end copy.
#[test]
fn request_permissions_headless_answer_is_forward_guidance_not_a_dead_end() {
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "fs_write", "target": "/app/out", "reason": "write result"}),
        None,
        false,
        20,
    );
    // Preserves the recoverable "no operator" signal.
    assert!(out.contains("no operator available"), "got: {out}");
    // Tells the model to proceed within its existing authority (forward
    // guidance) and that re-calling the tool is pointless headless.
    assert!(
        out.contains("Proceed within the authority you already have"),
        "headless answer must tell the model to proceed within current authority: {out}"
    );
    assert!(
        out.contains("re-calling request_permissions will not help"),
        "headless answer must tell the model not to keep asking: {out}"
    );
    // Must NOT re-route the model to a config edit it cannot perform
    // mid-run, or tell it to abandon its approach — the old dead-ends.
    assert!(
        !out.contains("must be configured by the owner"),
        "headless answer must not dead-end on an owner config edit: {out}"
    );
    assert!(
        !out.contains("take a different approach for now"),
        "headless answer must not tell the model to abandon its approach: {out}"
    );
}

#[test]
fn request_permissions_coaches_bad_inputs() {
    // Unknown capability → coach listing the valid axes (no gate consulted).
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "gpu", "target": "x", "reason": "y"}),
        None,
        false,
        20,
    );
    assert!(out.contains("unknown capability"), "got: {out}");
    assert!(out.contains("fs_read"), "got: {out}");
    // Missing target → coach.
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "exec", "reason": "y"}),
        None,
        false,
        20,
    );
    assert!(out.contains("'target' is required"), "got: {out}");
}

#[test]
fn request_permissions_is_a_real_tool_not_a_phantom() {
    // #721: a real, always-advertised tool — never an alias / hallucination.
    assert!(resolve_tool_alias("request_permissions").is_none());
    assert!(ALL_TOOL_NAMES.contains(&"request_permissions"));
    assert!(classify_phantom_reach(
        "request_permissions",
        &serde_json::json!({"capability": "exec", "target": "mkdir", "reason": "r"}),
        "granted: the operator allowed exec for 'mkdir'.",
        true,
    )
    .is_none());
}

// -- #728 request_user_input (generic ask-the-human) --------------------

/// A gate that answers a free-text question with a scripted
/// [`HumanQuestionOutcome`]. Its grant path (`ask`) is irrelevant here — it
/// denies.
struct AskGate {
    outcome: HumanQuestionOutcome,
    asked: Vec<String>,
}
impl AskGate {
    /// `Some(answer)` → an answer; `None` → no operator available.
    fn new(answer: Option<&str>) -> Self {
        let outcome = answer.map_or(HumanQuestionOutcome::Unavailable, |a| {
            HumanQuestionOutcome::Answer(a.to_string())
        });
        Self::with_outcome(outcome)
    }
    fn with_outcome(outcome: HumanQuestionOutcome) -> Self {
        Self {
            outcome,
            asked: Vec::new(),
        }
    }
}
impl super::PermissionGate for AskGate {
    fn ask(&mut self, _requests: &[super::PermissionRequest]) -> super::PermissionDecision {
        super::PermissionDecision::Deny
    }
    fn ask_question(&mut self, question: &str) -> HumanQuestionOutcome {
        self.asked.push(question.to_string());
        self.outcome.clone()
    }
}

#[test]
fn request_user_input_returns_the_human_answer() {
    // A gate whose ask_question returns Some(answer) → the tool returns that
    // answer verbatim, and the gate was asked the exact question.
    let mut gate = AskGate::new(Some("postgres"));
    let out = execute_request_user_input(
        &serde_json::json!({"question": "which database should I target?"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, "postgres");
    assert_eq!(
        gate.asked,
        vec!["which database should I target?".to_string()]
    );
}

#[test]
fn request_user_input_reaches_the_operator_even_when_permissions_are_denied() {
    // Blocker: disabling permission prompts must NOT erase the operator. A
    // gate whose authorization path denies (AskGate.ask → Deny) but which has
    // a present human still answers request_user_input — never "headless".
    let mut gate = AskGate::new(Some("postgres"));
    let out = execute_request_user_input(
        &serde_json::json!({"question": "which database?"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, "postgres");
    assert!(
        !out.contains("headless"),
        "a present operator is not headless: {out}"
    );
}

#[test]
fn request_user_input_no_gate_reports_headless_never_hangs() {
    // No gate (headless / eval / ACP) → the recoverable "no human available"
    // message — never a hang. (This test completing IS the no-hang proof: it
    // touches no real stdin.)
    let out = execute_request_user_input(
        &serde_json::json!({"question": "are you sure?"}),
        None,
        false,
        20,
    );
    assert_eq!(out, HEADLESS_NO_HUMAN);
    assert!(out.contains("no human available"), "got: {out}");
}

#[test]
fn request_user_input_unavailable_reports_no_operator_not_headless() {
    // A gate present but with no interactive operator (Unavailable) → the
    // no-operator message, NOT "headless": only an absent gate is headless.
    let mut gate = AskGate::with_outcome(HumanQuestionOutcome::Unavailable);
    let out = execute_request_user_input(
        &serde_json::json!({"question": "pick one"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, NO_OPERATOR_AVAILABLE);
    assert!(
        !out.contains("headless"),
        "Unavailable must not say headless: {out}"
    );
}

#[test]
fn request_user_input_cancelled_reports_cancel_not_headless() {
    // Esc / slash back-out (Cancelled) → an explicit cancel message, never
    // "headless" or "no human available" — the operator IS present.
    let mut gate = AskGate::with_outcome(HumanQuestionOutcome::Cancelled);
    let out = execute_request_user_input(
        &serde_json::json!({"question": "pick one"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, OPERATOR_CANCELLED);
    assert!(!out.contains("headless"), "got: {out}");
    assert!(!out.contains("no human available"), "got: {out}");
}

#[test]
fn request_user_input_exit_reports_exit_not_headless() {
    // Ctrl-C / Ctrl-D (ExitRequested) → an explicit exit message, not headless.
    let mut gate = AskGate::with_outcome(HumanQuestionOutcome::ExitRequested);
    let out = execute_request_user_input(
        &serde_json::json!({"question": "pick one"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, OPERATOR_EXIT_REQUESTED);
    assert!(!out.contains("headless"), "got: {out}");
}

#[test]
fn request_user_input_eof_is_not_an_empty_answer() {
    // EOF (InputClosed) must NOT surface as an empty answer (""), and must
    // not be reported as headless.
    let mut gate = AskGate::with_outcome(HumanQuestionOutcome::InputClosed);
    let out = execute_request_user_input(
        &serde_json::json!({"question": "pick one"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, OPERATOR_INPUT_CLOSED);
    assert!(!out.is_empty(), "EOF must not become an empty answer");
    assert!(!out.contains("headless"), "got: {out}");
}

#[test]
fn request_user_input_failure_is_distinct_from_headless() {
    // An input I/O failure (InputFailed) is distinct from a headless session.
    let mut gate = AskGate::with_outcome(HumanQuestionOutcome::InputFailed);
    let out = execute_request_user_input(
        &serde_json::json!({"question": "pick one"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, OPERATOR_INPUT_FAILED);
    assert_ne!(out, HEADLESS_NO_HUMAN);
    assert!(!out.contains("headless"), "got: {out}");
}

#[test]
fn request_user_input_empty_answer_stays_an_empty_answer() {
    // An explicitly submitted empty line is Answer("") — distinct from EOF.
    let mut gate = AskGate::with_outcome(HumanQuestionOutcome::Answer(String::new()));
    let out = execute_request_user_input(
        &serde_json::json!({"question": "pick one"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, "");
}

#[test]
fn request_user_input_requires_a_question() {
    // Missing / blank question → coach; the gate is never consulted.
    let mut gate = AskGate::new(Some("unused"));
    let out = execute_request_user_input(
        &serde_json::json!({"question": "   "}),
        Some(&mut gate),
        false,
        20,
    );
    assert!(out.contains("'question' is required"), "got: {out}");
    assert!(
        gate.asked.is_empty(),
        "gate not consulted for a blank question"
    );
}

#[test]
fn request_user_input_is_a_real_tool_not_a_phantom() {
    // #728: a real, always-advertised tool — never an alias of itself or a
    // hallucination.
    assert!(resolve_tool_alias("request_user_input").is_none());
    assert!(ALL_TOOL_NAMES.contains(&"request_user_input"));
    assert!(classify_phantom_reach(
        "request_user_input",
        &serde_json::json!({"question": "which db?"}),
        "postgres",
        true,
    )
    .is_none());
    // The always-advertised def rides in every session (empty MCP).
    let defs = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    let names: Vec<&str> = defs
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert!(names.contains(&"request_user_input"), "got: {names:?}");
}

#[test]
fn ask_verbs_rewrite_to_request_user_input() {
    // #728: the instinctive ask-the-human verbs resolve to the real tool.
    for verb in [
        "ask_user",
        "ask_human",
        "prompt_user",
        "get_user_input",
        "ask_question",
        "clarify",
        "ask",
    ] {
        match resolve_tool_alias(verb) {
            Some(AliasOutcome::Rewrite(c)) => {
                assert_eq!(c, "request_user_input", "verb: {verb}");
            }
            _ => panic!("expected Rewrite(request_user_input) for {verb}"),
        }
    }
}

#[tokio::test]
async fn request_user_input_dispatches_through_execute_tool() {
    // End-to-end through the dispatcher: the question reaches the gate and
    // the answer flows back. Fully mocked (AskGate, no real stdin).
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    let mut gate = AskGate::new(Some("the answer"));
    let out = execute_tool(
        "request_user_input",
        &serde_json::json!({"question": "what now?"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None, // memory_source
        Some(&mut gate),
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_eq!(out, "the answer");
    assert_eq!(gate.asked, vec!["what now?".to_string()]);
}

#[tokio::test]
async fn explain_request_user_input_keeps_the_interactive_question_gate() {
    // Regression: the non-Act authority clamp used to erase the whole gate,
    // which made this advertised Explain tool falsely report headless. Its
    // free-text path does not mint authority, so it must keep the operator.
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    let mut mcp = NoMcp;
    let mut gate = AskGate::new(Some("send an Act request"));

    let out = run_tool_with_disposition(
        "request_user_input",
        serde_json::json!({"question": "Please send this as an explicit action request."}),
        ws.path(),
        &caveats,
        &mut mcp,
        Some(&mut gate),
        None,
        PromptDisposition::Explain,
    )
    .await;

    assert_eq!(out, "send an Act request");
    assert_eq!(
        gate.asked,
        vec!["Please send this as an explicit action request.".to_string()]
    );
}

#[test]
fn get_context_remaining_is_a_real_tool_not_a_phantom() {
    // #727: real, always-advertised, no-arg budget read — never treated as
    // an alias of itself or a hallucination.
    assert!(resolve_tool_alias("get_context_remaining").is_none());
    assert!(ALL_TOOL_NAMES.contains(&"get_context_remaining"));
    assert!(classify_phantom_reach(
        "get_context_remaining",
        &serde_json::json!({}),
        "Context budget: ~10 tokens used of an input ceiling of ~80 (80% of num_ctx 100).",
        true,
    )
    .is_none());
    // The always-advertised def rides in every session (empty MCP).
    let defs = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(defs
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["function"]["name"] == "get_context_remaining"));
}

#[test]
fn budget_verbs_rewrite_to_get_context_remaining() {
    // #727: the instinctive "how much context is left" reaches all resolve
    // to the canonical no-arg read (safe silent Rewrite — matching arg shape).
    for n in [
        "context_remaining",
        "tokens_left",
        "remaining_tokens",
        "budget",
        "how_much_context",
        "context_budget",
        "token_budget",
    ] {
        assert!(
            matches!(
                resolve_tool_alias(n),
                Some(AliasOutcome::Rewrite("get_context_remaining"))
            ),
            "{n} must rewrite to get_context_remaining"
        );
        // A Rewrite alias is mined by the #717 telemetry as a Rewrite.
        assert!(
            is_context_remaining_call(n),
            "{n} must be recognized as a budget call by the loop"
        );
    }
    // The canonical name is recognized by the loop but is NOT an alias.
    assert!(is_context_remaining_call("get_context_remaining"));
    assert!(resolve_tool_alias("get_context_remaining").is_none());
    // An unrelated name is neither.
    assert!(!is_context_remaining_call("read_file"));
}

/// FLAG OFF (no gate): the denial is deterministic and still DENIES every
/// fs op (the #263 default-deny posture is intact) — now in the #721
/// recoverable form (`denied_fs_result`, carrying the request_permissions
/// path), pinned via the shared helper so the wording can't drift.
#[tokio::test]
async fn no_gate_denials_are_bit_for_bit_unchanged() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_read", "secret.txt"));
    let out = run_tool(
        "list_dir",
        serde_json::json!({"path": "."}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_read", "."));
    let out = run_tool(
        "write_file",
        serde_json::json!({"path": "a.txt", "content": "c"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_write", "a.txt"));
    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "a.txt", "old_string": "a", "new_string": "b"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_write", "a.txt"));
    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_write", "secret.txt"));
    // #721: every fs denial now carries the model-actionable recovery path.
    assert!(out.contains("request_permissions"), "got: {out}");
}

/// Gate allows an fs_read denial → the read proceeds and returns the
/// real contents; the gate was consulted with the tool + axis + full
/// path it would be granting.
#[tokio::test]
async fn gate_allow_turns_fs_read_denial_into_the_real_result() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "the contents").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(true, &denied);
    let out = run_tool_gated(
        "read_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    assert_eq!(out, "the contents");
    let full = ws.path().join("secret.txt").to_string_lossy().into_owned();
    assert_eq!(
        gate.asks,
        vec![("read_file".to_string(), format!("fs_read:{full}"))]
    );
}

#[cfg(not(windows))]
#[tokio::test]
async fn permission_retry_closes_each_live_generation_before_the_next_starts() {
    let _l = super::disable_ocap_tests::env_lock().await;
    // Pin the engine for deterministic permission-retry behavior when the
    // workspace suite runs tests concurrently with ambient shell settings.
    let _eng = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    #[derive(Default)]
    struct LifecycleOutput(std::sync::Mutex<Vec<String>>);
    impl crate::agentic::LiveToolOutput for LifecycleOutput {
        fn start(&self, generation: u64) {
            self.0.lock().unwrap().push(format!("start:{generation}"));
        }
        fn write(&self, generation: u64, _stream: crate::agentic::ToolOutputStream, chunk: &[u8]) {
            self.0.lock().unwrap().push(format!(
                "write:{generation}:{}",
                String::from_utf8_lossy(chunk)
            ));
        }
        fn finish(&self, generation: u64) {
            self.0.lock().unwrap().push(format!("finish:{generation}"));
        }
        fn abandon(&self, generation: u64) {
            self.0.lock().unwrap().push(format!("abandon:{generation}"));
        }
    }

    let ws = tempfile::TempDir::new().unwrap();
    let denied = Caveats {
        exec: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(true, &denied);
    let sink = std::sync::Arc::new(LifecycleOutput::default());
    let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 3, false);
    let out = exec_confined_command(
        // Use an external executable under every engine. Bare `echo` is a
        // Brush builtin and therefore correctly needs no exec grant.
        "/bin/echo retry-visible",
        &ws.path().to_string_lossy(),
        false,
        20,
        &denied,
        None,
        Some(&mut gate),
        false,
        None,
        Some(sink.clone()),
        &mut display,
    )
    .await;

    assert!(out.contains("retry-visible"), "retry result: {out}");
    assert_eq!(gate.asks.len(), 1, "permission prompt count");
    let events = sink.0.lock().unwrap();
    let starts: Vec<_> = events
        .iter()
        .filter(|event| event.starts_with("start:"))
        .cloned()
        .collect();
    assert_eq!(starts.len(), 2, "one viewport per attempt: {events:?}");
    let first_generation = starts[0].trim_start_matches("start:");
    let retry_start = events
        .iter()
        .position(|event| event == &starts[1])
        .expect("retry start event");
    assert!(
        events[..retry_start]
            .iter()
            .any(|event| event == &format!("finish:{first_generation}")),
        "retry started before the denied generation finished: {events:?}"
    );
    let second_generation = starts[1].trim_start_matches("start:");
    assert!(
        events.iter().any(|event| {
            event.starts_with(&format!("write:{second_generation}:"))
                && event.contains("retry-visible")
        }),
        "retry bytes were not delivered to its generation: {events:?}"
    );
    let expected_finish = format!("finish:{second_generation}");
    assert_eq!(events.last(), Some(&expected_finish), "events: {events:?}");
}

/// Gate denies → the result is the standard denial, bit-for-bit equal to
/// the no-gate path (#263: deny = the current denial result).
#[tokio::test]
async fn gate_deny_keeps_the_standard_denial_bit_for_bit() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(false, &denied);
    let gated = run_tool_gated(
        "read_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    let ungated = run_tool(
        "read_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(gated, ungated);
    assert_eq!(gated, denied_fs_result("fs_read", "secret.txt"));
    assert_eq!(gate.asks.len(), 1, "the human was asked exactly once");
}

/// Gate allows fs_write denials → write_file, edit_file, and delete_file proceed.
#[tokio::test]
async fn gate_allow_turns_fs_write_denials_into_real_writes() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("f.txt"), "old\n").unwrap();
    std::fs::write(ws.path().join("stale.txt"), "remove me\n").unwrap();
    let denied = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(true, &denied);
    let out = run_tool_gated(
        "write_file",
        serde_json::json!({"path": "new.txt", "content": "fresh"}),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    assert!(out.starts_with("wrote new.txt"), "got: {out}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("new.txt")).unwrap(),
        "fresh"
    );
    let out = run_tool_gated(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "old", "new_string": "new"}),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    assert!(out.starts_with("edited f.txt"), "got: {out}");
    let out = run_tool_gated(
        "delete_file",
        serde_json::json!({"path": "stale.txt"}),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    assert!(out.starts_with("deleted stale.txt"), "got: {out}");
    assert!(
        !ws.path().join("stale.txt").exists(),
        "gate-approved delete must remove the file"
    );
    assert_eq!(gate.asks.len(), 3);
    assert_eq!(gate.asks[0].0, "write_file");
    assert!(
        gate.asks[1].1.starts_with("fs_write:"),
        "got: {:?}",
        gate.asks[1]
    );
    assert_eq!(gate.asks[2].0, "delete_file");
}

/// list_dir consults the gate on an fs_read denial like read_file does.
#[tokio::test]
async fn gate_allow_turns_list_dir_denial_into_the_listing() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("seen.txt"), "x").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(true, &denied);
    let out = run_tool_gated(
        "list_dir",
        serde_json::json!({"path": "."}),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    assert!(out.contains("seen.txt"), "got: {out}");
}

/// A buggy/hostile gate answering Allow with caveats that STILL don't
/// cover the path must not bypass enforcement: the widened authority is
/// re-checked, never assumed (fs_gate_allows' re-check).
#[tokio::test]
async fn gate_allow_without_real_coverage_is_still_denied() {
    struct LyingGate;
    impl super::PermissionGate for LyingGate {
        fn ask(&mut self, _requests: &[super::PermissionRequest]) -> super::PermissionDecision {
            // "Allow", but the caveats grant nothing at all.
            super::PermissionDecision::Allow(Caveats {
                fs_read: Scope::none(),
                fs_write: Scope::none(),
                exec: Scope::none(),
                net: Scope::none(),
                max_calls: CountBound::Unlimited,
                valid_for_generation: Scope::All,
            })
        }
        fn ask_question(&mut self, _question: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
    }
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = LyingGate;
    let out = execute_tool(
        "read_file",
        &serde_json::json!({"path": "secret.txt"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &denied,
        &mut NoMcp,
        None,
        None,
        None,
        None, // memory_source
        Some(&mut gate),
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_read", "secret.txt"));
}

/// web_fetch with a gate: an out-of-allowlist host consults the gate
/// with the parsed host; on deny the dispatch runs under the ORIGINAL
/// caveats, so the leash produces today's denial (an `error:` result —
/// nothing is fetched).
#[tokio::test]
async fn web_fetch_gate_deny_dispatches_under_original_caveats() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path()); // net: Scope::none()
    let mut gate = MockGate::new(false, &caveats);
    let out = run_tool_gated(
        "web_fetch",
        serde_json::json!({"url": "https://denied.example.com:8443/page"}),
        ws.path(),
        &caveats,
        &mut gate,
    )
    .await;
    assert!(out.starts_with("error:"), "leash denial surfaces: {out}");
    assert_eq!(
        gate.asks,
        vec![(
            "web_fetch".to_string(),
            "net:denied.example.com".to_string()
        )]
    );
}

/// Regression for the field report: github.com is outside the default net
/// scope, so a TUI-provided gate must be consulted before the bridle leash
/// returns the denial to the model.
#[tokio::test]
async fn web_fetch_github_denial_consults_permission_gate() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path()); // net: Scope::none()
    let mut gate = MockGate::new(false, &caveats);
    let out = run_tool_gated(
        "web_fetch",
        serde_json::json!({"url": "https://github.com/openai/codex"}),
        ws.path(),
        &caveats,
        &mut gate,
    )
    .await;
    assert!(out.starts_with("error:"), "leash denial surfaces: {out}");
    assert_eq!(
        gate.asks,
        vec![("web_fetch".to_string(), "net:github.com".to_string())]
    );
}

/// An unparseable URL skips the net pre-check entirely — the gate is
/// never consulted and the dispatch (with the original caveats) answers.
#[tokio::test]
async fn web_fetch_unparseable_url_never_prompts() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let mut gate = MockGate::new(true, &caveats);
    let out = run_tool_gated(
        "web_fetch",
        serde_json::json!({"url": "not-a-url"}),
        ws.path(),
        &caveats,
        &mut gate,
    )
    .await;
    assert!(out.starts_with("error:"), "got: {out}");
    assert!(gate.asks.is_empty(), "no prompt for an unparseable URL");
}

/// Field-regression: a private code-review URL may be intentionally blocked
/// by the raw-fetch SSRF policy while an authenticated MCP source is already
/// connected. The result must preserve the refusal and put catalog discovery
/// plus the namespaced connector ahead of shell/user-configuration fallbacks.
#[tokio::test]
async fn private_address_fetch_failure_routes_to_connected_mcp_first() {
    let mcp = OneRemoteTool::new("opaque_bridge__read_object")
        .with_resource_url_prefixes(&["https://reviews.example.test/reviews/"]);
    let error = "denied: SSRF block: \"reviews.example.test\" resolved to \
                     private/loopback address 10.0.0.1 (not in the net allowlist)";

    let url = "https://reviews.example.test/reviews/42";
    let out = render_web_fetch_error(url, error, &mcp, None, PromptDisposition::Act);

    assert!(out.starts_with(&format!("error: {error}")), "got: {out}");
    let discovery = out.find("`tool_search`").expect("discovery instruction");
    let connector = out
        .find("opaque_bridge__read_object")
        .expect("connected namespaced MCP tool");
    assert!(
        discovery < connector,
        "tool discovery must be presented before its MCP candidate: {out}"
    );
    assert!(
        out.contains("Do not fall back to `run_command`/curl or `request_user_input`"),
        "the two field-seen dead ends must be explicitly fenced: {out}"
    );

    // Exercise the instructed route against the same live catalog: discovery
    // returns the exact MCP name, and the dispatcher invokes that remote tool
    // under an explicit persona grant. No shell or human-input tool enters the
    // sequence.
    let catalog = callable_mcp_catalog(&mcp, None, PromptDisposition::Act);
    let discovered =
        super::super::tool_search::execute_tool_search("opaque_bridge__read_object", &catalog);
    assert!(
        discovered.contains("opaque_bridge__read_object"),
        "tool_search must discover the connector: {discovered}"
    );
    let allowed = vec!["opaque_bridge__read_object".to_string()];
    let mut routed_mcp = OneRemoteTool::new("opaque_bridge__read_object")
        .with_resource_url_prefixes(&["https://reviews.example.test/reviews/"]);
    let result = run_remote_gated(
        "opaque_bridge__read_object",
        std::path::Path::new("."),
        &Caveats::top(),
        Some(&allowed),
        &mut routed_mcp,
        None,
    )
    .await;
    assert_eq!(result, "remote-tool-ran");
    assert!(routed_mcp.called, "the namespaced MCP route must dispatch");
}

/// HTTP authentication failures are structured successful transports in
/// agent-bridle. Newt must not feed their login/error body to the model as
/// page evidence; both statuses take the same MCP-first route.
#[test]
fn unauthorized_fetch_results_route_to_connected_mcp_not_error_body() {
    let mcp = OneRemoteTool::new("opaque_bridge__read_object")
        .with_resource_url_prefixes(&["https://reviews.example.test/reviews/"]);
    for status in [401_u64, 403] {
        let out = render_web_fetch_result(
            "https://reviews.example.test/reviews/42",
            &serde_json::json!({
                "status": status,
                "final_url": "https://reviews.example.test/login",
                "title": "Sign in",
                "markdown": "configure a local checkout client instead"
            }),
            &mcp,
            None,
            PromptDisposition::Act,
        );

        assert!(
            out.starts_with(&format!("error: web_fetch returned HTTP {status}")),
            "got: {out}"
        );
        assert!(out.contains("`tool_search`"), "missing discovery: {out}");
        assert!(
            out.contains("opaque_bridge__read_object"),
            "missing MCP route: {out}"
        );
        assert!(
            !out.contains("configure a local checkout client instead"),
            "an auth error body must not masquerade as review evidence: {out}"
        );
    }
}

#[test]
fn unauthorized_fetch_errors_also_route_to_connected_mcp() {
    let mcp = OneRemoteTool::new("opaque_bridge__read_object")
        .with_resource_url_prefixes(&["https://reviews.example.test/reviews/"]);
    for error in [
        "request failed with HTTP 401 Unauthorized",
        "request failed with HTTP status 403 Forbidden",
    ] {
        let out = render_web_fetch_error(
            "https://reviews.example.test/reviews/42",
            error,
            &mcp,
            None,
            PromptDisposition::Act,
        );
        assert!(out.starts_with(&format!("error: {error}")), "got: {out}");
        assert!(out.contains("`tool_search`"), "missing discovery: {out}");
        assert!(
            out.contains("opaque_bridge__read_object"),
            "missing MCP route: {out}"
        );
    }
}

/// Recovery is honest: without a callable MCP tool, Newt returns the raw
/// SSRF failure and does not claim that an authenticated route exists.
#[test]
fn private_address_fetch_without_mcp_keeps_original_failure() {
    let error = "denied: SSRF block: \"reviews.example.test\" resolved to \
                     private/loopback address 10.0.0.1 (not in the net allowlist)";
    let out = render_web_fetch_error(
        "https://reviews.example.test/reviews/42",
        error,
        &NoMcp,
        None,
        PromptDisposition::Act,
    );
    assert_eq!(out, format!("error: {error}"));
}

#[test]
fn private_address_fetch_with_undeclared_mcp_offers_non_authoritative_discovery() {
    let error = "denied: SSRF block: private address";
    // The name deliberately looks relevant. Without explicit URL affinity
    // it is only a connected-catalog discovery candidate, never an asserted
    // authenticated route.
    let mcp = OneRemoteTool::new("reviews_source__get_review");
    let out = render_web_fetch_error(
        "https://reviews.example.test/reviews/42",
        error,
        &mcp,
        None,
        PromptDisposition::Act,
    );
    assert!(out.starts_with(&format!("error: {error}")), "got: {out}");
    assert!(out.contains("non-authoritative discovery"), "got: {out}");
    assert!(out.contains("reviews_source__get_review"), "got: {out}");
    assert!(out.contains("discovery only"), "got: {out}");
    assert!(
        out.contains("do not assume that a candidate can read or authenticate"),
        "got: {out}"
    );
}

#[test]
fn discovery_query_uses_only_bounded_host_and_path_terms() {
    let url = reqwest::Url::parse(
        "https://review-broker.example.test/reviews/42?token=must-not-appear#fragment-secret",
    )
    .unwrap();
    let query = resource_url_discovery_query(&url);
    assert!(query.contains("reviews"), "got: {query}");
    assert!(query.contains("review"), "got: {query}");
    assert!(query.contains("broker"), "got: {query}");
    assert!(!query.contains("must"), "query value leaked: {query}");
    assert!(!query.contains("fragment"), "fragment leaked: {query}");
    assert!(query.split_whitespace().count() <= 8, "got: {query}");
}

#[test]
fn authoritative_recovery_lists_every_matching_tool_without_choosing_first() {
    let tools = ["review_source__get_review", "review_source__get_version"]
        .into_iter()
        .map(|name| {
            let mut definition = serde_json::json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": "Read an authenticated review resource.",
                    "parameters": {"type": "object"}
                }
            });
            preserve_mcp_resource_url_affinity(
                &mut definition,
                Some(&serde_json::json!({
                    "newt/resourceUrlPrefixes": [
                        "https://reviews.example.test/reviews/"
                    ]
                })),
            );
            definition
        })
        .collect();
    let mcp = CatalogOnlyMcp(tools);
    let out = authenticated_url_recovery(
        "error: HTTP 401 Unauthorized".to_string(),
        "https://reviews.example.test/reviews/42",
        &mcp,
        None,
        PromptDisposition::Act,
    );

    assert!(out.contains("explicitly declares one or more URL-affine tools"));
    assert!(out.contains("review_source__get_review"), "got: {out}");
    assert!(out.contains("review_source__get_version"), "got: {out}");
    assert!(!out.contains("the exact candidate name"), "got: {out}");
}

#[test]
fn resource_affinity_requires_exact_origin_and_path_boundary() {
    for declared in [
        "https://reviews.example.test/reviews",
        "https://reviews.example.test:443/reviews/",
    ] {
        let prefix = resource_url_prefix(declared).unwrap();
        for matching in [
            "https://reviews.example.test/reviews",
            "https://reviews.example.test/reviews/42",
            "https://reviews.example.test/reviews/42?version=2",
        ] {
            let url = reqwest::Url::parse(matching).unwrap();
            assert!(
                resource_url_has_prefix(&url, &prefix),
                "expected {declared} to match {matching}"
            );
        }
        for unrelated in [
            "http://reviews.example.test/reviews/42",
            "https://reviews.example.test:444/reviews/42",
            "https://reviews.example.test/reviews-extra/42",
            "https://reviews.example.test.evil/reviews/42",
        ] {
            let url = reqwest::Url::parse(unrelated).unwrap();
            assert!(
                !resource_url_has_prefix(&url, &prefix),
                "must not overmatch {declared} against {unrelated}"
            );
        }
    }
}

#[test]
fn affinity_adapter_preserves_valid_declaration_and_wire_scrubs_metadata() {
    let mut definition = serde_json::json!({
        "type": "function",
        "function": {
            "name": "opaque_bridge__read_object",
            "description": "Retrieve an object.",
            "parameters": {"type": "object"}
        }
    });
    preserve_mcp_resource_url_affinity(
        &mut definition,
        Some(&serde_json::json!({
            "newt/resourceUrlPrefixes": [
                "https://reviews.example.test/reviews/"
            ],
            "unrelated/serverMetadata": "must not cross the provider wire"
        })),
    );
    assert_eq!(
        definition["_meta"][MCP_RESOURCE_URL_PREFIXES_META_KEY],
        serde_json::json!(["https://reviews.example.test/reviews/"])
    );
    assert!(definition["_meta"]
        .get("unrelated/serverMetadata")
        .is_none());

    strip_mcp_catalog_metadata(&mut definition);
    assert!(definition.get("_meta").is_none());
    assert_eq!(definition["function"]["name"], "opaque_bridge__read_object");
}

#[test]
fn affinity_declaration_is_a_strict_nonempty_array() {
    for malformed in [
        serde_json::json!([]),
        serde_json::json!("https://reviews.example.test/reviews/"),
        serde_json::json!(["https://reviews.example.test/reviews/", 7]),
        serde_json::json!(["https://reviews.example.test/reviews/", "/reviews/42"]),
        serde_json::json!([" https://reviews.example.test/reviews/"]),
        serde_json::json!(["https://user:secret@reviews.example.test/reviews/"]),
        serde_json::json!(["https://reviews.example.test/reviews/?token=secret"]),
        serde_json::json!(["file:///tmp/reviews/"]),
    ] {
        let meta = serde_json::json!({
            "newt/resourceUrlPrefixes": malformed
        });
        let mut definition = serde_json::json!({
            "type": "function",
            "function": {
                "name": "opaque_bridge__read_object",
                "description": "Retrieve an object.",
                "parameters": {"type": "object"}
            }
        });
        preserve_mcp_resource_url_affinity(&mut definition, Some(&meta));
        assert!(
            definition.get("_meta").is_none(),
            "malformed declaration must add no affinity: {meta}"
        );

        let raw = serde_json::json!({
            "type": "function",
            "function": definition["function"].clone(),
            "_meta": meta
        });
        let url = reqwest::Url::parse("https://reviews.example.test/reviews/42").unwrap();
        assert!(
            !tool_declares_resource_url(&raw, &url),
            "raw malformed metadata must not bypass the adapter"
        );
    }
}

#[test]
fn names_and_descriptions_never_infer_resource_affinity() {
    let decoy = serde_json::json!({
        "type": "function",
        "function": {
            "name": "reviews_source__get_review",
            "description": "Read https://reviews.example.test/reviews/42",
            "parameters": {"type": "object"}
        }
    });
    let url = reqwest::Url::parse("https://reviews.example.test/reviews/42").unwrap();
    assert!(!tool_declares_resource_url(&decoy, &url));
}

#[test]
fn merged_model_catalog_scrubs_affinity_but_recovery_catalog_retains_it() {
    let mcp = OneRemoteTool::new("opaque_bridge__read_object")
        .with_resource_url_prefixes(&["https://reviews.example.test/reviews/"]);
    let recovery_catalog = callable_mcp_catalog(&mcp, None, PromptDisposition::Act);
    assert_eq!(
        recovery_catalog[0]["_meta"][MCP_RESOURCE_URL_PREFIXES_META_KEY],
        serde_json::json!(["https://reviews.example.test/reviews/"])
    );

    let model_catalog = merged_tool_definitions(
        &mcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    let remote = model_catalog
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "opaque_bridge__read_object")
        .expect("remote tool remains advertised after metadata scrubbing");
    assert!(remote.get("_meta").is_none());
}

/// Ordinary public content and unrelated transport errors keep their prior
/// behavior; the MCP route is specific to private-address/auth failures.
#[test]
fn ordinary_web_fetch_results_do_not_gain_mcp_recovery() {
    let mcp = OneRemoteTool::new("review_source__get_review");
    let ok = render_web_fetch_result(
        "https://docs.example.test/page",
        &serde_json::json!({
            "status": 200,
            "final_url": "https://docs.example.test/page",
            "title": "Guide",
            "markdown": "public content"
        }),
        &mcp,
        None,
        PromptDisposition::Act,
    );
    assert_eq!(
        ok,
        "# Guide\nhttps://docs.example.test/page\n\npublic content"
    );
    assert!(!ok.contains("tool_search"));

    let timeout = render_web_fetch_error(
        "https://docs.example.test/page",
        "denied: request to \"docs.example.test\" timed out",
        &mcp,
        None,
        PromptDisposition::Act,
    );
    assert_eq!(
        timeout,
        "error: denied: request to \"docs.example.test\" timed out"
    );
}

// -- save_note dispatch through execute_tool (Step 19.3) ----------------

#[tokio::test]
async fn save_note_without_sink_is_unknown_tool() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    // run_tool passes note_sink: None — the no-sink (headless) shape.
    let out = run_tool(
        "save_note",
        serde_json::json!({"action": "add", "text": "a fact"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.starts_with("unknown tool: save_note"), "got: {out}");
}

#[tokio::test]
async fn save_note_with_sink_routes_through_execute_tool() {
    use crate::agentic::note_sink::tests::MockSink;
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let mut sink = MockSink::default();
    let out = execute_tool(
        "save_note",
        &serde_json::json!({"action": "add", "text": "workspace builds with just check"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        Some(&mut sink),
        None,
        None, // memory_source
        None,
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_eq!(sink.calls, vec!["add:workspace builds with just check"]);
    assert!(
        out.starts_with("note saved: workspace builds"),
        "got: {out}"
    );
}

// -- recall dispatch through execute_tool (Step 17.5) -------------------

#[tokio::test]
async fn recall_without_source_is_unknown_tool() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    // run_tool passes recall_source: None — the no-store (headless) shape.
    let out = run_tool(
        "recall",
        serde_json::json!({"query": "tokio panic"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.starts_with("unknown tool: recall"), "got: {out}");
}

#[tokio::test]
async fn recall_with_source_routes_through_execute_tool() {
    use crate::agentic::recall::tests::{hit, MockSource};
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let source = MockSource {
        hits: vec![hit(
            "123456789012-abcd",
            "past work",
            3,
            ">>>tokio<<< panic",
        )],
        ..Default::default()
    };
    let out = execute_tool(
        "recall",
        &serde_json::json!({"query": "tokio panic"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        Some(&source),
        None, // memory_source
        None,
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_eq!(
        *source.calls.lock().unwrap(),
        vec![("tokio panic".to_string(), 5)]
    );
    assert!(out.contains("«tokio» panic"), "got: {out}");
    assert!(out.contains("past work"), "got: {out}");
}

// -- memory_fetch dispatch through execute_tool (#319) ------------------

/// FLAG OFF (no source): a `memory_fetch` call is treated like any unknown
/// tool — the inert-by-default shape (the tool was never advertised, so a
/// call here is a hallucination). Mirrors `recall_without_source`.
#[tokio::test]
async fn memory_fetch_without_source_is_unknown_tool() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    // run_tool passes memory_source: None — the no-source (headless) shape.
    let out = run_tool(
        "memory_fetch",
        serde_json::json!({"address": "note:1"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.starts_with("unknown tool: memory_fetch"), "got: {out}");
}

/// FLAG ON (source present): a `memory_fetch` call routes through the
/// injected `MemorySource` and returns its body. Mirrors
/// `recall_with_source_routes_through_execute_tool`.
#[tokio::test]
async fn memory_fetch_with_source_routes_through_execute_tool() {
    use crate::agentic::memory_fetch::tests::MockSource;
    use crate::agentic::MemAddr;
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let source = MockSource {
        body: Some("the exact note body".to_string()),
        ..Default::default()
    };
    let out = execute_tool(
        "memory_fetch",
        &serde_json::json!({"address": "note:1"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        Some(&source),
        None,
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_eq!(out, "the exact note body");
    assert_eq!(
        *source.calls.lock().unwrap(),
        vec![MemAddr::Note { id: "1".into() }]
    );
}
