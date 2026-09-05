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
use newt_core::caveats::{Caveats, CaveatsExt, CountBoundExt};
use newt_core::lazy_emission::lazy_emission_reason;
use newt_core::{Tier, TokenUsage};
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
    /// #883: the verify was already green on the PRE-EDIT baseline yet the crew
    /// LANDED edits — the check cannot prove the change (a vacuous verify, e.g. a
    /// filtered `cargo test <not-yet-existing-test>` that runs 0 tests and exits
    /// 0). Never reported as success.
    VacuousVerify,
}

/// One attempted crew-role dispatch and the backend result, if one succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleStep {
    pub role: String,
    pub tier: Tier,
    pub model: String,
    pub backend: Option<String>,
    pub failed_over: Vec<String>,
    pub model_id: Option<String>,
    pub usage: Option<TokenUsage>,
}

impl RoleStep {
    fn succeeded(
        role: &str,
        tier: Tier,
        model: &str,
        dispatch: &crate::Failover<crate::ChatReply>,
    ) -> Self {
        Self {
            role: role.to_string(),
            tier,
            model: model.to_string(),
            backend: Some(dispatch.chosen.clone()),
            failed_over: dispatch.failed.clone(),
            model_id: Some(dispatch.result.model_id.clone()),
            usage: dispatch.result.usage,
        }
    }

    fn failed(role: &str, tier: Tier, model: &str, failed_over: Vec<String>) -> Self {
        Self {
            role: role.to_string(),
            tier,
            model: model.to_string(),
            backend: None,
            failed_over,
            model_id: None,
            usage: None,
        }
    }
}

/// The result of running the crew on a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewOutcome {
    pub status: CrewStatus,
    /// Planning rounds spent (0 if the crew could not even start — no backend).
    pub attempts: u32,
    /// Paths the workspace reports as written, from the last applied plan.
    pub touched: Vec<String>,
    /// Paths whose edits were REFUSED on the last attempt — by the `fs_write`
    /// leash or the #812 leaf-scope fence — surfaced so a refusal is
    /// diagnosable in the terminal report instead of a bare "touched: (none)".
    pub refused: Vec<String>,
    /// Role dispatches in execution order, including unsuccessful dispatches.
    pub steps: Vec<RoleStep>,
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
/// Mirrors the gpu-runner+DGX crew loadout: a strong planner, a mid navigator, a small
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
    /// #883: apply verify-baseline calibration — flag a verify that is already
    /// green on the PRE-EDIT tree (yet the crew landed edits) as `VacuousVerify`
    /// instead of a false `Passed`. ON for a standalone crew leaf. OFF for
    /// team/plan sequential subtasks, whose SHARED workspace makes a green
    /// baseline expected once a prior leaf landed (per-leaf-verify-aware
    /// calibration there is a follow-up).
    pub calibrate_baseline: bool,
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

/// #812 leaf-scope fence — pure data + pure predicate.
///
/// The one-step directive tells a per-leaf worker it is executing ONE step of
/// a larger plan and to stay in its lane. It deliberately does NOT promise
/// "no edits is success": until the landing gate tolerates a verified no-op
/// leaf (#800, sequenced after this), a no-edit reply still fails the leaf
/// as nothing-to-land — the directive must not promise what the harness
/// doesn't deliver. Re-add the already-present clause with #800. Three-Cs
/// note: prompt knowledge as a `const` (working code first); promotion into
/// a droppable data pack alongside the `api_surface.rs` lexicons is the
/// flagged follow-up.
const ONE_STEP_DIRECTIVE: &str = "\n\nYou are implementing ONE STEP of a larger plan. \
Confine your edits to these files: {scope}. \
Do NOT implement other steps of the plan.";

fn one_step_directive(scope: &[String]) -> String {
    ONE_STEP_DIRECTIVE.replace("{scope}", &scope.join(", "))
}

/// Does the leaf scope permit editing `path`? PURE and meet-only: an empty
/// scope permits everything (no fence — byte-identical to pre-#812); a
/// non-empty scope permits an exact file match or anything under a listed
/// directory. The scope is a CONVENIENCE attenuation above the OCAP
/// boundary — it is intersected with (never unioned into) the `fs_write`
/// leash at apply, so it can only narrow the effective writable set.
///
/// Degenerate entries FAIL OPEN: an empty/whitespace entry, `"."`, or `"./"`
/// names the whole tree, so it permits everything rather than (absurdly)
/// denying every relative path. The fence is a convenience — bad fence DATA
/// must never be able to brick a leaf.
fn scope_permits(scope: &[String], path: &str) -> bool {
    if scope.is_empty() {
        return true;
    }
    let norm = path.strip_prefix("./").unwrap_or(path);
    scope.iter().any(|entry| {
        let e = entry.trim();
        let e = e.strip_prefix("./").unwrap_or(e);
        let dir = e.strip_suffix('/').unwrap_or(e);
        if dir.is_empty() || dir == "." {
            return true; // degenerate entry = the whole tree = no fence
        }
        norm == e || norm == dir || norm.starts_with(&format!("{dir}/"))
    })
}

/// The PLAN-step system prompt: turns a leaf's `task` text into whole-file
/// edits. Pinned as a `const` (not inlined) so the extraction convention below
/// is unit-testable without a live model.
///
/// The trailing sentence is the fix for the autopsy's "other" bucket
/// (`2026-07-02-pr802-baseline.json`, task `010-decompose-god-function`): a
/// worker asked to extract a helper "at the definition site of `summarize`"
/// nested the new `fn` *inside* `summarize`'s body instead of beside it
/// (`git show b670783`), or emitted it `pub`, or added a `panic!`/bare
/// `.unwrap()` the hidden grade_spec's structural fence rejects
/// (`helpers_are_private_with_exact_signatures` /
/// `summarize_delegates_to_all_three_helpers` /
/// `helper_bodies_are_straightline` in `tests/grade_spec.rs`). The prior
/// prompt said nothing about *where* or *how* a new function should be
/// defined, so an ambiguous instruction resolved to the wrong shape more
/// often than not. This is a convention, not a grader change: it does not
/// touch `verify`, cannot make a leaf self-pass, and a violating body still
/// fails the same real gate it did before — it only shapes what the worker
/// is more likely to emit on the attempt that lands.
const CREW_PLAN_SYSTEM: &str =
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
     file unchanged …'. If a file is unchanged, omit its block. \
     When a task asks you to extract, add, or define a new helper or \
     function, define it as a top-level sibling function next to the \
     caller — never nested inside another function's body. Give it \
     exactly the requested name and signature, and keep it private \
     unless the task explicitly says `pub`. Do not introduce a new \
     panic, unwrap, or other failure path the task did not ask for.";

/// Run the crew's two-pass control loop on `task`, returning the outcome.
///
/// `None` from [`run_role`](BackendPool::run_role) (nothing live serves a pinned
/// model) is a hard stop, not a silent skip: navigator-unavailable ⇒
/// `NeedsHumanReview` with `attempts: 0`; planner-unavailable mid-loop ⇒
/// `NeedsHumanReview` at the current attempt. Triage-unavailable is non-fatal — the
/// next round simply plans without a fresh diagnosis.
///
/// `scope` (#812) fences the worker to its leaf's declared files: the
/// navigator is seeded to prefer them, the planner prompt carries the
/// one-step directive, and out-of-scope edits are refused into the same
/// feedback channel as leash refusals. Empty scope ⇒ no fence.
pub async fn run_crew(
    pool: &BackendPool,
    dispatcher: &dyn Dispatcher,
    workspace: &mut dyn Workspace,
    cfg: &CrewConfig,
    caveats: &Caveats,
    task: &str,
    scope: &[String],
) -> CrewOutcome {
    // --- max_calls budget (#753): complete mediation for the call-count axis ---
    //
    // The crew's unit of "a call" is a MODEL/ROLE dispatch — every navigate,
    // plan, and triage round issues exactly one `run_role_with_timeout`. That is
    // the same "inference call" unit `newt-coder` already bounds (`coder.rs`'s
    // `calls_used` / `check_call_budget`), and it is the resource `max_calls` is
    // meant to cap (model invocations / cost), so we count and gate THAT — not the
    // planning-round count. `cfg.max_attempts` still bounds the planning rounds;
    // this is an INDEPENDENT ceiling so a clamped `max_calls` caveat actually has
    // teeth. Before EACH dispatch we ask `max_calls.permits_one_more(calls_used)`,
    // and when the budget denies we stop with an honest `NeedsHumanReview`
    // cap-exit (never reported as success). `CountBound::Unlimited` (the default /
    // `Caveats::top`) permits every call, so an unclamped crew is unchanged.
    //
    // NET AXIS — deliberately NOT gated in this loop. The crew has no DIRECT
    // network effect that a `caveats.permits_net(host)` check could mediate:
    //   (a) model inference goes to the backend INFRASTRUCTURE endpoint (the
    //       agent's own substrate, not a task-chosen host), and
    //   (b) any network a verification command performs happens INSIDE
    //       `workspace.run_test()` — that is the EXEC axis's concern (which
    //       command may run) and requires an OS sandbox for true containment,
    //       not a predicate at this layer.
    // So `permits_net` is correctly NOT a crew-loop call-site; net at the crew
    // layer is governed transitively via the exec axis + the OS sandbox, by
    // design (complete mediation per axis: this axis needs a sandbox, not a
    // crew-loop predicate).
    let mut calls_used: u64 = 0;
    let mut steps = Vec::new();

    // 1. NAVIGATE — pick the relevant files (then the harness reads them).
    //    #812: a scoped leaf seeds the navigator with its declared files — a
    //    soft preference (the navigator may still surface context files; only
    //    APPLY is fenced).
    let scope_seed = if scope.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nPLAN SCOPE: this step is expected to need only these files — \
             prefer them: {}",
            scope.join(", ")
        )
    };
    let nav_req = ChatRequest::new()
        .system(
            "You are a repository navigator. Reply with ONLY JSON \
             {\"relevant_files\":[\"path\", ...]} listing the files needed to do the task.",
        )
        .user(format!(
            "TASK:\n{task}\n\nAVAILABLE FILES:\n{:?}{scope_seed}",
            workspace.files()
        ));
    // #698: per-role dispatch bound — the crew config's `role_timeout` if set,
    // else the env/default (`NEWT_ROLE_TIMEOUT_SECS` → 600s).
    let role_bound = cfg
        .role_timeout
        .unwrap_or_else(crate::dispatch::role_dispatch_timeout);
    // max_calls (#753): a zero budget can't even afford the navigator — honest
    // cap-exit before any model is touched.
    if !caveats.max_calls.permits_one_more(calls_used) {
        return CrewOutcome {
            status: CrewStatus::NeedsHumanReview,
            attempts: 0,
            touched: Vec::new(),
            refused: Vec::new(),
            steps,
        };
    }
    calls_used += 1;
    let nav_candidates = pool
        .ranked_candidates(Tier::Standard, Some(&cfg.navigator_model))
        .into_iter()
        .map(|backend| backend.name.clone())
        .collect();
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
        Some(f) => {
            steps.push(RoleStep::succeeded(
                "navigator",
                Tier::Standard,
                &cfg.navigator_model,
                &f,
            ));
            parse(&f.result.content)
        }
        None => {
            steps.push(RoleStep::failed(
                "navigator",
                Tier::Standard,
                &cfg.navigator_model,
                nav_candidates,
            ));
            return CrewOutcome {
                status: CrewStatus::NeedsHumanReview,
                attempts: 0,
                touched: Vec::new(),
                refused: Vec::new(),
                steps,
            };
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

    // #883 verify calibration: capture the verify verdict on the PRE-EDIT tree
    // once, before any apply() (apply only runs inside the loop). A verify that
    // is already green here cannot discriminate the crew's edits — a later green
    // with edits landed is vacuous. Gated: OFF for team/plan sequential subtasks
    // whose shared workspace is expectedly green once a prior leaf landed.
    let baseline_ok = cfg.calibrate_baseline && workspace.run_test().0;

    let mut failures: Vec<String> = Vec::new();
    let mut touched: Vec<String> = Vec::new();
    let mut last_refused: Vec<String> = Vec::new();
    let mut reprompted_zero_edit = false;

    for attempt in 1..=cfg.max_attempts {
        // 3. PLAN — emit full-file edits, told about the prior failure if any.
        let prior = match failures.last() {
            Some(f) => format!("\n\nThe previous attempt FAILED verification:\n{f}\nFix it."),
            None => String::new(),
        };
        let plan_req = ChatRequest::new().system(CREW_PLAN_SYSTEM).user(format!(
            "TASK:\n{task}{directive}\n\nRELEVANT FILES:\n{curated}{prior}",
            directive = if scope.is_empty() {
                String::new()
            } else {
                one_step_directive(scope)
            },
        ));
        // max_calls (#753): gate the planner dispatch. If the budget is spent the
        // crew stops here — `attempt - 1` planning rounds completed before this one
        // (this round never started), an honest cap-exit, not a vacuous pass.
        if !caveats.max_calls.permits_one_more(calls_used) {
            return CrewOutcome {
                status: CrewStatus::NeedsHumanReview,
                attempts: attempt - 1,
                touched,
                refused: last_refused,
                steps,
            };
        }
        calls_used += 1;
        let planner_candidates = pool
            .ranked_candidates(Tier::Complex, Some(&cfg.planner_model))
            .into_iter()
            .map(|backend| backend.name.clone())
            .collect();
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
            Some(f) => {
                steps.push(RoleStep::succeeded(
                    "planner",
                    Tier::Complex,
                    &cfg.planner_model,
                    &f,
                ));
                parse_edits(&f.result.content)
            }
            None => {
                steps.push(RoleStep::failed(
                    "planner",
                    Tier::Complex,
                    &cfg.planner_model,
                    planner_candidates,
                ));
                return CrewOutcome {
                    status: CrewStatus::NeedsHumanReview,
                    attempts: attempt,
                    touched,
                    refused: last_refused,
                    steps,
                };
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
        // #812: the leaf-scope fence — a SECOND, meet-only partition. Effective
        // writable set = worktree ∩ fs_write ∩ scope; the fence can only narrow.
        // Out-of-scope edits ride the same refusal/feedback channel as leash
        // refusals (with their own message) so the next round re-aims instead
        // of silently landing over-reach. Empty scope ⇒ this is a no-op.
        let (allowed, scope_refused): (Vec<Edit>, Vec<Edit>) = allowed
            .into_iter()
            .partition(|e| scope_permits(scope, &e.path));
        last_refused = refused
            .iter()
            .chain(scope_refused.iter())
            .map(|e| e.path.clone())
            .collect();
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
            && scope_refused.is_empty()
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
        let mut refusal_notes: Vec<String> = Vec::new();
        if !refused.is_empty() {
            let names: Vec<&str> = refused.iter().map(|e| e.path.as_str()).collect();
            refusal_notes.push(format!(
                "REFUSED (outside the fs_write leash — attenuate the task or widen the grant): {}",
                names.join(", ")
            ));
        }
        if !scope_refused.is_empty() {
            let names: Vec<&str> = scope_refused.iter().map(|e| e.path.as_str()).collect();
            refusal_notes.push(format!(
                "REFUSED (outside this leaf's scope — implement ONLY this step; \
                 the scoped files are: {}): {}",
                scope.join(", "),
                names.join(", ")
            ));
        }
        let (ok, output) = workspace.run_test();
        if ok {
            // #883 verify calibration: a check that was ALREADY green on the
            // pre-edit baseline cannot prove the crew's edits. Baseline-green +
            // edits-landed = Vacuous — never a success. A keep-green verify-only
            // leaf (baseline green, ZERO edits) classifies KeepGreen and falls
            // through to the honest pass below (preserving #701).
            if classify_verify(baseline_ok, true, !touched.is_empty()) == VerifyCalibration::Vacuous
            {
                return CrewOutcome {
                    status: CrewStatus::VacuousVerify,
                    attempts: attempt,
                    touched,
                    refused: last_refused,
                    steps,
                };
            }
            // #812 adversarial-review finding: a green check on the UNCHANGED
            // tree while edits were REFUSED is NOT success — nothing landed
            // and the model aimed outside its lane/leash. Accepting it would
            // return a vacuous first-attempt Passed that dies downstream as
            // "nothing to land" with ZERO re-aim rounds (the fallback repo
            // verify is green at HEAD in the common one-shot case). Feed the
            // refusal back and retry instead — attempts exhausting ends in an
            // honest NeedsHumanReview, the design doc's predicted arm.
            if touched.is_empty() && !refusal_notes.is_empty() {
                failures.push(refusal_notes.join("\n"));
                continue;
            }
            return CrewOutcome {
                status: CrewStatus::Passed,
                attempts: attempt,
                touched,
                refused: last_refused,
                steps,
            };
        }
        let output = if refusal_notes.is_empty() {
            output
        } else {
            format!("{}\n{output}", refusal_notes.join("\n"))
        };

        // 6. TRIAGE — diagnose the failure; fed into the next planning round.
        let tri_req = ChatRequest::new()
            .system(
                "You are a build cop. Reply with ONLY JSON \
                 {\"summary\":\"what failed\",\"next_action\":\"what to change\"}.",
            )
            .user(format!("TASK:\n{task}\n\nVERIFICATION OUTPUT:\n{output}"));
        // max_calls (#753): gate the triage dispatch. With the budget spent the
        // next planning round could not dispatch either, so stop now — this round's
        // plan/apply/verify already completed, so `attempt` rounds were spent.
        if !caveats.max_calls.permits_one_more(calls_used) {
            return CrewOutcome {
                status: CrewStatus::NeedsHumanReview,
                attempts: attempt,
                touched,
                refused: last_refused,
                steps,
            };
        }
        calls_used += 1;
        let triage_candidates = pool
            .ranked_candidates(Tier::Fast, Some(&cfg.triage_model))
            .into_iter()
            .map(|backend| backend.name.clone())
            .collect();
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
            Some(f) => {
                steps.push(RoleStep::succeeded(
                    "triage",
                    Tier::Fast,
                    &cfg.triage_model,
                    &f,
                ));
                parse(&f.result.content)
            }
            None => {
                steps.push(RoleStep::failed(
                    "triage",
                    Tier::Fast,
                    &cfg.triage_model,
                    triage_candidates,
                ));
                TriageOut::default()
            }
        };
        failures.push(format!("{} -> {}", tri.summary, tri.next_action));
    }

    // 7. CAP-EXIT — honest: attempts exhausted, never reported as success.
    CrewOutcome {
        status: CrewStatus::NeedsHumanReview,
        attempts: cfg.max_attempts,
        touched,
        refused: last_refused,
        steps,
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
    fn crew_plan_system_states_the_extraction_convention() {
        // Regression for the autopsy's "other" bucket
        // (2026-07-02-pr802-baseline.json, task 010-decompose-god-function):
        // before this fix CREW_PLAN_SYSTEM said nothing about *where* a new
        // helper goes, and devstral-small-2:24b nested the extracted `fn`
        // inside `summarize`'s body 4/4 runs (`git show b670783`) while other
        // models emitted it `pub` or with a bare `panic!`/`.unwrap()` — all
        // rejected by tests/grade_spec.rs's structural fence. Pin the prose so
        // a future edit can't silently drop the convention.
        assert!(CREW_PLAN_SYSTEM.contains("top-level sibling function"));
        assert!(CREW_PLAN_SYSTEM.contains("never nested inside another function's body"));
        assert!(CREW_PLAN_SYSTEM.contains("keep it private unless the task explicitly says"));
        assert!(CREW_PLAN_SYSTEM.contains("Do not introduce a new panic, unwrap"));
        // The bake-off-pinned FILE:/END-FILE directive must survive untouched —
        // this is an APPEND, not a rewrite of the emission-format contract.
        assert!(CREW_PLAN_SYSTEM.contains("FILE: <relative/path>"));
        assert!(CREW_PLAN_SYSTEM.contains("END-FILE"));
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
            // Single-crew tests exercise the #883 calibration by default.
            calibrate_baseline: true,
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
            &[],
        )
        .await;
        assert_eq!(out.status, CrewStatus::NeedsHumanReview, "{out:?}");
        assert_eq!(out.steps.len(), 1, "{out:?}");
        assert_eq!(out.steps[0].role, "navigator");
        assert_eq!(out.steps[0].failed_over, ["dgx"]);
        assert!(out.steps[0].backend.is_none());
    }

    /// A dispatcher whose planner ALWAYS emits a (non-empty) edit writing
    /// `target.rs=GOOD` — a landed edit even though the baseline is already green.
    struct AlwaysEditGoodMock;
    #[async_trait]
    impl Dispatcher for AlwaysEditGoodMock {
        async fn dispatch(
            &self,
            backend: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            if backend.name == "backend-a" {
                anyhow::bail!("backend-a dispatch failed");
            }
            let content = match model {
                "nav" => r#"{"relevant_files":["target.rs"]}"#.to_string(),
                "triage" => r#"{"summary":"ok","next_action":"none"}"#.to_string(),
                "planner" => r#"{"edits":[{"path":"target.rs","new_content":"GOOD"}]}"#.to_string(),
                other => panic!("unexpected model {other}"),
            };
            Ok(ChatReply {
                content,
                model_id: model.to_string(),
                usage: None,
            })
        }
    }

    /// A crew run records which backend served every role, including the planner's
    /// failed first dispatch. Before the ledger existed this routing fact was
    /// discarded when `Failover` was reduced to `result.content`.
    #[tokio::test]
    async fn crew_outcome_records_role_steps_and_planner_failover() {
        let p = BackendPool::from_source(&StaticSource {
            backends: vec![
                PoolBackend::new("backend-a", "http://backend-a:11434", BackendKind::Ollama)
                    .with_tiers(vec![Tier::Complex])
                    .with_models(["planner"])
                    .with_health(Health::Up),
                PoolBackend::new("backend-b", "http://backend-b:11434", BackendKind::Ollama)
                    .with_models(["nav", "planner", "triage"])
                    .with_health(Health::Up),
            ],
        });
        let mut ws = MemWs::new();
        let out = run_crew(
            &p,
            &AlwaysEditGoodMock,
            &mut ws,
            &cfg(1),
            &newt_core::caveats::Caveats::top(),
            "make target.rs GOOD",
            &["other.rs".to_string()],
        )
        .await;

        assert_eq!(
            out.steps
                .iter()
                .map(|step| step.role.as_str())
                .collect::<Vec<_>>(),
            ["navigator", "planner", "triage"]
        );
        assert_eq!(out.steps[1].failed_over, ["backend-a"]);
        assert_eq!(out.steps[1].backend.as_deref(), Some("backend-b"));
    }

    #[tokio::test]
    async fn vacuous_verify_flags_green_baseline_change() {
        // #883 regression: the verify is GREEN on the pre-edit baseline
        // (`MemWs::good` → target.rs=GOOD) yet the crew LANDS an edit. The check
        // cannot prove the change, so the crew must report VacuousVerify — NOT a
        // false Passed. This test FAILS on pre-#883 code (which returns Passed).
        let p = pool();
        let mut ws = MemWs::good();
        let out = run_crew(
            &p,
            &AlwaysEditGoodMock,
            &mut ws,
            &cfg(3),
            &newt_core::caveats::Caveats::top(),
            "make target.rs GOOD",
            &[],
        )
        .await;
        assert_eq!(out.status, CrewStatus::VacuousVerify, "{out:?}");
        assert!(!out.touched.is_empty(), "the crew landed an edit: {out:?}");
        assert_eq!(out.attempts, 1);
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
            &[],
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
            &[],
        )
        .await;
        assert_eq!(out.status, CrewStatus::NeedsHumanReview);
        assert_eq!(out.attempts, 1);
    }

    #[test]
    fn scope_permits_is_meet_only_and_prefix_aware() {
        let scope = vec!["src/util.rs".to_string(), "tests/".to_string()];
        // empty scope = no fence
        assert!(scope_permits(&[], "anything/at/all.rs"));
        // exact file match, with ./ normalization on either side
        assert!(scope_permits(&scope, "src/util.rs"));
        assert!(scope_permits(&scope, "./src/util.rs"));
        assert!(scope_permits(&["./src/util.rs".to_string()], "src/util.rs"));
        // directory entries permit anything beneath them
        assert!(scope_permits(&scope, "tests/grade_spec.rs"));
        assert!(scope_permits(&["src".to_string()], "src/lib.rs"));
        // out-of-scope stays out — including sneaky prefixes of a scoped name
        assert!(!scope_permits(&scope, "src/lib.rs"));
        assert!(!scope_permits(&scope, "src/util.rs.bak"));
        assert!(!scope_permits(&scope, "Cargo.toml"));
    }

    /// Planner that always emits edits to BOTH `target.rs` (the fix) and
    /// `README.md` (over-reach) — the #812 fence must let the fix land and
    /// refuse the over-reach, in the same attempt.
    struct OverreachMock;
    #[async_trait]
    impl Dispatcher for OverreachMock {
        async fn dispatch(
            &self,
            _backend: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            let content = match model {
                "nav" => r#"{"relevant_files":["target.rs","README.md"]}"#.to_string(),
                "triage" => r#"{"summary":"s","next_action":"n"}"#.to_string(),
                "planner" => {
                    "FILE: target.rs\nGOOD\nEND-FILE\nFILE: README.md\nhacked\nEND-FILE".to_string()
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
    async fn scope_fence_refuses_over_reach_but_lands_the_in_scope_fix() {
        // #812: scope = the leaf's lane. The in-scope edit lands (verify goes
        // green), the out-of-scope edit is REFUSED — the accidental-whole-fix /
        // orphan-file mechanism cannot land work outside the lane.
        let p = pool();
        let d = OverreachMock;
        let mut ws = MemWs::new();
        let scope = vec!["target.rs".to_string()];
        let out = run_crew(
            &p,
            &d,
            &mut ws,
            &cfg(3),
            &newt_core::caveats::Caveats::top(),
            "make target.rs GOOD",
            &scope,
        )
        .await;
        assert_eq!(out.status, CrewStatus::Passed);
        assert_eq!(out.touched, vec!["target.rs".to_string()]);
        assert_eq!(
            ws.read("README.md").as_deref(),
            Some("docs"),
            "out-of-scope edit must be refused, not landed"
        );
    }

    #[tokio::test]
    async fn empty_scope_lands_the_same_edits_as_before() {
        // #812 control: with NO scope the same over-reaching planner lands
        // both edits — proving the fence (not the mock) makes the difference
        // and that unscoped dispatch stays byte-identical to pre-#812.
        let p = pool();
        let d = OverreachMock;
        let mut ws = MemWs::new();
        let out = run_crew(
            &p,
            &d,
            &mut ws,
            &cfg(3),
            &newt_core::caveats::Caveats::top(),
            "make target.rs GOOD",
            &[],
        )
        .await;
        assert_eq!(out.status, CrewStatus::Passed);
        assert_eq!(
            ws.read("README.md").as_deref(),
            Some("hacked"),
            "no scope → no fence → the over-reach lands (today's behavior)"
        );
    }

    /// Planner that ONLY ever emits an out-of-scope edit — models a worker
    /// aimed at the wrong lane (a wrongly-derived scope, the doc's
    /// medium-risk vector).
    struct OffTargetMock;
    #[async_trait]
    impl Dispatcher for OffTargetMock {
        async fn dispatch(
            &self,
            _backend: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            let content = match model {
                "nav" => r#"{"relevant_files":["README.md"]}"#.to_string(),
                "triage" => r#"{"summary":"s","next_action":"n"}"#.to_string(),
                "planner" => "FILE: README.md\nhacked\nEND-FILE".to_string(),
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
    async fn refused_only_green_baseline_is_not_a_vacuous_pass() {
        // #812 adversarial-review regression: green tree (verify passes on
        // the UNCHANGED workspace) + every edit scope-refused. Accepting the
        // green check as Passed would return a first-attempt vacuous
        // Passed-with-nothing that dies downstream as "nothing to land" with
        // ZERO re-aim rounds. The loop must instead feed the refusal back,
        // retry, and end in an honest NeedsHumanReview with the refusal
        // SURFACED in the outcome.
        let p = pool();
        let d = OffTargetMock;
        let mut ws = MemWs::good(); // target.rs already GOOD → run_test green
        let scope = vec!["target.rs".to_string()];
        let out = run_crew(
            &p,
            &d,
            &mut ws,
            &cfg(3),
            &newt_core::caveats::Caveats::top(),
            "make target.rs GOOD",
            &scope,
        )
        .await;
        assert_eq!(
            out.status,
            CrewStatus::NeedsHumanReview,
            "refused-only + green baseline must NOT be a vacuous pass"
        );
        assert_eq!(out.attempts, 3, "all re-aim rounds were offered");
        assert!(out.touched.is_empty());
        assert_eq!(
            out.refused,
            vec!["README.md".to_string()],
            "the refusal is diagnosable from the outcome"
        );
        assert_eq!(ws.read("README.md").as_deref(), Some("docs"), "untouched");
    }

    /// Dispatcher that RECORDS every prompt (model, full user content) while
    /// delegating behavior to canned replies — lets a test pin the #812
    /// prompt mechanisms (nav seed + one-step directive), which are otherwise
    /// mutation-invisible.
    struct RecordingDispatcher {
        prompts: std::sync::Mutex<Vec<(String, String)>>,
    }
    #[async_trait]
    impl Dispatcher for RecordingDispatcher {
        async fn dispatch(
            &self,
            _backend: &PoolBackend,
            model: &str,
            req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            let user = req
                .messages
                .iter()
                .filter(|m| m.role == "user")
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
                .join("\n");
            self.prompts.lock().unwrap().push((model.to_string(), user));
            let content = match model {
                "nav" => r#"{"relevant_files":["target.rs"]}"#.to_string(),
                "triage" => r#"{"summary":"s","next_action":"n"}"#.to_string(),
                "planner" => "FILE: target.rs\nGOOD\nEND-FILE".to_string(),
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
    async fn scoped_dispatch_carries_nav_seed_and_one_step_directive() {
        // #812: the two PROMPT mechanisms are load-bearing (they aim the
        // worker before the hard fence ever bites) — pin their presence when
        // scoped and absence when not, so deleting them cannot pass silently.
        let p = pool();
        let d = RecordingDispatcher {
            prompts: std::sync::Mutex::new(Vec::new()),
        };
        let mut ws = MemWs::new();
        let scope = vec!["target.rs".to_string()];
        run_crew(
            &p,
            &d,
            &mut ws,
            &cfg(1),
            &newt_core::caveats::Caveats::top(),
            "make target.rs GOOD",
            &scope,
        )
        .await;
        {
            let prompts = d.prompts.lock().unwrap();
            let nav = &prompts.iter().find(|(m, _)| m == "nav").unwrap().1;
            assert!(nav.contains("PLAN SCOPE"), "navigator seeded: {nav}");
            assert!(nav.contains("target.rs"));
            let plan = &prompts.iter().find(|(m, _)| m == "planner").unwrap().1;
            assert!(
                plan.contains("ONE STEP of a larger plan"),
                "one-step directive present: {plan}"
            );
            assert!(plan.contains("target.rs"));
            prompts.iter().for_each(|(_, p)| {
                assert!(
                    !p.contains("emit NO edits"),
                    "the no-edits-is-success promise is #800's to make, not ours yet"
                );
            });
        }
        // Unscoped control: neither mechanism appears.
        let d2 = RecordingDispatcher {
            prompts: std::sync::Mutex::new(Vec::new()),
        };
        let mut ws2 = MemWs::new();
        run_crew(
            &p,
            &d2,
            &mut ws2,
            &cfg(1),
            &newt_core::caveats::Caveats::top(),
            "make target.rs GOOD",
            &[],
        )
        .await;
        let prompts = d2.prompts.lock().unwrap();
        for (_, prompt) in prompts.iter() {
            assert!(!prompt.contains("PLAN SCOPE"));
            assert!(!prompt.contains("ONE STEP of a larger plan"));
        }
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
        let out = run_crew(
            &p,
            &d,
            &mut ws,
            &cfg(3),
            &read_only,
            "make target.rs GOOD",
            &[],
        )
        .await;
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
            &[],
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
            &[],
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
            &[],
        )
        .await;
        assert_eq!(out.status, CrewStatus::NeedsHumanReview);
        assert_eq!(out.attempts, 0);
        assert_eq!(out.steps.len(), 1, "{out:?}");
        assert_eq!(out.steps[0].role, "navigator");
        assert!(out.steps[0].failed_over.is_empty());
        assert!(out.steps[0].backend.is_none());
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
            &[],
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
            &[],
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
            &[],
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

    /// Counts EVERY model dispatch (across all roles) so a test can assert the
    /// crew honored a `max_calls` budget. The planner always emits a BAD edit, so
    /// the crew never converges — absent the budget it would burn every attempt.
    struct CountingMock {
        dispatches: AtomicUsize,
    }
    #[async_trait]
    impl Dispatcher for CountingMock {
        async fn dispatch(
            &self,
            _backend: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            let content = match model {
                "nav" => r#"{"relevant_files":["target.rs"]}"#.to_string(),
                "triage" => {
                    r#"{"summary":"still BAD","next_action":"set content to GOOD"}"#.to_string()
                }
                // Never converges: always re-emits BAD, so verify always fails.
                "planner" => r#"{"edits":[{"path":"target.rs","new_content":"BAD"}]}"#.to_string(),
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
    async fn max_calls_caveat_bounds_total_model_calls() {
        // #753: a clamped `max_calls` caveat must bound the crew's model calls even
        // when `cfg.max_attempts` is far larger. With AtMost(3) and max_attempts=10,
        // the crew makes navigate + plan + triage = 3 dispatches and then stops at
        // the next planner gate (the budget is spent). RED on the pre-#753 code,
        // which bounds the loop ONLY by `cfg.max_attempts` and so burns navigate +
        // 10*(plan+triage) = 21 dispatches before exiting.
        let p = pool();
        let d = CountingMock {
            dispatches: AtomicUsize::new(0),
        };
        let mut ws = MemWs::new();
        let budget = newt_core::caveats::Caveats {
            max_calls: newt_core::caveats::CountBound::AtMost(3),
            ..newt_core::caveats::Caveats::top()
        };
        let out = run_crew(
            &p,
            &d,
            &mut ws,
            &cfg(10),
            &budget,
            "modify target.rs to be GOOD",
            &[],
        )
        .await;
        let calls = d.dispatches.load(Ordering::SeqCst);
        assert!(
            calls <= 3,
            "max_calls=AtMost(3) must bound the crew's model calls regardless of \
             cfg.max_attempts=10, but it made {calls} dispatches"
        );
        // Budget-exhausted is an honest cap-exit — never reported as success.
        assert_eq!(out.status, CrewStatus::NeedsHumanReview, "{out:?}");
    }

    #[tokio::test]
    async fn max_calls_zero_denies_even_the_navigator() {
        // #753 edge: AtMost(0) can't afford a single model call, so the crew exits
        // honestly having dispatched NOTHING (attempts:0).
        let p = pool();
        let d = CountingMock {
            dispatches: AtomicUsize::new(0),
        };
        let mut ws = MemWs::new();
        let budget = newt_core::caveats::Caveats {
            max_calls: newt_core::caveats::CountBound::AtMost(0),
            ..newt_core::caveats::Caveats::top()
        };
        let out = run_crew(&p, &d, &mut ws, &cfg(5), &budget, "modify target.rs", &[]).await;
        assert_eq!(
            d.dispatches.load(Ordering::SeqCst),
            0,
            "no call may be made"
        );
        assert_eq!(out.status, CrewStatus::NeedsHumanReview);
        assert_eq!(out.attempts, 0);
    }
}

/// #883 verify calibration: classify a verify verdict against its PRE-EDIT baseline.
/// A verify that already passes on the unedited tree cannot prove the crew's edits —
/// a later green is VACUOUS. A real pass = baseline failed and the edited tree passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyCalibration {
    /// Baseline FAILED, edited PASSED — a genuine, discriminating pass.
    Discriminating,
    /// Baseline PASSED and the crew made NO edits — a legitimate verify-only keep-green leaf.
    KeepGreen,
    /// Baseline PASSED yet the crew LANDED edits — the check cannot prove the change.
    Vacuous,
    /// The edited tree does not pass yet — retry / triage.
    StillFailing,
}

/// Pure classifier: no I/O. `made_edits` is whether the crew wrote any file.
pub fn classify_verify(baseline_ok: bool, edited_ok: bool, made_edits: bool) -> VerifyCalibration {
    match (baseline_ok, edited_ok) {
        (_, false) => VerifyCalibration::StillFailing,
        (false, true) => VerifyCalibration::Discriminating,
        (true, true) if made_edits => VerifyCalibration::Vacuous,
        (true, true) => VerifyCalibration::KeepGreen,
    }
}

#[cfg(test)]
mod calibration_tests {
    use super::{classify_verify, VerifyCalibration};

    #[test]
    fn classify_verify_real_fix_is_discriminating() {
        assert_eq!(
            classify_verify(false, true, true),
            VerifyCalibration::Discriminating
        );
    }

    #[test]
    fn classify_verify_green_baseline_with_edits_is_vacuous() {
        // The #883 core case: verify was already green on the unedited tree, yet the
        // crew landed edits -> the check cannot prove the change. Also covers a
        // filtered `cargo test <missing>` that runs 0 tests and exits 0 (baseline_ok=true).
        assert_eq!(
            classify_verify(true, true, true),
            VerifyCalibration::Vacuous
        );
    }

    #[test]
    fn classify_verify_green_baseline_no_edits_is_keep_green() {
        assert_eq!(
            classify_verify(true, true, false),
            VerifyCalibration::KeepGreen
        );
    }

    #[test]
    fn classify_verify_not_passing_yet_is_still_failing() {
        assert_eq!(
            classify_verify(false, false, true),
            VerifyCalibration::StillFailing
        );
        assert_eq!(
            classify_verify(true, false, true),
            VerifyCalibration::StillFailing
        );
        assert_eq!(
            classify_verify(false, false, false),
            VerifyCalibration::StillFailing
        );
    }
}
