//! crew.rs — the role-routing **control loop**.
//!
//! This is the top of the scheduler's trait stack: it orchestrates the crew
//! (navigate → curate → plan → apply → verify → triage → revise) over the three
//! seams the rest of the crate exposes — [`BackendPool`](crate::BackendPool)
//! (placement/health), [`Dispatcher`](crate::Dispatcher) (the swappable inference
//! strategy), and the new [`Workspace`] (the effects side). Because **both** I/O
//! sides are injected traits, the whole loop — including the triage→revise
//! convergence the live runs never exercised — is unit-testable with mocks and no
//! network.
//!
//! It is a faithful Rust port of the empirically-validated
//! `experiments/crew-mvp/crew_repo.py` two-pass machine: a navigator curates
//! context, a planner emits full-file edits, the **harness** (not the model) runs
//! the verification, and on failure a triage role feeds a diagnosis back into the
//! next planning round. The harness owning test execution is guardrail #3 from the
//! crew design — the model requests a check, it never reports the result.
//!
//! The loop is itself a strategy seam: `run_crew` is a free function over the
//! traits, so a use case can swap the *control program* (this linear two-pass loop
//! ↔ a future panel/tournament) the same way it swaps the `Dispatcher` transport.

use crate::{BackendPool, ChatRequest, Dispatcher};
use newt_core::Tier;
use serde::Deserialize;

/// A targeted edit: the full new content for one file (created if absent). The
/// crew-MVP found whole-file edits the most robust shape for local models — no
/// fragile patch/hunk arithmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub path: String,
    pub new_content: String,
}

/// Terminal disposition of a crew run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrewStatus {
    /// The harness's verification passed.
    Passed,
    /// Attempts were exhausted without a green check — escalate to a human (or a
    /// stronger loadout). Never reported as success: an honest cap-exit.
    NeedsHumanReview,
}

/// The result of running the crew on a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewOutcome {
    pub status: CrewStatus,
    /// Planning rounds spent (0 if the crew could not even start — no backend).
    pub attempts: u32,
    /// Paths the workspace reports as written, from the last applied plan.
    pub touched: Vec<String>,
}

/// The effects side of the loop, injected so the orchestration stays pure.
///
/// A real implementation operates on an **isolated** worktree (never the live
/// tree — the adversarial-review guardrail), and `run_test` shells out to the
/// task's verification command. The mock in tests is in-memory.
pub trait Workspace: Send {
    /// Candidate paths the navigator may consider.
    fn files(&self) -> Vec<String>;
    /// Read one file's content, or `None` if it does not exist.
    fn read(&self, path: &str) -> Option<String>;
    /// Apply edits; returns the paths actually written.
    fn apply(&mut self, edits: &[Edit]) -> Vec<String>;
    /// Run the verification command: `(passed, captured_output)`.
    fn run_test(&self) -> (bool, String);
}

/// Which model each role is pinned to (the [`BackendPool`] routes by these).
/// Mirrors the gnuc+DGX crew loadout: a strong planner, a mid navigator, a small
/// fast triage.
#[derive(Debug, Clone)]
pub struct CrewConfig {
    pub navigator_model: String,
    pub planner_model: String,
    pub triage_model: String,
    /// Maximum planning rounds before an honest `NeedsHumanReview` cap-exit.
    pub max_attempts: u32,
}

// --- role output contracts (parsed from each role's JSON reply) ---------------

#[derive(Deserialize, Default)]
struct NavOut {
    #[serde(default)]
    relevant_files: Vec<String>,
}

#[derive(Deserialize, Default)]
struct PlanOut {
    #[serde(default)]
    edits: Vec<EditOut>,
}

#[derive(Deserialize)]
struct EditOut {
    path: String,
    new_content: String,
}

#[derive(Deserialize, Default)]
struct TriageOut {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    next_action: String,
}

/// Robustly parse a model reply as JSON `T`: try the whole string, then fall back
/// to the outermost `{..}` block (local models often wrap JSON in prose). Returns
/// `T::default()` if nothing parses — the loop degrades (empty plan ⇒ a failed
/// verify ⇒ triage), it never panics on a malformed reply.
fn parse<T: serde::de::DeserializeOwned + Default>(content: &str) -> T {
    if let Ok(v) = serde_json::from_str::<T>(content) {
        return v;
    }
    if let (Some(i), Some(j)) = (content.find('{'), content.rfind('}')) {
        if j > i {
            if let Ok(v) = serde_json::from_str::<T>(&content[i..=j]) {
                return v;
            }
        }
    }
    T::default()
}

/// Run the crew's two-pass control loop on `task`, returning the outcome.
///
/// `None` from [`run_role`](BackendPool::run_role) (nothing live serves a pinned
/// model) is a hard stop, not a silent skip: navigator-unavailable ⇒
/// `NeedsHumanReview` with `attempts: 0`; planner-unavailable mid-loop ⇒
/// `NeedsHumanReview` at the current attempt. Triage-unavailable is non-fatal — the
/// next round simply plans without a fresh diagnosis.
pub async fn run_crew(
    pool: &BackendPool,
    dispatcher: &dyn Dispatcher,
    workspace: &mut dyn Workspace,
    cfg: &CrewConfig,
    task: &str,
) -> CrewOutcome {
    // 1. NAVIGATE — pick the relevant files (then the harness reads them).
    let nav_req = ChatRequest::new()
        .system(
            "You are a repository navigator. Reply with ONLY JSON \
             {\"relevant_files\":[\"path\", ...]} listing the files needed to do the task.",
        )
        .user(format!(
            "TASK:\n{task}\n\nAVAILABLE FILES:\n{:?}",
            workspace.files()
        ));
    let nav: NavOut = match pool
        .run_role(dispatcher, Tier::Standard, &cfg.navigator_model, nav_req)
        .await
    {
        Some(f) => parse(&f.result.content),
        None => {
            return CrewOutcome {
                status: CrewStatus::NeedsHumanReview,
                attempts: 0,
                touched: Vec::new(),
            }
        }
    };

    // 2. CURATE — the harness reads only the navigator-selected, existing files.
    let curated: String = nav
        .relevant_files
        .iter()
        .filter_map(|f| workspace.read(f).map(|c| format!("=== {f} ===\n{c}")))
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut failures: Vec<String> = Vec::new();
    let mut touched: Vec<String> = Vec::new();

    for attempt in 1..=cfg.max_attempts {
        // 3. PLAN — emit full-file edits, told about the prior failure if any.
        let prior = match failures.last() {
            Some(f) => format!("\n\nThe previous attempt FAILED verification:\n{f}\nFix it."),
            None => String::new(),
        };
        let plan_req = ChatRequest::new()
            .system(
                "You are a senior engineer. Reply with ONLY JSON \
                 {\"edits\":[{\"path\":\"..\",\"new_content\":\"<FULL new file content>\"}]}.",
            )
            .user(format!(
                "TASK:\n{task}\n\nRELEVANT FILES:\n{curated}{prior}"
            ));
        let plan: PlanOut = match pool
            .run_role(dispatcher, Tier::Complex, &cfg.planner_model, plan_req)
            .await
        {
            Some(f) => parse(&f.result.content),
            None => {
                return CrewOutcome {
                    status: CrewStatus::NeedsHumanReview,
                    attempts: attempt,
                    touched,
                }
            }
        };
        let edits: Vec<Edit> = plan
            .edits
            .into_iter()
            .map(|e| Edit {
                path: e.path,
                new_content: e.new_content,
            })
            .collect();

        // 4. APPLY (isolated worktree) + 5. VERIFY (harness runs the check).
        touched = workspace.apply(&edits);
        let (ok, output) = workspace.run_test();
        if ok {
            return CrewOutcome {
                status: CrewStatus::Passed,
                attempts: attempt,
                touched,
            };
        }

        // 6. TRIAGE — diagnose the failure; fed into the next planning round.
        let tri_req = ChatRequest::new()
            .system(
                "You are a build cop. Reply with ONLY JSON \
                 {\"summary\":\"what failed\",\"next_action\":\"what to change\"}.",
            )
            .user(format!("TASK:\n{task}\n\nVERIFICATION OUTPUT:\n{output}"));
        let tri: TriageOut = match pool
            .run_role(dispatcher, Tier::Fast, &cfg.triage_model, tri_req)
            .await
        {
            Some(f) => parse(&f.result.content),
            None => TriageOut::default(),
        };
        failures.push(format!("{} -> {}", tri.summary, tri.next_action));
    }

    // 7. CAP-EXIT — honest: attempts exhausted, never reported as success.
    CrewOutcome {
        status: CrewStatus::NeedsHumanReview,
        attempts: cfg.max_attempts,
        touched,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatReply, Health, PoolBackend, StaticSource};
    use async_trait::async_trait;
    use newt_core::BackendKind;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// In-memory workspace. `run_test` passes iff `target.rs` contains "GOOD".
    struct MemWs {
        files: BTreeMap<String, String>,
    }
    impl MemWs {
        fn new() -> Self {
            let mut files = BTreeMap::new();
            files.insert("target.rs".to_string(), "BAD".to_string());
            files.insert("README.md".to_string(), "docs".to_string());
            Self { files }
        }
    }
    impl Workspace for MemWs {
        fn files(&self) -> Vec<String> {
            self.files.keys().cloned().collect()
        }
        fn read(&self, path: &str) -> Option<String> {
            self.files.get(path).cloned()
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
                Some(c) if c.contains("GOOD") => (true, "ok: 1 passed".into()),
                _ => (false, "FAILED: target.rs must contain GOOD".into()),
            }
        }
    }

    /// Role-aware mock: keys canned JSON off the pinned model. The planner emits a
    /// BAD edit on its first call and a GOOD edit thereafter — driving the
    /// triage→revise convergence deterministically.
    struct RoleMock {
        planner_calls: AtomicUsize,
    }
    impl RoleMock {
        fn new() -> Self {
            Self {
                planner_calls: AtomicUsize::new(0),
            }
        }
    }
    #[async_trait]
    impl Dispatcher for RoleMock {
        async fn dispatch(
            &self,
            _backend: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            let content = match model {
                "nav" => r#"{"relevant_files":["target.rs"]}"#.to_string(),
                "triage" => r#"{"summary":"target.rs is BAD","next_action":"set content to GOOD"}"#
                    .to_string(),
                "planner" => {
                    let n = self.planner_calls.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        r#"prose... {"edits":[{"path":"target.rs","new_content":"BAD"}]}"#
                            .to_string()
                    } else {
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

    fn cfg(max_attempts: u32) -> CrewConfig {
        CrewConfig {
            navigator_model: "nav".into(),
            planner_model: "planner".into(),
            triage_model: "triage".into(),
            max_attempts,
        }
    }

    /// One backend serving all three role models at every tier.
    fn pool() -> BackendPool {
        BackendPool::from_source(&StaticSource {
            backends: vec![
                PoolBackend::new("dgx", "http://dgx:11434", BackendKind::Ollama)
                    .with_models(["nav", "planner", "triage"])
                    .with_health(Health::Up),
            ],
        })
    }

    #[tokio::test]
    async fn converges_after_a_failed_first_attempt() {
        // attempt 1: planner -> BAD -> verify fails -> triage; attempt 2: planner ->
        // GOOD -> verify passes. This is the revise path the live runs never reached.
        let p = pool();
        let d = RoleMock::new();
        let mut ws = MemWs::new();
        let out = run_crew(&p, &d, &mut ws, &cfg(3), "make target.rs GOOD").await;
        assert_eq!(out.status, CrewStatus::Passed);
        assert_eq!(out.attempts, 2);
        assert_eq!(out.touched, vec!["target.rs".to_string()]);
        assert_eq!(ws.read("target.rs").as_deref(), Some("GOOD"));
    }

    #[tokio::test]
    async fn honest_cap_exit_when_attempts_exhausted() {
        // max_attempts=1 only ever produces the BAD plan -> never green.
        let p = pool();
        let d = RoleMock::new();
        let mut ws = MemWs::new();
        let out = run_crew(&p, &d, &mut ws, &cfg(1), "make target.rs GOOD").await;
        assert_eq!(out.status, CrewStatus::NeedsHumanReview);
        assert_eq!(out.attempts, 1);
    }

    #[tokio::test]
    async fn needs_review_when_no_backend_serves_the_navigator() {
        // Pool serves only "planner" — the navigator model is unroutable.
        let p = BackendPool::from_source(&StaticSource {
            backends: vec![PoolBackend::new("x", "http://x:11434", BackendKind::Ollama)
                .with_models(["planner"])
                .with_health(Health::Up)],
        });
        let d = RoleMock::new();
        let mut ws = MemWs::new();
        let out = run_crew(&p, &d, &mut ws, &cfg(3), "task").await;
        assert_eq!(out.status, CrewStatus::NeedsHumanReview);
        assert_eq!(out.attempts, 0);
    }
}
