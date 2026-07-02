//! team.rs — the **plan → task → crew** loop (the "team" front door).
//!
//! Where a [crew](crate::run_crew) solves ONE task and a [panel](crate::run_panel)
//! runs N voices on one task, a **team** takes a whole GOAL: a **lead** model
//! decomposes it into an ordered list of subtasks, and each subtask is then handed
//! to a crew. Subtasks run **sequentially over a shared workspace** (subtask N
//! builds on N-1), and the team **stops at the first blocked subtask** — a plan
//! can't proceed past a step the crew couldn't land. The aggregate is honest:
//! `AllPassed` only if every crew passed; otherwise `Blocked` with the remaining
//! subtasks marked `Skipped`.
//!
//! "Different LLM, different personas, different tasks": the lead uses its own
//! model; the crew's planner/navigator/triage use theirs (each a pinned model /
//! loadout, routed by the [`BackendPool`]); each subtask is distinct work. Pure
//! orchestration over the existing seams — unit-testable with mocks, no network.

use crate::{run_crew, BackendPool, ChatRequest, CrewConfig, CrewStatus, Dispatcher, Workspace};
use newt_core::caveats::{Caveats, CaveatsExt};
use newt_core::Tier;
use serde::{Deserialize, Serialize};

/// How a team runs: which model leads (decomposes), and the crew that executes
/// each subtask.
#[derive(Debug, Clone)]
pub struct TeamConfig {
    /// The model that decomposes the goal into subtasks.
    pub lead_model: String,
    /// The tier the lead runs at.
    pub lead_tier: Tier,
    /// The crew (planner/navigator/triage models + budget) that runs each subtask.
    pub crew: CrewConfig,
    /// Cap on how many subtasks the plan may have.
    pub max_subtasks: usize,
}

/// What happened to one subtask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtaskStatus {
    /// The crew landed it (verification passed).
    Passed,
    /// The crew exhausted its budget — escalate.
    NeedsHumanReview,
    /// Not attempted because an earlier subtask blocked the plan.
    Skipped,
}

/// One subtask's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtaskResult {
    pub subtask: String,
    pub status: SubtaskStatus,
    /// Planning rounds the crew spent (0 if skipped / not started).
    pub attempts: u32,
}

/// Terminal disposition of a team run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamStatus {
    /// Every subtask's crew passed.
    AllPassed,
    /// A subtask blocked; the rest were skipped — honest `NeedsHumanReview`.
    Blocked,
    /// The lead produced no usable plan (unreachable, or empty/garbled).
    NoPlan,
}

/// The result of a team run.
#[derive(Debug, Clone)]
pub struct TeamOutcome {
    pub status: TeamStatus,
    /// The decomposed subtasks, in order.
    pub plan: Vec<String>,
    /// Per-subtask records (same order as `plan`).
    pub results: Vec<SubtaskResult>,
}

/// A planned subtask: the work, plus an optional **per-subtask** verification
/// command (so independent subtasks each get their own check, not one shared one),
/// plus an optional leaf-scope fence (#816, mirroring crew mode's `Subtask.context`
/// from #812: model-declared, meet-only, empty = unconstrained — never touches
/// `verify`, never widens authority above the worktree/fs_write boundary it sits
/// inside). Unlike crew mode's def-site-grounded scope (#812 §"Sharpen it"), this
/// is the model's self-report ALONE — `run_team` has no repo-grep seam to ground
/// it against, and porting that hardening is tracked separately (see #816's PR).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Subtask {
    task: String,
    verify: Option<String>,
    files: Vec<String>,
}

/// The lead's entry — accepts a plain string OR a `{task, verify, files}` object,
/// so a weaker lead that emits bare strings still works (verify/files fall back to
/// the workspace's default check / an unfenced dispatch).
#[derive(Deserialize)]
#[serde(untagged)]
enum SubtaskSpec {
    Plain(String),
    Detailed {
        task: String,
        #[serde(default)]
        verify: Option<String>,
        #[serde(default)]
        files: Vec<String>,
    },
}

#[derive(Deserialize, Default)]
struct PlanOut {
    #[serde(default)]
    subtasks: Vec<SubtaskSpec>,
}

/// Run the team on `goal`: lead decomposes → a crew runs each subtask over the
/// shared `workspace`, stopping at the first block.
pub async fn run_team(
    pool: &BackendPool,
    dispatcher: &dyn Dispatcher,
    workspace: &mut dyn Workspace,
    cfg: &TeamConfig,
    caveats: &Caveats,
    goal: &str,
) -> TeamOutcome {
    // 1. DECOMPOSE — the lead breaks the goal into ordered subtasks.
    let req = ChatRequest::new()
        .system(
            "You are a tech lead. Break the GOAL into an ordered list of small, \
             independently-verifiable engineering subtasks. Reply with ONLY JSON \
             {\"subtasks\":[{\"task\":\"<imperative step>\",\"verify\":\"<a shell command \
             that exits 0 once THIS step is done; omit if there is no per-step check>\",\
             \"files\":[\"<relative path of a REAL existing file this step edits; omit \
             if unknown>\"]}]} — concrete, smallest-first.",
        )
        .user(format!(
            "GOAL:\n{goal}\n\nAt most {} subtasks.",
            cfg.max_subtasks
        ));
    let plan: Vec<Subtask> = match pool
        .run_role(dispatcher, cfg.lead_tier, &cfg.lead_model, req)
        .await
    {
        Some(f) => parse_plan(&f.result.content, cfg.max_subtasks),
        None => {
            return TeamOutcome {
                status: TeamStatus::NoPlan,
                plan: Vec::new(),
                results: Vec::new(),
            }
        }
    };
    if plan.is_empty() {
        return TeamOutcome {
            status: TeamStatus::NoPlan,
            plan: Vec::new(),
            results: Vec::new(),
        };
    }
    let task_list: Vec<String> = plan.iter().map(|s| s.task.clone()).collect();

    // 2. DISPATCH — a crew per subtask, sequential over the shared workspace,
    //    stopping at the first block (a plan can't proceed past a failed step).
    //    Each subtask installs its OWN verification command when the lead supplied
    //    one (per-subtask verify); otherwise the workspace's default check stands.
    let mut results = Vec::with_capacity(plan.len());
    let mut blocked = false;
    for st in &plan {
        if blocked {
            results.push(SubtaskResult {
                subtask: st.task.clone(),
                status: SubtaskStatus::Skipped,
                attempts: 0,
            });
            continue;
        }
        // #754 — gate the per-subtask `verify` through the exec axis (the T2
        // "verify-as-payload" vector). The `verify` is LEAD-authored, and the
        // lead is an LLM: untrusted plan input. Installed as the workspace test
        // command, the crew later runs it as a shell command (`sh -c`), so its
        // authority follows its PROVENANCE — it must be authorized by the exec
        // caveat, fail-closed, exactly as `crew_runner` gates the caller-supplied
        // top-level verify and `plan_exec` gates the plan-leaf verify.
        // `permits_exec` is exact-match, so a narrow exec scope cannot be escaped
        // by chaining ("cargo; curl" never equals "cargo"). REFUSE, not run: a
        // denied verify is NOT installed — the subtask proceeds under the
        // workspace's DEFAULT check rather than executing an un-permitted command.
        if let Some(verify) = &st.verify {
            if caveats.permits_exec(verify) {
                workspace.set_test_command(verify);
            } else {
                eprintln!(
                    "per-subtask verify refused: {verify:?} is outside the exec caveat \
                     — falling back to the workspace default check"
                );
            }
        }
        // #816: the lead-declared files list is the leaf-scope fence, threaded
        // into run_crew's meet-only `scope_permits` gate exactly as #812 threads
        // crew mode's `Subtask.context`. Empty files (a Plain entry, or a
        // Detailed entry that omitted files) ⇒ unfenced, byte-identical to
        // pre-#816 dispatch.
        let outcome = run_crew(
            pool, dispatcher, workspace, &cfg.crew, caveats, &st.task, &st.files,
        )
        .await;
        let status = match outcome.status {
            CrewStatus::Passed => SubtaskStatus::Passed,
            CrewStatus::NeedsHumanReview => {
                blocked = true;
                SubtaskStatus::NeedsHumanReview
            }
        };
        results.push(SubtaskResult {
            subtask: st.task.clone(),
            status,
            attempts: outcome.attempts,
        });
    }

    let status = if blocked {
        TeamStatus::Blocked
    } else {
        TeamStatus::AllPassed
    };
    TeamOutcome {
        status,
        plan: task_list,
        results,
    }
}

/// Parse the lead's reply into a subtask list: try the whole string as JSON, then
/// the outermost `{..}` (local models often wrap JSON in prose). Capped at `max`.
fn parse_plan(content: &str, max: usize) -> Vec<Subtask> {
    let parsed: PlanOut = serde_json::from_str(content)
        .ok()
        .or_else(|| {
            let (i, j) = (content.find('{')?, content.rfind('}')?);
            (j > i)
                .then(|| serde_json::from_str(&content[i..=j]).ok())
                .flatten()
        })
        .unwrap_or_default();
    parsed
        .subtasks
        .into_iter()
        .map(|s| match s {
            SubtaskSpec::Plain(task) => Subtask {
                task,
                verify: None,
                files: Vec::new(),
            },
            SubtaskSpec::Detailed {
                task,
                verify,
                files,
            } => Subtask {
                task,
                verify: verify.filter(|v| !v.trim().is_empty()),
                files: files
                    .into_iter()
                    .map(|f| f.trim().to_string())
                    .filter(|f| !f.is_empty())
                    .collect(),
            },
        })
        .map(|s| Subtask {
            task: s.task.trim().to_string(),
            verify: s.verify,
            files: s.files,
        })
        .filter(|s| !s.task.is_empty())
        .take(max)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatReply, Edit, Health, PoolBackend, StaticSource};
    use async_trait::async_trait;
    use newt_core::BackendKind;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// In-memory workspace: `run_test` passes iff `target.rs` contains "GOOD".
    /// Records the per-subtask verify commands the team installs.
    struct MemWs {
        files: BTreeMap<String, String>,
        verifies: Vec<String>,
    }
    impl MemWs {
        fn new() -> Self {
            let mut files = BTreeMap::new();
            files.insert("target.rs".to_string(), "BAD".to_string());
            files.insert("README.md".to_string(), "docs".to_string());
            Self {
                files,
                verifies: Vec::new(),
            }
        }
    }
    impl Workspace for MemWs {
        fn files(&self) -> Vec<String> {
            self.files.keys().cloned().collect()
        }
        fn read(&self, p: &str) -> Option<String> {
            self.files.get(p).cloned()
        }
        fn apply(&mut self, edits: &[Edit]) -> Vec<String> {
            edits
                .iter()
                .map(|e| {
                    self.files.insert(e.path.clone(), e.new_content.clone());
                    e.path.clone()
                })
                .collect()
        }
        fn run_test(&self) -> (bool, String) {
            match self.files.get("target.rs") {
                Some(c) if c.contains("GOOD") => (true, "ok".into()),
                _ => (false, "needs GOOD".into()),
            }
        }
        fn set_test_command(&mut self, cmd: &str) {
            self.verifies.push(cmd.to_string());
        }
    }

    /// Mock dispatcher: the lead returns a 2-subtask plan; the crew roles converge
    /// (planner emits GOOD) UNLESS `block` is set (planner always emits BAD).
    struct TeamMock {
        plan_json: String,
        block: bool,
        planner_calls: AtomicUsize,
    }
    #[async_trait]
    impl Dispatcher for TeamMock {
        async fn dispatch(
            &self,
            _b: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            let content = match model {
                "lead" => self.plan_json.clone(),
                "nav" => r#"{"relevant_files":["target.rs"]}"#.to_string(),
                "triage" => r#"{"summary":"missing GOOD","next_action":"set GOOD"}"#.to_string(),
                "planner" => {
                    let n = self.planner_calls.fetch_add(1, Ordering::SeqCst);
                    // block => never GOOD; else GOOD from the first attempt.
                    if self.block {
                        r#"{"edits":[{"path":"target.rs","new_content":"BAD"}]}"#.to_string()
                    } else {
                        let _ = n;
                        r#"{"edits":[{"path":"target.rs","new_content":"GOOD"}]}"#.to_string()
                    }
                }
                other => panic!("unexpected model {other}"),
            };
            Ok(ChatReply {
                content,
                model_id: model.to_string(),
                usage: None,
            })
        }
    }

    fn pool() -> BackendPool {
        BackendPool::from_source(&StaticSource {
            backends: vec![
                PoolBackend::new("dgx", "http://dgx:11434", BackendKind::Ollama)
                    .with_models(["lead", "nav", "planner", "triage"])
                    .with_health(Health::Up),
            ],
        })
    }

    fn cfg() -> TeamConfig {
        TeamConfig {
            lead_model: "lead".into(),
            lead_tier: Tier::Complex,
            crew: CrewConfig {
                navigator_model: "nav".into(),
                planner_model: "planner".into(),
                triage_model: "triage".into(),
                max_attempts: 2,
                role_timeout: None,
            },
            max_subtasks: 5,
        }
    }

    #[tokio::test]
    async fn decomposes_and_runs_every_subtask() {
        let p = pool();
        let d = TeamMock {
            plan_json: r#"{"subtasks":["do A","do B"]}"#.into(),
            block: false,
            planner_calls: AtomicUsize::new(0),
        };
        let mut ws = MemWs::new();
        let out = run_team(
            &p,
            &d,
            &mut ws,
            &cfg(),
            &newt_core::caveats::Caveats::top(),
            "build the thing",
        )
        .await;
        assert_eq!(out.status, TeamStatus::AllPassed);
        assert_eq!(out.plan, vec!["do A".to_string(), "do B".to_string()]);
        assert!(out
            .results
            .iter()
            .all(|r| r.status == SubtaskStatus::Passed));
    }

    #[tokio::test]
    async fn blocks_and_skips_the_rest() {
        let p = pool();
        let d = TeamMock {
            plan_json: r#"prose {"subtasks":["do A","do B","do C"]} more"#.into(),
            block: true, // the crew never converges -> first subtask blocks
            planner_calls: AtomicUsize::new(0),
        };
        let mut ws = MemWs::new();
        let out = run_team(
            &p,
            &d,
            &mut ws,
            &cfg(),
            &newt_core::caveats::Caveats::top(),
            "goal",
        )
        .await;
        assert_eq!(out.status, TeamStatus::Blocked);
        assert_eq!(out.results[0].status, SubtaskStatus::NeedsHumanReview);
        assert_eq!(out.results[1].status, SubtaskStatus::Skipped);
        assert_eq!(out.results[2].status, SubtaskStatus::Skipped);
    }

    #[tokio::test]
    async fn no_plan_when_lead_unreachable() {
        // Pool serves the crew models but NOT "lead" -> run_role None -> NoPlan.
        let p = BackendPool::from_source(&StaticSource {
            backends: vec![PoolBackend::new("x", "http://x:11434", BackendKind::Ollama)
                .with_models(["nav", "planner", "triage"])
                .with_health(Health::Up)],
        });
        let d = TeamMock {
            plan_json: String::new(),
            block: false,
            planner_calls: AtomicUsize::new(0),
        };
        let mut ws = MemWs::new();
        let out = run_team(
            &p,
            &d,
            &mut ws,
            &cfg(),
            &newt_core::caveats::Caveats::top(),
            "goal",
        )
        .await;
        assert_eq!(out.status, TeamStatus::NoPlan);
        assert!(out.results.is_empty());
    }

    #[tokio::test]
    async fn installs_per_subtask_verify_commands() {
        // The lead emits {task, verify} objects → run_team installs each subtask's
        // own verification on the workspace before running its crew.
        let p = pool();
        let d = TeamMock {
            plan_json: r#"{"subtasks":[
                {"task":"do A","verify":"check-a"},
                {"task":"do B","verify":"check-b"}
            ]}"#
            .into(),
            block: false,
            planner_calls: AtomicUsize::new(0),
        };
        let mut ws = MemWs::new();
        let out = run_team(
            &p,
            &d,
            &mut ws,
            &cfg(),
            &newt_core::caveats::Caveats::top(),
            "goal",
        )
        .await;
        assert_eq!(out.status, TeamStatus::AllPassed);
        assert_eq!(out.plan, vec!["do A".to_string(), "do B".to_string()]);
        assert_eq!(
            ws.verifies,
            vec!["check-a".to_string(), "check-b".to_string()]
        );
    }

    #[tokio::test]
    async fn denied_per_subtask_verify_is_refused_not_installed() {
        // #754 (T2 "verify-as-payload"): the per-subtask `verify` is LEAD-authored
        // — the lead is an LLM, so it is untrusted plan input. The exec caveat here
        // permits "check-a" but NOT "check-b". The permitted verify IS installed;
        // the denied one is REFUSED, not run — it is absent from the recorded
        // commands, so that subtask falls back to the workspace's default check
        // instead of executing an un-permitted shell command.
        //
        // RED on pre-#754 code: `set_test_command` was called unconditionally, so
        // BOTH "check-a" and "check-b" were recorded. GREEN after: only "check-a".
        let p = pool();
        let d = TeamMock {
            plan_json: r#"{"subtasks":[
                {"task":"do A","verify":"check-a"},
                {"task":"do B","verify":"check-b"}
            ]}"#
            .into(),
            block: false,
            planner_calls: AtomicUsize::new(0),
        };
        let mut ws = MemWs::new();
        // Exec authority covers ONLY "check-a"; "check-b" is outside the caveat.
        let mut caveats = newt_core::caveats::Caveats::top();
        caveats.exec = newt_core::caveats::Scope::only(["check-a".to_string()]);
        let out = run_team(&p, &d, &mut ws, &cfg(), &caveats, "goal").await;
        // The plan still ran (refuse-not-run: the denied subtask proceeds under the
        // default check, which the converging crew passes).
        assert_eq!(out.status, TeamStatus::AllPassed);
        assert_eq!(out.plan, vec!["do A".to_string(), "do B".to_string()]);
        // Only the permitted verify was installed; the denied one was refused.
        assert_eq!(ws.verifies, vec!["check-a".to_string()]);
    }

    /// Lead emits one subtask; the planner always emits edits to BOTH
    /// `target.rs` (the intended fix) and `README.md` (over-reach), regardless
    /// of scope. `scoped` toggles whether the lead's entry declares
    /// `files:["target.rs"]` — the #816 fence must let the fix land and
    /// refuse the over-reach when scoped, and land both (unfenced,
    /// byte-identical to pre-#816) when not.
    struct OverreachTeamMock {
        scoped: bool,
    }
    #[async_trait]
    impl Dispatcher for OverreachTeamMock {
        async fn dispatch(
            &self,
            _b: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            let content = match model {
                "lead" if self.scoped => {
                    r#"{"subtasks":[{"task":"fix target.rs","files":["target.rs"]}]}"#.to_string()
                }
                "lead" => r#"{"subtasks":[{"task":"fix target.rs"}]}"#.to_string(),
                "nav" => r#"{"relevant_files":["target.rs","README.md"]}"#.to_string(),
                "triage" => r#"{"summary":"s","next_action":"n"}"#.to_string(),
                "planner" => r#"{"edits":[
                    {"path":"target.rs","new_content":"GOOD"},
                    {"path":"README.md","new_content":"hacked"}
                ]}"#
                .to_string(),
                other => panic!("unexpected model {other}"),
            };
            Ok(ChatReply {
                content,
                model_id: model.to_string(),
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn scoped_subtask_fences_out_of_scope_edits() {
        // #816: a lead-declared `files` list fences the team-dispatched crew
        // exactly like #812 fences a plan leaf. The in-scope edit lands
        // (verify goes green); the out-of-scope edit is refused.
        let p = pool();
        let d = OverreachTeamMock { scoped: true };
        let mut ws = MemWs::new();
        let out = run_team(
            &p,
            &d,
            &mut ws,
            &cfg(),
            &newt_core::caveats::Caveats::top(),
            "goal",
        )
        .await;
        assert_eq!(out.status, TeamStatus::AllPassed);
        assert_eq!(ws.read("target.rs").as_deref(), Some("GOOD"));
        assert_eq!(
            ws.read("README.md").as_deref(),
            Some("docs"),
            "out-of-scope edit must be refused, not landed"
        );
    }

    #[tokio::test]
    async fn unscoped_subtask_is_unfenced_same_as_before_816() {
        // Same OverreachTeamMock, but the lead omits `files` -> Subtask.files
        // is empty -> no fence -> the over-reach edit lands too, proving the
        // fence (not the mock) makes the difference, and that omitted-files
        // dispatch is byte-identical to pre-#816 team dispatch.
        let p = pool();
        let d = OverreachTeamMock { scoped: false };
        let mut ws = MemWs::new();
        let out = run_team(
            &p,
            &d,
            &mut ws,
            &cfg(),
            &newt_core::caveats::Caveats::top(),
            "goal",
        )
        .await;
        assert_eq!(out.status, TeamStatus::AllPassed);
        assert_eq!(ws.read("target.rs").as_deref(), Some("GOOD"));
        assert_eq!(
            ws.read("README.md").as_deref(),
            Some("hacked"),
            "unscoped dispatch must remain unfenced"
        );
    }

    #[tokio::test]
    async fn empty_plan_is_no_plan() {
        let p = pool();
        let d = TeamMock {
            plan_json: r#"{"subtasks":[]}"#.into(),
            block: false,
            planner_calls: AtomicUsize::new(0),
        };
        let mut ws = MemWs::new();
        let out = run_team(
            &p,
            &d,
            &mut ws,
            &cfg(),
            &newt_core::caveats::Caveats::top(),
            "goal",
        )
        .await;
        assert_eq!(out.status, TeamStatus::NoPlan);
    }
}
