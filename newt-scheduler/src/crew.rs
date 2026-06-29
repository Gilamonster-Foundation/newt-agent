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
use newt_core::caveats::{Caveats, CaveatsExt};
use newt_core::lazy_emission::lazy_emission_reason;
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
    /// Swap the verification command — used by the [team](crate::run_team) loop to
    /// give each subtask its **own** check (per-subtask verify). Default no-op, so
    /// a fixed-verification workspace (most mocks) is unaffected.
    fn set_test_command(&mut self, _cmd: &str) {}
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
    /// Per-role dispatch wall-clock bound (#695). `None` ⇒ the env/default
    /// (`role_dispatch_timeout`). Settable from the crew config so a slow
    /// loadout can widen it without an env var (review on #698).
    pub role_timeout: Option<std::time::Duration>,
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

/// Parse a planner emission into whole-file [`Edit`]s, robustly.
///
/// Prefers a **marker block** format — content written RAW, with no escaping:
/// ```text
/// FILE: <relative/path>
/// <full updated file content>
/// END-FILE
/// ```
/// This is the shape the standalone coder uses and it lands reliably. The legacy
/// `{"edits":[{"path","new_content"}]}` JSON shape is **escape-fragile**: a model
/// embedding multi-line code in a JSON string routinely leaves newlines/quotes
/// unescaped, which fails strict JSON parsing and silently drops *every* edit (the
/// crew then "completes" having delivered nothing). We fall back to JSON only when
/// no marker block is present, so valid-JSON emitters keep working.
fn parse_edits(content: &str) -> Vec<Edit> {
    let blocks = parse_file_blocks(content);
    if !blocks.is_empty() {
        return blocks;
    }
    parse::<PlanOut>(content)
        .edits
        .into_iter()
        .map(|e| Edit {
            path: e.path,
            new_content: e.new_content,
        })
        .collect()
}

/// Extract `FILE: <path>` / `END-FILE` blocks; the body between them is the file
/// content verbatim (no unescaping). Surrounding prose is ignored. A block with no
/// closing `END-FILE` is dropped (incomplete emission), never half-applied.
fn parse_file_blocks(content: &str) -> Vec<Edit> {
    let mut edits = Vec::new();
    let mut lines = content.lines();
    while let Some(line) = lines.next() {
        let path = match line.strip_prefix("FILE:") {
            Some(p) => p.trim().to_string(),
            None => continue,
        };
        if path.is_empty() {
            continue;
        }
        let mut body: Vec<&str> = Vec::new();
        let mut closed = false;
        for l in lines.by_ref() {
            if l.trim() == "END-FILE" {
                closed = true;
                break;
            }
            body.push(l);
        }
        if closed {
            edits.push(Edit {
                path,
                new_content: body.join("\n"),
            });
        }
    }
    edits
}

/// Heuristic: does this leaf instruction plausibly require a CODE CHANGE? Defaults
/// to TRUE (so a zero-edit attempt is re-prompted, #701) UNLESS the task is clearly
/// verify-only — a verify/validate verb with no change verb — so a validate leaf is
/// never goaded into a spurious edit (the #701 adversarial review).
fn task_requires_change(task: &str) -> bool {
    let t = task.to_ascii_lowercase();
    const CHANGE: &[&str] = &[
        "add",
        "modify",
        "implement",
        "refactor",
        "create",
        "write",
        "fix",
        "change",
        "update",
        "remove",
        "rename",
        "replace",
        "introduce",
        "build",
        "edit",
        "delete",
        "rewrite",
        "wire",
        "extract",
    ];
    const VERIFY: &[&str] = &["ensure", "verify", "validate", "confirm"];
    let has_change = CHANGE.iter().any(|v| t.contains(*v));
    let has_verify = VERIFY.iter().any(|v| t.contains(*v));
    // Re-prompt by default; skip ONLY a clearly verify-only task.
    has_change || !has_verify
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
    caveats: &Caveats,
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
    // #698: per-role dispatch bound — the crew config's `role_timeout` if set,
    // else the env/default (`NEWT_ROLE_TIMEOUT_SECS` → 600s).
    let role_bound = cfg
        .role_timeout
        .unwrap_or_else(crate::dispatch::role_dispatch_timeout);
    let nav: NavOut = match pool
        .run_role_with_timeout(
            dispatcher,
            Tier::Standard,
            &cfg.navigator_model,
            nav_req,
            role_bound,
        )
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

    // 2. CURATE — the harness reads only the navigator-selected, existing files,
    //    AND only those the `fs_read` leash permits (ROADMAP 23.1 / #752): complete
    //    mediation for the READ axis, mirroring the `fs_write` partition at apply
    //    below. A clamped `fs_read` caveat must have teeth — a denied file is NEVER
    //    read, and the denied set is surfaced to the crew honestly, so the algebra's
    //    read narrowing fails VISIBLY (a note in the context) rather than silently.
    let (readable, denied_read): (Vec<&str>, Vec<&str>) = nav
        .relevant_files
        .iter()
        .map(String::as_str)
        .partition(|f| caveats.permits_fs_read(f));
    let mut curated: String = readable
        .iter()
        .filter_map(|f| workspace.read(f).map(|c| format!("=== {f} ===\n{c}")))
        .collect::<Vec<_>>()
        .join("\n\n");
    if !denied_read.is_empty() {
        let note = format!(
            "{} file(s) not readable under your fs_read caveat: {}",
            denied_read.len(),
            denied_read.join(", ")
        );
        curated = if curated.is_empty() {
            note
        } else {
            format!("{curated}\n\n{note}")
        };
    }

    let mut failures: Vec<String> = Vec::new();
    let mut touched: Vec<String> = Vec::new();
    let mut reprompted_zero_edit = false;

    for attempt in 1..=cfg.max_attempts {
        // 3. PLAN — emit full-file edits, told about the prior failure if any.
        let prior = match failures.last() {
            Some(f) => format!("\n\nThe previous attempt FAILED verification:\n{f}\nFix it."),
            None => String::new(),
        };
        let plan_req = ChatRequest::new()
            .system(
                "You are a senior engineer implementing a change. For EACH file you \
                 modify, emit the COMPLETE updated file in EXACTLY this block format \
                 — no diffs, no JSON, no code fences, no prose, no explanation:\n\
                 FILE: <relative/path>\n\
                 <the full, updated file content, verbatim>\n\
                 END-FILE\n\
                 Repeat the block for every changed file. Write the file content \
                 RAW between the markers — do NOT escape it. Emit the COMPLETE \
                 file — NEVER a diff, an ellipsis, or a placeholder such as \
                 '<the full file content remains unchanged>' or '… rest of the \
                 file unchanged …'. If a file is unchanged, omit its block.",
            )
            .user(format!(
                "TASK:\n{task}\n\nRELEVANT FILES:\n{curated}{prior}"
            ));
        let edits: Vec<Edit> = match pool
            .run_role_with_timeout(
                dispatcher,
                Tier::Complex,
                &cfg.planner_model,
                plan_req,
                role_bound,
            )
            .await
        {
            Some(f) => parse_edits(&f.result.content),
            None => {
                return CrewOutcome {
                    status: CrewStatus::NeedsHumanReview,
                    attempts: attempt,
                    touched,
                }
            }
        };

        // 4. APPLY (isolated worktree) + 5. VERIFY (harness runs the check).
        //    Per-member authority (ROADMAP 23.1): only edits the leash permits land;
        //    out-of-`fs_write` edits are REFUSED (attenuation, never amplify) and fed
        //    back, so a crew member cannot write outside its granted scope — even in
        //    the isolated worktree. Verification stays ground truth.
        let (allowed, refused): (Vec<Edit>, Vec<Edit>) = edits
            .into_iter()
            .partition(|e| caveats.permits_fs_write(&e.path));
        // Refuse lazy / elided emissions BEFORE apply (#688): applying a
        // `<the full file content remains unchanged>` placeholder silently
        // overwrites real code and only surfaces downstream as a compile error.
        // A lazy body is never applied — feed back a deterministic repair and
        // retry, so the file keeps its real content.
        let (clean, lazy): (Vec<Edit>, Vec<Edit>) = allowed
            .into_iter()
            .partition(|e| lazy_emission_reason(&e.new_content).is_none());
        touched = workspace.apply(&clean);
        if !lazy.is_empty() {
            let reasons: Vec<String> = lazy
                .iter()
                .map(|e| {
                    let why = lazy_emission_reason(&e.new_content).unwrap_or_default();
                    format!("{} ({why})", e.path)
                })
                .collect();
            failures.push(format!(
                "LAZY/ELIDED EMISSION refused — these files were NOT modified; \
                 re-emit each as the COMPLETE file verbatim, with NO '<…>' or \
                 '…unchanged…' placeholders: {}",
                reasons.join("; ")
            ));
            continue;
        }
        // #701: a CHANGE-required leaf that landed NO edits (and nothing was
        // leash-refused) would pass verify VACUOUSLY on the unchanged tree and
        // deliver nothing — the #548 retest failure mode (the model located the
        // code but emitted no edit). Re-prompt ONCE for the actual edit before
        // accepting that no-op pass. `task_requires_change` skips a CLEARLY
        // verify-only leaf so the re-prompt can't goad it into a spurious edit.
        if touched.is_empty()
            && refused.is_empty()
            && !reprompted_zero_edit
            && attempt < cfg.max_attempts
            && task_requires_change(task)
        {
            reprompted_zero_edit = true;
            failures.push(
                "Your reply landed NO file edits. If this task requires changing \
                 code, emit the COMPLETE file(s) in the edits JSON now — emit the \
                 change itself, do not just describe it. If NO code change is needed \
                 (a verify-only task), reply again with no edits to confirm."
                    .to_string(),
            );
            continue;
        }
        let (ok, output) = workspace.run_test();
        if ok {
            return CrewOutcome {
                status: CrewStatus::Passed,
                attempts: attempt,
                touched,
            };
        }
        let output = if refused.is_empty() {
            output
        } else {
            let names: Vec<&str> = refused.iter().map(|e| e.path.as_str()).collect();
            format!(
                "REFUSED (outside the fs_write leash — attenuate the task or widen the grant): {}\n{output}",
                names.join(", ")
            )
        };

        // 6. TRIAGE — diagnose the failure; fed into the next planning round.
        let tri_req = ChatRequest::new()
            .system(
                "You are a build cop. Reply with ONLY JSON \
                 {\"summary\":\"what failed\",\"next_action\":\"what to change\"}.",
            )
            .user(format!("TASK:\n{task}\n\nVERIFICATION OUTPUT:\n{output}"));
        let tri: TriageOut = match pool
            .run_role_with_timeout(
                dispatcher,
                Tier::Fast,
                &cfg.triage_model,
                tri_req,
                role_bound,
            )
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

    #[test]
    fn strict_json_parse_drops_unescaped_multiline_content() {
        // THE BUG: a model embedding a full file in a JSON string routinely leaves
        // real newlines unescaped — invalid JSON — so the strict parse drops EVERY
        // edit and the crew silently delivers nothing. (Real newlines below, the
        // shape a local model actually emits.)
        let emission =
            "{\"edits\":[{\"path\":\"src/x.rs\",\"new_content\":\"fn a() {\n  ok\n}\"}]}";
        let p: PlanOut = parse(emission);
        assert_eq!(
            p.edits.len(),
            0,
            "unescaped multiline content must fail strict JSON → zero edits (the bug)"
        );
    }

    #[test]
    fn parse_edits_accepts_marker_blocks_with_raw_multiline() {
        // THE FIX: FILE:/END-FILE markers carry content RAW — no escaping — so a
        // multi-line file lands as one edit even though the same content fails JSON.
        let emission = "FILE: src/x.rs\nfn a() {\n  ok\n}\nEND-FILE";
        let edits = parse_edits(emission);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, "src/x.rs");
        assert_eq!(edits[0].new_content, "fn a() {\n  ok\n}");
    }

    #[test]
    fn parse_edits_ignores_surrounding_prose_and_drops_unclosed_blocks() {
        // Prose around the block is ignored; a block with no END-FILE is dropped
        // (never half-applied).
        let with_prose = "Sure, here is the fix:\nFILE: a.rs\nok\nEND-FILE\nDone!";
        assert_eq!(parse_edits(with_prose).len(), 1);
        let unclosed = "FILE: a.rs\nincomplete content with no terminator";
        assert_eq!(parse_file_blocks(unclosed).len(), 0);
    }

    #[test]
    fn parse_edits_falls_back_to_valid_json() {
        // Models that DO emit valid JSON keep working via the fallback.
        let emission = "{\"edits\":[{\"path\":\"a.rs\",\"new_content\":\"ok\"}]}";
        let edits = parse_edits(emission);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_content, "ok");
    }

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
        /// Already-passing workspace (`target.rs` = GOOD) — models a verify-only
        /// leaf whose check is green with no edits needed.
        fn good() -> Self {
            let mut files = BTreeMap::new();
            files.insert("target.rs".to_string(), "GOOD".to_string());
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
            role_timeout: None,
        }
    }

    /// A dispatch that never returns — models a hung role (#698 test).
    struct HangingDispatcher;
    #[async_trait]
    impl Dispatcher for HangingDispatcher {
        async fn dispatch(
            &self,
            _backend: &PoolBackend,
            _model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            tokio::time::sleep(std::time::Duration::from_secs(99_999)).await;
            unreachable!("a hung dispatch must be cancelled by the role timeout")
        }
    }

    #[tokio::test]
    async fn role_timeout_from_config_bounds_a_hung_dispatch() {
        // #698: the crew config's role_timeout bounds a hung role dispatch — the
        // navigator times out, so the crew exits honestly (NeedsHumanReview) fast
        // instead of hanging on the model.
        let p = pool();
        let mut ws = MemWs::new();
        let cc = CrewConfig {
            role_timeout: Some(std::time::Duration::from_millis(10)),
            ..cfg(3)
        };
        let out = run_crew(
            &p,
            &HangingDispatcher,
            &mut ws,
            &cc,
            &newt_core::caveats::Caveats::top(),
            "modify target.rs to be GOOD",
        )
        .await;
        assert_eq!(out.status, CrewStatus::NeedsHumanReview, "{out:?}");
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
        let out = run_crew(
            &p,
            &d,
            &mut ws,
            &cfg(3),
            &newt_core::caveats::Caveats::top(),
            "make target.rs GOOD",
        )
        .await;
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
        let out = run_crew(
            &p,
            &d,
            &mut ws,
            &cfg(1),
            &newt_core::caveats::Caveats::top(),
            "make target.rs GOOD",
        )
        .await;
        assert_eq!(out.status, CrewStatus::NeedsHumanReview);
        assert_eq!(out.attempts, 1);
    }

    #[tokio::test]
    async fn refuses_edits_outside_the_fs_write_leash() {
        // 23.1: a read-only session (fs_write = none) means every edit is REFUSED at
        // apply — even the GOOD one — so the crew can never satisfy the check and
        // exits honestly, having written nothing. The leash holds against a crew that
        // *would* otherwise converge.
        let p = pool();
        let d = RoleMock::new();
        let mut ws = MemWs::new();
        let read_only = newt_core::caveats::Caveats {
            fs_write: newt_core::caveats::Scope::none(),
            ..newt_core::caveats::Caveats::top()
        };
        let out = run_crew(&p, &d, &mut ws, &cfg(3), &read_only, "make target.rs GOOD").await;
        assert_eq!(out.status, CrewStatus::NeedsHumanReview);
        assert!(
            out.touched.is_empty(),
            "nothing may be written outside the leash"
        );
        assert_eq!(ws.read("target.rs").as_deref(), Some("BAD"), "untouched");
    }

    /// Workspace that RECORDS every `read()` call, so a test can assert exactly
    /// which navigator-selected files the harness actually opened. `target.rs`
    /// drives the verify (passes once it contains "GOOD"), as in [`MemWs`].
    struct RecordingWs {
        files: BTreeMap<String, String>,
        reads: std::cell::RefCell<Vec<String>>,
    }
    impl Workspace for RecordingWs {
        fn files(&self) -> Vec<String> {
            self.files.keys().cloned().collect()
        }
        fn read(&self, path: &str) -> Option<String> {
            self.reads.borrow_mut().push(path.to_string());
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
                Some(c) if c.contains("GOOD") => (true, "ok".into()),
                _ => (false, "FAILED".into()),
            }
        }
    }

    /// Navigator selects one in-scope and one out-of-scope file; the planner makes
    /// `target.rs` GOOD on its first reply so the loop converges in one attempt (the
    /// curate read happens once, before the attempt loop).
    struct ReadGateMock;
    #[async_trait]
    impl Dispatcher for ReadGateMock {
        async fn dispatch(
            &self,
            _backend: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            let content = match model {
                "nav" => r#"{"relevant_files":["docs/x.rs","secret.rs"]}"#.to_string(),
                "planner" => r#"{"edits":[{"path":"target.rs","new_content":"GOOD"}]}"#.to_string(),
                "triage" => r#"{"summary":"","next_action":""}"#.to_string(),
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
    async fn refuses_to_read_outside_the_fs_read_leash() {
        // #752: complete mediation for the READ axis. fs_read = Only(["docs/x.rs"])
        // clamps reads to that one file; the navigator selects BOTH it and the
        // out-of-scope `secret.rs`. The harness must read the in-scope file and must
        // NOT read the denied one — otherwise the clamped caveat is silently ignored.
        // RED on the pre-#752 code, which read every navigator pick unconditionally.
        let p = pool();
        let mut ws = RecordingWs {
            files: BTreeMap::from([
                ("target.rs".to_string(), "BAD".to_string()),
                ("docs/x.rs".to_string(), "DOC".to_string()),
                ("secret.rs".to_string(), "TOPSECRET".to_string()),
            ]),
            reads: std::cell::RefCell::new(Vec::new()),
        };
        let read_clamped = newt_core::caveats::Caveats {
            fs_read: newt_core::caveats::Scope::only(["docs/x.rs".to_string()]),
            ..newt_core::caveats::Caveats::top()
        };
        let out = run_crew(
            &p,
            &ReadGateMock,
            &mut ws,
            &cfg(3),
            &read_clamped,
            "make target.rs GOOD",
        )
        .await;
        assert_eq!(out.status, CrewStatus::Passed, "{out:?}");
        let reads = ws.reads.borrow();
        assert!(
            reads.iter().any(|r| r == "docs/x.rs"),
            "the in-scope file must be read: {reads:?}"
        );
        assert!(
            !reads.iter().any(|r| r == "secret.rs"),
            "the out-of-scope file must NOT be read under the fs_read leash: {reads:?}"
        );
    }

    /// Planner that always emits a lazy/elided placeholder for the target file.
    struct LazyMock;
    #[async_trait]
    impl Dispatcher for LazyMock {
        async fn dispatch(
            &self,
            _backend: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            let content = match model {
                "nav" => r#"{"relevant_files":["target.rs"]}"#.to_string(),
                "triage" => {
                    r#"{"summary":"lazy","next_action":"emit the complete file"}"#.to_string()
                }
                "planner" => "FILE: target.rs\n<the full file content remains unchanged>\nEND-FILE"
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
    async fn lazy_emission_is_refused_and_never_overwrites_the_file() {
        // #688 / the #548 second failure: a `<the full file content remains
        // unchanged>` placeholder must NOT be applied (applying it would delete the
        // real file), and the run stays honest (never reported as passed).
        let p = pool();
        let mut ws = MemWs::new();
        let out = run_crew(
            &p,
            &LazyMock,
            &mut ws,
            &cfg(2),
            &newt_core::caveats::Caveats::top(),
            "make target.rs GOOD",
        )
        .await;
        assert_eq!(
            ws.read("target.rs").as_deref(),
            Some("BAD"),
            "a lazy placeholder must never overwrite the real file"
        );
        assert_eq!(out.status, CrewStatus::NeedsHumanReview);
        assert!(out.touched.is_empty(), "nothing clean was emitted to apply");
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
        let out = run_crew(
            &p,
            &d,
            &mut ws,
            &cfg(3),
            &newt_core::caveats::Caveats::top(),
            "task",
        )
        .await;
        assert_eq!(out.status, CrewStatus::NeedsHumanReview);
        assert_eq!(out.attempts, 0);
    }

    /// Planner emits NO edits on its first call (the #701 / #548 failure mode —
    /// "located the code but emitted no edit"), then the GOOD edit.
    struct ZeroEditThenGoodMock {
        planner_calls: AtomicUsize,
    }
    #[async_trait]
    impl Dispatcher for ZeroEditThenGoodMock {
        async fn dispatch(
            &self,
            _backend: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            let content = match model {
                "nav" => r#"{"relevant_files":["target.rs"]}"#.to_string(),
                "triage" => {
                    r#"{"summary":"no edits emitted","next_action":"emit the file"}"#.to_string()
                }
                "planner" => {
                    let n = self.planner_calls.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        // Prose only — located the code, emitted no edits block.
                        "I located the change in target.rs and it should be set to GOOD."
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

    #[tokio::test]
    async fn zero_edit_attempt_is_reprompted_and_recovers() {
        // #701: the first planner reply emits NO edits (the #548 retest failure).
        // The crew must RE-PROMPT for the edit, not pass vacuously on the
        // unchanged tree and land nothing.
        let p = pool();
        let d = ZeroEditThenGoodMock {
            planner_calls: AtomicUsize::new(0),
        };
        let mut ws = MemWs::new();
        let out = run_crew(
            &p,
            &d,
            &mut ws,
            &cfg(3),
            &newt_core::caveats::Caveats::top(),
            "modify target.rs to be GOOD",
        )
        .await;
        assert_eq!(out.status, CrewStatus::Passed, "{out:?}");
        assert_eq!(
            out.attempts, 2,
            "re-prompt consumes attempt 1, edit lands on 2"
        );
        assert_eq!(
            out.touched,
            vec!["target.rs".to_string()],
            "must land the edit after the re-prompt, not a no-op pass"
        );
        assert_eq!(ws.read("target.rs").as_deref(), Some("GOOD"));
    }

    /// Planner that NEVER emits an edit — proves the re-prompt is bounded.
    struct AlwaysZeroEditMock;
    #[async_trait]
    impl Dispatcher for AlwaysZeroEditMock {
        async fn dispatch(
            &self,
            _backend: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            let content = match model {
                "nav" => r#"{"relevant_files":["target.rs"]}"#.to_string(),
                "triage" => {
                    r#"{"summary":"still no edits","next_action":"emit the file"}"#.to_string()
                }
                "planner" => "Analysis only — no edits.".to_string(),
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
    async fn persistent_zero_edit_is_reprompted_once_then_exits_honestly() {
        // #701: the re-prompt is bounded — a model that NEVER emits an edit is
        // re-prompted once, then the loop proceeds and exits honestly (no infinite
        // re-prompt, no vacuous pass on the unchanged BAD tree).
        let p = pool();
        let mut ws = MemWs::new();
        let out = run_crew(
            &p,
            &AlwaysZeroEditMock,
            &mut ws,
            &cfg(3),
            &newt_core::caveats::Caveats::top(),
            "modify target.rs to be GOOD",
        )
        .await;
        assert_eq!(out.status, CrewStatus::NeedsHumanReview, "{out:?}");
        assert!(out.touched.is_empty());
    }

    #[tokio::test]
    async fn verify_only_leaf_is_not_reprompted() {
        // #701 review: a CLEARLY verify-only leaf that correctly emits no edits and
        // is already green must NOT be re-prompted (no spurious-edit goading) — it
        // passes on attempt 1 without consuming a re-prompt.
        let p = pool();
        let mut ws = MemWs::good();
        let out = run_crew(
            &p,
            &AlwaysZeroEditMock,
            &mut ws,
            &cfg(3),
            &newt_core::caveats::Caveats::top(),
            "ensure target.rs still validates",
        )
        .await;
        assert_eq!(out.status, CrewStatus::Passed, "{out:?}");
        assert_eq!(
            out.attempts, 1,
            "verify-only leaf must not consume a re-prompt"
        );
        assert!(out.touched.is_empty());
    }

    #[test]
    fn task_requires_change_skips_only_clear_verify_only() {
        assert!(task_requires_change(
            "Modify the help output in newt-tui/src/lib.rs"
        ));
        assert!(task_requires_change("Add a unit test for the rollup"));
        // Ambiguous (no change verb, no verify verb) defaults to re-prompt.
        assert!(task_requires_change("Roll up /dgx in the top-level help"));
        // Clearly verify-only -> skipped.
        assert!(!task_requires_change(
            "Ensure /dgx help still lists all subcommands"
        ));
        assert!(!task_requires_change(
            "Verify the rollup behavior via manual check"
        ));
    }
}
