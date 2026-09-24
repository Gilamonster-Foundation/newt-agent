//! `newt headless` — the non-interactive entry that drives the agentic loop
//! to complete one task and emit a trace. Originally built for Terminal-Bench
//! (epic #1419 / the release-champion ceremony, WS1), but not specific to
//! it — the timer/`fire` cron path and any script driving one headless turn
//! use this same entry. Terminal-Bench-specific behavior belongs behind its
//! own flag, never baked into this command's name or default behavior.
//!
//! It is a THIN wrapper over the same [`TurnDriver`] / `chat_complete` loop the
//! interactive TUI runs — no second loop. Headless contract:
//! `permission_gate: None` (a capability denial fails the call, never hangs).
//! P3: the default lane is now `Confined` (OCAP on, workspace-fenced); the
//! unconfined [`Caveats::top`] lane requires the explicit `--unsafe-host-exec`.
//!
//! Two headless lanes, selected up front. **P3 (`noninteractive-launch-policy`):
//! OCAP-off is an EXPLICIT opt-in — `--non-interactive` no longer selects it.**
//!
//! - **`--unsafe-host-exec` / `NEWT_UNSAFE_HOST_EXEC` (the `--yolo`/full-access
//!   lane):** sets `NEWT_FULL_ACCESS=1` + `NEWT_DISABLE_OCAP=1` so the host shell
//!   runs and no prompt can appear — the bootstrap lane that isolated the agentic
//!   variable from the confinement variable while the bench floor was
//!   established. Requires the explicit flag; `--confined` still wins.
//! - **`--confined` / `NEWT_BENCH_OCAP=on` — the OCAP-on lane AND the default:**
//!   OCAP stays ON.
//!   Instead of full access, [`confined_bench_caveats`] seeds a workspace-fenced
//!   authority — reads/exec/net stay open, but writes are confined to the
//!   workspace, the configured scratch root (the platform temp dir unless
//!   `NEWT_SCRATCH_DIR` / `[scratch] dir` names an absolute one) and explicit
//!   `NEWT_WRITE_PATHS` grants (a `Scope::Only`
//!   fs_write, never `Scope::All`). The container's mutable system roots
//!   (`/usr /usr/local /var /etc /opt /root /home`, the #1487 bench rationale:
//!   package installs) are OPT-IN: this lane is the default, and a default must
//!   be safe on a developer host, where `/home` held `~/.rustup`, `~/.ssh` and
//!   every other checkout. Bench containers pass them via `NEWT_WRITE_PATHS`.
//!   A `Scope::Only` write auto-consents at the
//!   tool gate (the preset IS the operator's consent — see
//!   `tools::confirm_unrestricted_fs_mutation`), so in-fence writes run with no
//!   prompt. This is the lane the 0.7.6 OCAP-parity gate measures against the
//!   `--yolo` scores.
//!
//! **Scope of the write fence (be honest about it).** The fence enforces newt's
//! own `write_file`/`edit_file` tool gate (`tools::tui_permits_path`), where an
//! out-of-fence write is denied. It does NOT confine writes performed by
//! programs the agent *spawns* (`exec` is `Scope::All`): confining those needs
//! the kernel L3 fence (Landlock), and this lane forces the brush engine even
//! when Landlock is absent, so a spawned command's writes are then advisory, not
//! kernel-enforced. Combined with `fs_read = All` + `net = All`, this lane is a
//! **bench isolation control for disposable containers, not a security sandbox**
//! against a hostile agent. The fence started broad on its first cut (#1487); the
//! standard mutable roots have since moved behind an explicit grant.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use newt_core::caveats::{Caveats, CountBound, Scope};
use newt_core::model_card::ChatCompletionsCapability;
use newt_core::role_profile::Cognition;
use newt_core::{
    BackendKind, Config, OpenAiApi, RuntimeSettingsSnapshot, TurnDriver, TurnDriverConfig,
    TurnStatus,
};

use crate::headless_contract;

/// Parsed `newt headless` arguments (mirrors the `Command::Headless` fields).
pub struct HeadlessArgs {
    pub cwd: PathBuf,
    pub instruction_file: PathBuf,
    pub profile: Option<PathBuf>,
    pub non_interactive: bool,
    /// EXPLICIT opt-in to OCAP-off ambient host execution (`--unsafe-host-exec`,
    /// or the `NEWT_UNSAFE_HOST_EXEC` env twin). This is the ONLY route to the
    /// full-access Yolo lane — `--non-interactive` never grants it. `--confined`
    /// still wins (confinement is never silently dropped). P3.
    pub unsafe_host_exec: bool,
    /// OCAP-ON confined bench lane: keep OCAP enabled and seed a workspace-fenced
    /// caveat (see [`confined_bench_caveats`]). Also enabled by the
    /// `NEWT_BENCH_OCAP=on` env twin (so the Harbor adapter can flip it without a
    /// flag). Wins over `--unsafe-host-exec`.
    pub confined: bool,
    pub events: Option<PathBuf>,
    pub smart_harness: bool,
    pub frame_dir: Option<PathBuf>,
    /// Distinguishes an explicit hermetic request from the legacy default.
    pub hermetic_explicit: bool,
    /// The validated continuity mode this run launched under.
    ///
    /// Already proved legal by `LaunchConfig::from_flags` at the call site, so
    /// nothing here re-checks it. Carried rather than re-derived so the record
    /// can state what the run could and could not inherit.
    pub launch: newt_launch::LaunchConfig,
    pub max_rounds: Option<usize>,
    /// The served model's FULL context window (e.g. llama.cpp `--ctx-size`).
    /// Newt gates input at the tighter of `[context].input_ceiling_pct` and the
    /// room left after the active cognition policy's maximum output, so a long
    /// turn compacts under the shared window instead of overrunning it during
    /// generation. None keeps Newt's default.
    pub context_window: Option<u32>,
    /// Operator-supplied sha256 of the weights actually served, for the
    /// contract record's `model_digest` (W0 #1511). Also settable via the
    /// `NEWT_MODEL_DIGEST` env twin. `None` ⇒ the field is OMITTED from the
    /// record — never fabricated (a name is not an identity, and a made-up
    /// digest would defeat the silent-re-upload detection the field exists
    /// for). Local-weights derivation would only apply to the embedded
    /// backend, which `headless` cannot drive (it needs an HTTP endpoint).
    pub model_digest: Option<String>,
    /// `--scratchpad-state`: the explicit starting state that opts the run into
    /// the scratchpad (#2314). `None` keeps the headless baseline.
    pub scratchpad_state: Option<PathBuf>,
    /// `--require-feature`: features the run must be able to supply (#2314).
    pub require_feature: Vec<headless_contract::Feature>,
    /// `--output-allowance`: explicit output-token allowance (#2312).
    pub output_allowance: Option<u32>,
    /// `--run-allowance`: explicit run-level call-count allowance (#2313).
    pub run_allowance: Option<u32>,
}

/// Seed a fresh scratchpad from `--scratchpad-state` JSON, returning the store
/// and the content id of the entries it actually holds — the receipt's `seed`.
/// Refused here, before any backend work, when the JSON is not an object of
/// string values or names an empty key (which `state_set` would also refuse).
/// F16: the model was never told its absolute workspace root — only the
/// tool schemas said paths are "relative to the workspace root", never what
/// that root *is*. Observed cost: replay 2488-r6 invented `cwd=/workspace`
/// (a test-fixture path, not a real one) and burned 30 minutes treating the
/// resulting "could not find Cargo.toml" as an external blocker. Seed the
/// driver's transcript with one system message naming the real root, the
/// same fact `newt-tui`'s interactive loop already states via its
/// `Workspace: {path}` line (`newt-tui/src/lib.rs`).
fn workspace_root_system_message(workspace: &str) -> newt_core::MemMessage {
    newt_core::MemMessage::system(format!("Workspace root: {workspace}"))
}

fn seed_scratchpad(json: &str) -> Result<(Arc<newt_core::SessionScratchpadStore>, String)> {
    use newt_core::ScratchpadStore;
    let entries: std::collections::BTreeMap<String, String> = serde_json::from_str(json)
        .context("--scratchpad-state must be a JSON object of string values")?;
    anyhow::ensure!(
        entries.keys().all(|k| !k.trim().is_empty()),
        "--scratchpad-state keys must be non-empty"
    );
    let store = Arc::new(newt_core::SessionScratchpadStore::default());
    for (key, value) in entries {
        store.set(&key, value);
    }
    let canonical = content_addressable::canonical::to_canonical_dagcbor(&store.entries())?;
    let seed = content_addressable::ContentId::from_canonical_bytes(&canonical).to_string();
    Ok((store, seed))
}

fn smart_launch(
    launch: &newt_launch::LaunchConfig,
    enabled: bool,
    hermetic_explicit: bool,
) -> newt_launch::LaunchConfig {
    if enabled && !hermetic_explicit && !launch.continuity.is_resumable() {
        newt_launch::LaunchConfig {
            continuity: newt_launch::Continuity::Resume { from: None },
        }
    } else {
        launch.clone()
    }
}

/// Which headless lane `newt headless` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadlessLane {
    /// OCAP on, workspace-fenced tool writes — the safe default and the
    /// `--confined` / `NEWT_BENCH_OCAP=on` parity lane.
    Confined,
    /// OCAP off, full host access — the explicit `--unsafe-host-exec` /
    /// `NEWT_UNSAFE_HOST_EXEC` opt-in only.
    Yolo,
}

/// Resolve the headless lane purely from the flags + env twins, so precedence
/// is unit-tested without a live run.
///
/// P3 (`noninteractive-launch-policy`): OCAP-off host access is now an
/// **explicit, unmistakable opt-in** — `--unsafe-host-exec` (or the
/// `NEWT_UNSAFE_HOST_EXEC` env twin) — NEVER a side effect of `--non-interactive`.
/// `--non-interactive` changes interaction only; it does not widen authority.
/// The precedence is fail-closed:
/// - `--confined` / `NEWT_BENCH_OCAP=on` → OCAP on, workspace-fenced writes (the
///   parity lane) — wins over an unsafe request (confinement is never silently
///   dropped);
/// - else `--unsafe-host-exec` / `NEWT_UNSAFE_HOST_EXEC` → Yolo (OCAP off, host
///   shell) — the sole route to unconfined execution;
/// - else → **Confined** (the safe default): OCAP stays on and writes are fenced
///   to the workspace. A plain `newt headless` is confined, not full-access.
fn resolve_lane(
    confined_flag: bool,
    ocap_env: Option<&str>,
    unsafe_host_exec: bool,
) -> HeadlessLane {
    let confined = confined_flag || ocap_env.is_some_and(|v| v.trim().eq_ignore_ascii_case("on"));
    if confined {
        HeadlessLane::Confined
    } else if unsafe_host_exec {
        HeadlessLane::Yolo
    } else {
        // Fail-closed default: OCAP retained, workspace-fenced. Ambient host
        // execution requires the explicit `--unsafe-host-exec` opt-in above.
        HeadlessLane::Confined
    }
}

/// Resolve the operator-supplied model digest: the `--model-digest` flag wins
/// over the `NEWT_MODEL_DIGEST` env twin (the same flag/env pattern as the
/// lane); blank values fall through. Pure so precedence is unit-tested
/// without racing the process environment. NEVER derives or invents a digest.
fn resolve_model_digest(flag: Option<&str>, env: Option<&str>) -> Option<String> {
    [flag, env]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|d| !d.is_empty())
        .map(str::to_string)
}

fn apply_context_config(driver: &mut TurnDriverConfig, context: Option<&newt_core::ContextConfig>) {
    let Some(context) = context else {
        return;
    };
    driver.compaction_trigger_policy = context.compaction_trigger_policy;
    driver.input_ceiling_pct =
        newt_core::config::normalize_input_ceiling_pct(context.input_ceiling_pct);
    driver.low_budget_pct = context.low_budget_pct;
    driver.estimation = context.estimation;
    driver.summary_input_cap_floor_chars = context.summary_input_cap_floor_chars;
    driver.context_manager = context.manager;
}

fn apply_context_window(driver: &mut TurnDriverConfig, context_window: u32) {
    let input_budget =
        newt_core::config::input_percentage_ceiling(context_window, driver.input_ceiling_pct);
    driver.safe_context = Some(input_budget);
    driver.max_ok_input = Some(input_budget);
    driver.num_ctx = Some(context_window);
}

/// Install the TYPED model family (the resolved card's declared metadata
/// under the same association gates as the capability decision — never
/// model-name substring inference) and resolve the posture snapshot.
fn resolve_runtime_posture(cfg: &Config, family: Option<&str>) -> RuntimeSettingsSnapshot {
    newt_core::initiative::set_active_model_family(family.map(str::to_string));
    RuntimeSettingsSnapshot::resolve(cfg, None, None)
}

fn headless_tool_round_limit(
    configured: usize,
    explicit_tenacity: Option<newt_core::Tenacity>,
    explicit_rounds: Option<usize>,
) -> usize {
    // `headless` enforces the same effective cap the TUI would, but it
    // does not persist per-turn records, so the derivation (`source`,
    // `configured`, `tenacity`) has nowhere durable to land here — an
    // asymmetry #1982 documents rather than hides. If headless ever grows turn
    // persistence, record the full `ToolRoundLimit` there too.
    newt_core::tenacity::resolve_tool_round_limit(configured, explicit_tenacity, explicit_rounds)
        .rounds
}

/// The serving principal `headless` decides capabilities for — the
/// SAME mapping the TUI's `BackendChoice::principal()` uses, so one
/// backend/card pair can never get exact capabilities + typed family in
/// chat but Undecided/no-family in headless (or vice versa):
///
/// * Instance ⇒ artifact identity;
/// * Multiplexer ⇒ the effective operator model is the pick;
/// * **no serving axis ⇒ `SelectedModel`** — headless requires a nonempty
///   effective operator model before this runs, and an operator-SELECTED
///   identity justifies exact association exactly as in chat.
fn headless_principal(
    serving: Option<newt_core::Serving>,
    model: &str,
) -> newt_core::model_card::ServingPrincipal<'_> {
    use newt_core::model_card::ServingPrincipal as P;
    match serving {
        Some(newt_core::Serving::Instance) => P::Instance,
        Some(newt_core::Serving::Multiplexer) => P::MultiplexerModel(model),
        None => P::SelectedModel(model),
    }
}

/// The cognition level this backend can actually receive from Newt. Responses
/// always has a defined `reasoning.effort` projection; Chat Completions must
/// explicitly opt into the local generation fields; Ollama has no projection.
/// This describes wire projection only; the driver retains semantic intent.
fn projected_cognition(
    cognition: Option<Cognition>,
    kind: BackendKind,
    api: OpenAiApi,
    chat_capability: ChatCompletionsCapability,
) -> Option<Cognition> {
    newt_core::agentic::projected_cognition(cognition, kind, api, chat_capability)
}

/// Run one task headless and emit its trace. Returns the process exit code:
/// `0` when the turn completed, `1` on an infrastructure/turn failure. (Task
/// pass/fail is Terminal-Bench's job via the task's own verification — this exit
/// code is only "did the agent run cleanly".)
pub async fn run(args: HeadlessArgs) -> Result<i32> {
    // 0. The explicit scratchpad seed is admitted before anything else runs.
    let scratchpad = match &args.scratchpad_state {
        Some(path) => Some(seed_scratchpad(
            &std::fs::read_to_string(path)
                .with_context(|| format!("reading --scratchpad-state {}", path.display()))?,
        )?),
        None => None,
    };
    // 1. Config: an explicit --profile is a FILE (Config::load); else the normal
    //    search order (Config::resolve — honors disk drop-ins + --backend-*).
    let cfg = match &args.profile {
        Some(path) => {
            // With the capability SIDECAR there is no load-time
            // materialization step any more: `Config::load` is a
            // deterministic parse; the FALLIBLE backend assembly
            // (`prepare_runtime`) then validates identity/destination and
            // applies the CLI request — a duplicate/empty backend name or
            // an invalid `--backend-*` is a hard error here, never a
            // warn-and-continue.
            resolve_profile(path)?
        }
        None => crate::migration_notices::read(|report| {
            newt_core::Config::resolve_runtime_unpublished(report)
        })
        .context("resolving config")?,
    };
    // NOTHING is published yet: the process-global settings land only after
    // the backend pick + capability sidecar VALIDATE below — a refused
    // selection or an unknown card must not leave half-published globals.

    // 2. Choose the headless lane (pure resolution → unit-tested precedence),
    //    then apply its process-env setup.
    //    SAFETY: single-threaded before the driver spawns its turn thread.
    // P3: `--non-interactive` is accepted for CLI back-compat and controls
    // INTERACTION only. `newt headless` is always headless (there is no prompt gate
    // to suppress), so the flag has no effect on authority — that is now solely
    // `--confined` / `--unsafe-host-exec` (the lane), never a side effect of it.
    let _non_interactive = args.non_interactive;
    let ocap_env = std::env::var("NEWT_BENCH_OCAP").ok();
    // P3: OCAP-off host access requires the explicit `--unsafe-host-exec` flag or
    // its `NEWT_UNSAFE_HOST_EXEC` env twin — NEVER `--non-interactive`.
    let unsafe_host = args.unsafe_host_exec
        || std::env::var("NEWT_UNSAFE_HOST_EXEC")
            .ok()
            .is_some_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "on" | "yes"
                )
            });
    let lane = resolve_lane(args.confined, ocap_env.as_deref(), unsafe_host);
    match lane {
        HeadlessLane::Confined => {
            // Keep OCAP enabled. DEFENSIVELY clear any inherited
            // `NEWT_DISABLE_OCAP` / `NEWT_FULL_ACCESS` — `ocap_disabled()` reads
            // those straight from the process env, so an inherited `=1` (a
            // wrapper/pod that also runs the `--yolo` lane) would silently route
            // every command to the host shell and void this lane's confinement
            // contract. Then pin the brush shell engine so compound commands run
            // even when the Landlock L3 fence is unavailable in the container (the
            // SafeSubset fallback refuses that grammar). Caveat seeded on `dc`.
            unsafe {
                std::env::remove_var("NEWT_DISABLE_OCAP");
                std::env::remove_var("NEWT_FULL_ACCESS");
                std::env::set_var("NEWT_SHELL_ENGINE", "brush");
            }
        }
        HeadlessLane::Yolo => {
            // The `--yolo --full-access` bootstrap lane: full access
            // (Caveats::top) AND OCAP disabled, so an UNRESTRICTED fs write
            // auto-accepts instead of waiting on a (nonexistent) prompt gate and
            // silently denying — the write path the benchmark depends on.
            unsafe {
                std::env::set_var("NEWT_FULL_ACCESS", "1");
                std::env::set_var("NEWT_DISABLE_OCAP", "1");
            }
        }
    }

    // Freeze the launch authority for this headless process now that the lane
    // has resolved its env twins (Confined cleared them; Yolo set them). Deep
    // libraries read the frozen value, so a later env mutation cannot widen
    // authority mid-run (noninteractive-launch-policy).
    newt_core::launch_authority::freeze(newt_core::launch_authority::LaunchAuthority::from_env());

    // 3. Backend: the shared TYPED selection contract — an explicit
    // $NEWT_PROVIDER/default_backend that cannot be honored is a hard error
    // (never a silent fallback), carried by pick_backend itself — paired
    // with the slot's own provenance receipt.
    let picked = pick_backend(&cfg)?;
    let backend = picked.backend;
    let url = backend.endpoint.clone();
    let model = backend
        .effective_model()
        .context("backend has no model (set model = in the [[backends]] entry)")?
        .to_string();
    let kind = backend.kind.unwrap_or(BackendKind::Openai);
    let api_key = backend.resolve_api_key();
    let api = backend.api.unwrap_or_default();
    // The SAME capability sidecar + typed principal decision the TUI uses —
    // one owner of the merge, one decision, no drift between lanes. Headless
    // has no adoption: the principal is the backend's own declaration
    // (Instance ⇒ the binding holds; a declared-model multiplexer ⇒ exact
    // bound-model equality, true by construction here; undeclared ⇒
    // conservative). An unknown named card is a hard error — headless fails
    // fast and names the fix rather than running without declarations the
    // operator believes are active.
    let pinned = newt_core::Config::pinned_config_path();
    let card_source = args.profile.as_deref().or(pinned.as_deref());
    // The binding evidence comes from the RECEIPT (pre-overlay declaration,
    // or an explicit --backend-card rebind) — never re-derived from the
    // flattened backend a CLI/session override may have retargeted.
    let caps = newt_core::model_card::ResolvedCapabilities::resolve(
        backend,
        &picked.receipt.binding,
        card_source,
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    let principal = headless_principal(backend.serving, &model);
    let destination = newt_core::BackendDestination::of(backend);
    let decision = caps.for_route(&destination, principal);
    if let Some(notice) = newt_tui::applicability_prose(decision.applicability()) {
        eprintln!("newt: {notice}");
    }
    let chat_capability = decision.chat_completions();
    // The pick + capability sidecar VALIDATED — only now do the resolved
    // configuration's process-global settings publish.
    cfg.publish_runtime_settings();
    // Surface the backend's wire API (`api = "responses"`) to the agentic loop,
    // exactly as the chat path does — without this, a responses-only model like
    // gpt-5.6-sol is driven over /v1/chat/completions and 400s on function tools.
    newt_tui::apply_openai_api_env(api);

    // Attribute the model's family so a per-family `[initiative]` config
    // default applies to this run (an explicit `--initiative` still supersedes it).
    // The family comes ONLY from the route-gated typed card decision (#1820) —
    // a cardless model gets no family regardless of what its name resembles.
    // W0 (#1511): the LEVEL this run resolves to, recorded verbatim in the
    // contract's effective_config — the bench never re-derives it from a
    // profile (contract requirement 5).
    let runtime = resolve_runtime_posture(&cfg, caps.family_for_route(&destination, principal));
    // #2314: a required feature this run cannot supply stops here, before the
    // instruction is read or anything reaches a backend.
    headless_contract::admit_required(&args.require_feature, |feature| match feature {
        headless_contract::Feature::Scratchpad => scratchpad.is_some(),
        headless_contract::Feature::Crew => runtime.crew,
        headless_contract::Feature::CodeSearch => false,
    })?;
    // #2312: the flag, else the model's `[[model_tuning]]` entry — the lookup
    // the TUI uses — else the cognition table and wire defaults downstream. An
    // unusable explicit value is refused here, like a required feature.
    let output_allowance = args
        .output_allowance
        .or_else(|| cfg.find_model_tuning(&model)?.output_allowance);
    newt_core::agentic::validate_output_allowance(output_allowance, args.context_window)?;
    // #2313: the flag, else the model's `[[model_tuning]]` entry -- same
    // precedence as output_allowance. No context-window validation applies:
    // a call-count budget has no relationship to the context window.
    let run_allowance = args
        .run_allowance
        .or_else(|| cfg.find_model_tuning(&model)?.run_allowance);
    let tenacity_level = runtime.tenacity.label();
    let initiative_level = runtime.initiative.label();
    let cognition = projected_cognition(runtime.cognition, kind, api, chat_capability);
    // `None` means Newt projects no cognition controls. Call that `default`
    // rather than `off`: a capable server may enable reasoning by default.
    let cognition_level = cognition.map_or("default", |level| level.label());

    // 4. The task instruction.
    let instruction_file = args.instruction_file.canonicalize().with_context(|| {
        format!(
            "resolving --instruction-file {}",
            args.instruction_file.display()
        )
    })?;
    let instruction = std::fs::read_to_string(&instruction_file)
        .with_context(|| format!("reading --instruction-file {}", instruction_file.display()))?;
    let workspace = args
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| args.cwd.clone())
        .to_string_lossy()
        .into_owned();

    // Herdr: report this headless run through the SAME lifecycle seam the TUI
    // uses (no surface-specific wiring). Inside a Herdr pane this installs the
    // bounded reporter actor and announces the session; outside one it is a
    // no-op subscription and `emit` stays a single atomic load. The guard
    // releases pane authority on every exit path of this function.
    let _herdr = newt_tui::herdr::session_guard(&workspace);
    let headless_session = newt_core::lifecycle::new_session_id();
    newt_core::lifecycle::set_active_session(&headless_session);
    newt_core::lifecycle::emit_for(
        Some(headless_session.as_str().to_string()),
        newt_core::lifecycle::LifecycleEvent::SessionStarted {
            session_id: headless_session.as_str().to_string(),
        },
    );

    // 5. Drive one full turn (== a complete multi-round agentic turn).
    let mut dc = TurnDriverConfig::new(&url, &model, kind, &workspace);
    apply_context_config(&mut dc, cfg.context.as_ref());
    dc.output_allowance = output_allowance;
    dc.run_allowance = run_allowance;
    dc.api_key = api_key;
    dc.chat_completions_capability = chat_capability;
    dc.responses_capability = decision.responses();
    dc.openai_api = api;
    dc.reasoning_replay_scope = decision.reasoning_replay_scope();
    // Paired with the line above ON PURPOSE. `headless` resolves the
    // model's capabilities from the same backend the TUI does; omitting this
    // left `headless` on the `false` default, so a declared reasoning model had
    // its chain-of-thought printed into the answer in headless runs only —
    // the N-call-sites trap, in the one lane with no human watching the
    // stream. `headless_takes_every_capability_from_the_decision`
    // (tests/capability_wiring.rs) pins the pair.
    dc.emits_leading_reasoning = decision.emits_leading_reasoning();
    dc.max_tool_rounds = headless_tool_round_limit(
        dc.max_tool_rounds,
        newt_core::tenacity::cli_tenacity(),
        args.max_rounds,
    );
    // OCAP-ON confined lane: replace the default unconfined caveat with a
    // workspace-fenced authority. The tool gate consults `dc.caveats` and the
    // permission_gate stays `None` — an in-fence write auto-consents; an
    // out-of-fence tool write is denied. This is the only seam the confined lane
    // touches; the driver + tool layer are unchanged.
    let smart_config = cfg.smart_harness.clone().unwrap_or_default();
    let smart_enabled = args.smart_harness || smart_config.enabled;
    // #2524 item 1: the operator's signed OCAP durable grants, folded into
    // `dc.caveats` below. Resolved once, outside the `Confined` branch, so the
    // admitted-grants list is available for the contract record regardless of
    // lane (empty in the `Yolo` lane, whose `Caveats::top()` has no `Only`
    // axis to widen).
    let (ocap_store, ocap_load_warnings) = resolve_ocap_store();
    for warning in &ocap_load_warnings {
        eprintln!("newt: OCAP policy: {warning}");
    }
    let mut ocap_admitted_grants: Vec<String> = Vec::new();
    if lane == HeadlessLane::Confined {
        // Scratch is resolved ONCE, here: it builds the fence AND becomes the
        // child's `TMPDIR` (the brush child does not inherit newt's ambient env;
        // the seam delivers this value, and without it the child's tools would
        // fall back to the platform temp dir, which a configured fence need not
        // grant).
        // Not for the smart lane, whose fence is workspace-only and whose callers
        // point temp at the workspace.
        let scratch = resolve_fence_scratch();
        if let (false, Some(root)) = (smart_enabled, scratch.first()) {
            // Same single-threaded-at-this-point contract as the lane's other
            // env writes above.
            unsafe { std::env::set_var("NEWT_CHILD_TMPDIR", root) };
        }
        dc.caveats = confined_bench_caveats(&workspace, &scratch);
        if smart_enabled {
            let scoped =
                newt_core::confined_exec::build_tool_caveats(std::path::Path::new(&workspace));
            // The smart lane's isolation needs a write fence with NO model-writable
            // ancestor of the workspace/frame, so it drops `/tmp` and takes the
            // workspace-only set; the default lane's fence must contain it
            // (`smart_fence_is_within_the_default_fence`).
            dc.caveats.fs_read = scoped.fs_read;
            dc.caveats.fs_write = scoped.fs_write;
            newt_core::caveats::apply_cli_fs_grants(&mut dc.caveats, &workspace);
        }
        // Fold AFTER the lane's own fence (and any smart-lane narrowing +
        // explicit CLI grants) is in place, so a durable grant widens the
        // final fence rather than one a later step immediately re-narrows.
        ocap_admitted_grants = fold_ocap_grants(&mut dc.caveats, &ocap_store);
    }
    let launch = smart_launch(&args.launch, smart_enabled, args.hermetic_explicit);
    anyhow::ensure!(
        smart_enabled || (args.frame_dir.is_none() && launch.continuity.parent_frame().is_none()),
        "frame storage and --resume-from require --smart-harness or [smart_harness] enabled = true"
    );
    let mut smart_manifest = None;
    let smart_harness = if smart_enabled {
        newt_core::agentic::smart_harness::validate_isolation_runtime()
            .context("smart-harness: isolation runtime check")?;
        let harness_launch = newt_core::config::HarnessLaunch {
            workspace: std::path::Path::new(&workspace),
            caveats: &dc.caveats,
            frame_dir: args.frame_dir.as_deref(),
            resume_from: launch.continuity.parent_frame(),
            hermetic: !launch.continuity.is_resumable(),
        };
        // Admit storage before loading or contacting the auxiliary. Explicit
        // broad CLI grants stay visible and are rejected if they expose it.
        smart_config.directory(&harness_launch).with_context(|| {
            format!(
                "smart-harness: resolving the frame directory (workspace {}, frame dir {})",
                harness_launch.workspace.display(),
                harness_launch
                    .frame_dir
                    .map(|d| d.display().to_string())
                    .unwrap_or_else(|| "<default>".into())
            )
        })?;
        let auxiliary = newt_inference::smart_harness::build(&smart_config, &url, kind)
            .with_context(|| format!("smart-harness: building the auxiliary against {url}"))?;
        let mut manifest = auxiliary.manifest;
        manifest["primary_api"] = serde_json::json!(api.label());
        // An io::Error surfaced through `?` says only "No such file or directory";
        // name the stage and the paths so a refusal is actionable (observed: a
        // pre-inference ENOENT inside a Harbor task container with no path at all).
        let session = smart_config
            .open_session(&harness_launch, manifest.clone())
            .with_context(|| {
                format!(
                    "smart-harness: opening the session (workspace {}, frame dir {})",
                    harness_launch.workspace.display(),
                    harness_launch
                        .frame_dir
                        .map(|d| d.display().to_string())
                        .unwrap_or_else(|| "<default>".into())
                )
            })?;
        let session_config = serde_json::to_value(session.config())?;
        smart_manifest = Some(serde_json::json!({
            "invocation_mode": if launch.continuity.parent_frame().is_some() { "resume" } else { "fresh" },
            "starting_cid": launch.continuity.parent_frame(),
            "configuration": session_config,
        }));
        let harness = Arc::new(newt_core::agentic::smart_harness::SmartHarness::new(
            session,
            auxiliary.complete,
            smart_config.adjudication.clone(),
        )?);
        dc.smart_harness = Some(harness.clone());
        Some(harness)
    } else {
        None
    };
    // Pin the model's served context window so the loop's pre-send guard +
    // compaction keep each request under the backend's `--ctx-size` (e.g. dgx1
    // llama.cpp serves qwen3-coder at 32768). `--context-window` is the FULL
    // served window. `safe_context` starts at the configured percentage bound;
    // the OpenAI loop then applies the tighter `full_window - max_tokens` bound
    // for the active cognition policy. Because the KV window is shared by input
    // and output, gating on the undiscounted window overruns it during
    // generation. `num_ctx` is inert on the OpenAI wire but carries the full
    // window locally (and remains a real wire option on the Ollama path).
    if let Some(cw) = args.context_window {
        apply_context_window(&mut dc, cw);
    }
    // Captured before `dc` moves into the driver: the cap the run ACTUALLY
    // uses (post `--max-rounds`), for the contract's effective_config.
    let max_rounds = dc.max_tool_rounds as u32;
    let read_scope = dc.caveats.fs_read.clone();
    let mut driver =
        TurnDriver::with_transcript(dc, vec![workspace_root_system_message(&workspace)])
            .with_cognition(runtime.cognition)
            .with_tenacity(runtime.tenacity)
            .with_initiative(runtime.initiative);
    if runtime.crew {
        driver = driver.with_crew_runner(Arc::new(crate::crew_runner::LocalCrewRunner::new(
            // The crew runner needs an owned flattened Config; the
            // receipts stay with this run.
            newt_core::Config::clone(&cfg),
            PathBuf::from(&workspace),
            newt_core::agentic::Presence::Prompt,
        )));
    }
    if let Some((store, _)) = &scratchpad {
        driver = driver.with_scratchpad(store.clone());
    }
    let crew_level = if driver.has_crew_runner() {
        "on"
    } else {
        "off"
    };
    // #2552 round 3 (F26 v4): decide WHERE `workspace` sits relative to a
    // repo — its own toplevel, a SUBDIRECTORY of a larger one, or no repo at
    // all — ONCE, and make every git source below obey the SAME three-way
    // decision. Round 1's bug was `status_before.is_none()` (a symptom) and
    // `GitEngine::open(workspace).is_some()` (a DIFFERENT, upward-discovering
    // check) disagreeing. Round 2 collapsed "subdirectory" and "not a repo"
    // into one case and dropped subtree-scoped status entirely for it — round
    // 2's review: that re-creates the exact "nothing changed" bug this PR
    // fixes for the everyday `--cwd repo/crate` invocation, which has no
    // nested repos of its own to fall back on.
    let repo_location = newt_core::agentic::locate_workspace_repo(&workspace, &read_scope);
    // U7: the workspace's changed-path set at run start, diffed against the same
    // probe at exit so the hand-back names what THIS run changed, not prior dirt.
    // `Root`-shaped only at the repo's own toplevel; `None` off-repo. A
    // subdirectory's own (subtree-scoped) delta is computed later, in
    // `files_changed_delta`, since it needs the prefix AND an `after` probe.
    let status_before = matches!(
        repo_location,
        newt_core::agentic::WorkspaceRepoLocation::OwnRoot
    )
    .then(|| newt_core::agentic::snapshot_workspace(&workspace, &read_scope))
    .flatten();
    // #2552 round 3: the subtree-scoped equivalent of `status_before`, for a
    // workspace that is a SUBDIRECTORY of a larger repo — restores F26 v2's
    // scoping as a real third case rather than dropping it (round 2's
    // review: dropping it re-creates the "nothing changed" bug this PR fixes
    // for the everyday `--cwd repo/crate` invocation).
    let subtree_before = match &repo_location {
        newt_core::agentic::WorkspaceRepoLocation::InsideRepo { prefix } => {
            newt_core::agentic::snapshot_workspace_subtree(&workspace, prefix, &read_scope)
        }
        _ => None,
    };
    // Multi-repo recon PR2: nested first-level repos can exist whether
    // `workspace` is a subdirectory of a larger repo OR not a repo at all —
    // gated on NOT being the repo's own toplevel, same as `status_before`'s
    // complement, never on whether a status snapshot came back.
    let nested_before = (!matches!(
        repo_location,
        newt_core::agentic::WorkspaceRepoLocation::OwnRoot
    ))
    .then(|| newt_core::agentic::snapshot_nested_repos(&workspace, &read_scope));
    // #2537: HEAD before the run, via newt-git's OWN embedded engine (never a
    // shelled-out `git`) — the baseline the hand-back diffs against to name
    // commit(s) this run actually produced. The engine is opened whenever
    // `workspace` is inside SOME repo (own root OR a subdirectory of one) —
    // `commits`/HEAD use ONLY the engine's `head_snapshot`/`commits_since`
    // (run-window commit IDs, no path content), never its STATUS methods, so
    // opening it for a subdirectory of a larger repo cannot leak a path
    // (#2552 round 3: an unannounced commit to the enclosing repo is exactly
    // what an operator running from `repo/crate` most needs to see). `None`
    // off-repo, or when the read scope can't open the legacy engine (bounded
    // fs_read); the hand-back then reports "unavailable"/omits `commits`,
    // never a guess.
    let head_before = (!matches!(
        repo_location,
        newt_core::agentic::WorkspaceRepoLocation::NotARepo
    ))
    .then(|| newt_git::GitEngine::open(std::path::Path::new(&workspace), &read_scope).ok())
    .flatten()
    .and_then(|e| {
        e.head_snapshot(&newt_core::git_caveats::GitCaveats::read_only())
            .ok()
    });
    let started = Instant::now();
    driver
        .submit(instruction.trim())
        .map_err(|e| anyhow::anyhow!("submit failed: {e:?}"))?;
    // A `headless` run IS a real model turn — announce it through the seam so the
    // pane shows Working for the duration of the driver loop.
    newt_core::lifecycle::emit(newt_core::lifecycle::LifecycleEvent::TurnStarted);

    let outcome = loop {
        match driver.poll() {
            TurnStatus::Completed(o) => break Ok(o),
            TurnStatus::Failed(e) => break Err(e),
            TurnStatus::Idle | TurnStatus::Running => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    };
    match &outcome {
        Ok(o) if o.error.is_none() => {
            newt_core::lifecycle::emit(newt_core::lifecycle::LifecycleEvent::TurnCompleted);
        }
        Ok(o) => {
            newt_core::lifecycle::emit(newt_core::lifecycle::LifecycleEvent::TurnFailed {
                reason: o.error.clone(),
            });
        }
        Err(e) => {
            newt_core::lifecycle::emit(newt_core::lifecycle::LifecycleEvent::TurnFailed {
                reason: Some(e.clone()),
            });
        }
    }
    let elapsed = started.elapsed();
    let wall_secs = elapsed.as_secs_f64();
    // Contract timing is integral milliseconds.
    let wall_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);

    // 6. Emit the trace record (one JSONL line), including the per-tool-call
    //    trajectory (name/args-digest/ok/duration) the TurnDriver now lends —
    //    the material for the failure taxonomy.
    // A part-way inference failure now arrives as `Ok(o)` with `o.error` set —
    // so its PARTIAL trajectory is preserved (an infra failure must not report
    // the agent as having done nothing). `Err` is only a spawn/thread failure
    // with no trajectory at all.
    let o_opt = outcome.as_ref().ok();
    // A failed `head()` used to `?`-return before ANY record was written. It is
    // now a failed run that still hands back its record (U7).
    let head_error = match (&mut smart_manifest, &smart_harness) {
        (Some(manifest), Some(harness)) => match harness.head() {
            Ok(head) => {
                manifest["head"] = serde_json::json!(head.to_string());
                None
            }
            Err(e) => Some(format!("smart-harness head unavailable: {e}")),
        },
        _ => None,
    };
    let clean = head_error.is_none() && matches!(&outcome, Ok(o) if o.error.is_none());
    // ONE derivation, two renderings (#2212). `outcome` is decided first and
    // `status` is a function of it, so the trace line and the contract record
    // cannot disagree about whether the turn finished. They used to be
    // independent expressions over the same `o`, twenty-four lines apart, and
    // a round-cap exit satisfied one and not the other.
    let terminal = headless_contract::terminal(
        clean,
        match &outcome {
            Ok(o) => o.error_class,
            Err(_) => None,
        },
        o_opt.and_then(|o| o.end_reason),
        smart_harness.is_some(),
    );
    let outcome_label = headless_contract::outcome_label(terminal);
    let status = headless_contract::status_label(terminal);
    let error = head_error.or_else(|| match &outcome {
        Ok(o) => o.error.clone(),
        Err(e) => Some(e.clone()),
    });
    let reply_chars = o_opt.map(|o| o.reply.len()).unwrap_or(0);
    let usage = o_opt.and_then(|o| o.usage.as_ref().map(|u| u.total()));
    let halluc = o_opt.map(|o| o.hallucinations).unwrap_or(0);
    // The per-tool trajectory — the material for the failure taxonomy. The
    // single highest-signal field is `write_calls`: a failed task with 0 writes
    // never ACTED (the initiative target); with writes it acted but wrong. Only
    // newt's real workspace-write tools count, via `is_workspace_write_call`
    // itself rather than a second copy of its literal set — aliases like
    // `create_file`/`str_replace`/`apply_patch` get a coaching reply and never
    // modify the tree.
    let (tool_calls, write_calls, calls_after_last_write, end_reason, trajectory) = match o_opt {
        Some(o) => {
            let writes = o
                .tool_events
                .iter()
                .filter(|e| newt_core::agentic::is_workspace_write_call(&e.tool))
                .count();
            (
                o.tool_events.len(),
                writes,
                headless_contract::calls_after_last_write(&o.tool_events),
                format!("{:?}", o.end_reason),
                serde_json::to_value(&o.tool_events).unwrap_or(serde_json::Value::Null),
            )
        }
        None => (0, 0, None, "None".to_string(), serde_json::Value::Null),
    };
    // #2537: hand-back commit truth. Re-open the embedded engine at exit
    // (the working tree may have changed under an already-open handle) and
    // ask its OWN `status`/`log` — never a shelled-out `git` — for what this
    // run left dirty and what it committed. The repo location is re-checked
    // fresh (in case the run itself `git init`ed the workspace — #2552 round
    // 2). The engine is opened whenever `workspace` is inside SOME repo (own
    // root OR a subdirectory of one — #2552 round 3), but its STATUS methods
    // (`uncommitted_paths`) are used ONLY at the repo's own toplevel:
    // `GitEngine::open` discovers upward exactly like the shelled `git` did
    // before F26, so its unscoped status would hand a workspace-subdirectory
    // run the ENCLOSING repo's dirty set (#2552 round 2 blocker 1) — a
    // subdirectory's current dirty set instead comes from
    // `snapshot_workspace_subtree` inside `uncommitted_repo_file_list`,
    // below. The engine's `commits_since` carries only run-window commit
    // IDs, never path content, so it IS safe to use for a subdirectory too
    // (round 3: an unannounced commit to the enclosing repo is what an
    // operator running from `repo/crate` most needs to see).
    let git_caveats = newt_core::git_caveats::GitCaveats::read_only();
    let repo_location = newt_core::agentic::locate_workspace_repo(&workspace, &read_scope);
    let is_root_repo = matches!(
        repo_location,
        newt_core::agentic::WorkspaceRepoLocation::OwnRoot
    );
    let git_engine = (!matches!(
        repo_location,
        newt_core::agentic::WorkspaceRepoLocation::NotARepo
    ))
    .then(|| newt_git::GitEngine::open(std::path::Path::new(&workspace), &read_scope).ok())
    .flatten();
    let uncommitted_at_exit = is_root_repo
        .then(|| {
            git_engine
                .as_ref()
                .and_then(|e| uncommitted_paths(e, &git_caveats))
        })
        .flatten();
    let uncommitted_delta = uncommitted_repo_file_list(
        uncommitted_at_exit,
        &repo_location,
        &nested_before,
        &workspace,
        &read_scope,
    );
    let commits_this_run = git_engine.as_ref().and_then(|engine| {
        commits_since(
            engine,
            &git_caveats,
            head_before.as_ref().and_then(|h| h.head.as_deref()),
        )
    });
    let mut record = serde_json::json!({
        "kind": "solve_result",
        "task_file": instruction_file.to_string_lossy(),
        "cwd": workspace,
        "model": model,
        "endpoint": url,
        "backend_kind": kind.label(),
        "status": status,
        "reply_chars": reply_chars,
        "usage_total_tokens": usage,
        "hallucinations": halluc,
        "wall_secs": wall_secs,
        "tool_calls": tool_calls,
        "write_calls": write_calls,
        // Calls spent after the last SUCCESSFUL write (#2214) — what the
        // rounds went ON, which `end_reason: RoundCap` cannot say. Thrash, a
        // too-small cap, and write-complete-then-grind are one value today;
        // this separates the third, and only the third deserves "raise the
        // cap". `null` means the run never landed a write at all.
        //
        // A THRESHOLD is deliberately not applied here: the integer is the
        // measurement, and which tail length counts as a grind is the
        // consumer's call. It gates on `ok`, unlike `write_calls` above.
        //
        // Deliberately on the solve_result line and NOT in the contract
        // record, for the same reason as `continuity` below: the record is
        // parsed by gilamonster-bench with its own re-declared structs, and
        // adding a field there needs the unknown-field question answered
        // first (#2218).
        "calls_after_last_write": calls_after_last_write,
        "end_reason": end_reason,
        // What this run could and could not inherit. A cap exit under
        // `hermetic` is a failure — there was no continuation available by
        // construction — while the same `end_reason` under `resume` may be a
        // pause. Without this a reader cannot tell those apart, which is how a
        // failure class gets attributed to the wrong cause.
        //
        // Deliberately on the solve_result line and NOT in the contract record:
        // the record is parsed by gilamonster-bench with its own re-declared
        // structs, and adding a field there needs the unknown-field question
        // answered first (#2218). This line is newt's own.
        "continuity": launch.continuity.as_str(),
        "continuity_note": launch.describe(),
        "resume_from": launch.continuity.parent_frame(),
        "trajectory": trajectory,
        "error": error,
        // Harness-written, never the model's claim; solve_result only (the
        // contract record is field-pinned). Carries no hash or id.
        "handback": headless_contract::handback(
            files_changed_delta(
                &status_before,
                &subtree_before,
                &repo_location,
                &nested_before,
                &workspace,
                &read_scope,
            ),
            o_opt.map_or(&[][..], |o| &o.tool_events[..]),
            &end_reason,
            reply_chars > 0,
            uncommitted_delta,
            commits_this_run.clone(),
        ),
    });
    headless_contract::conditional_stanza(&mut record, "smart_harness", smart_manifest.clone());
    // #2313: per-attempt usage, and the attempt ledger's chain lines ahead of
    // this line, but only into a trace that is kept (`--events`); the head is
    // reported only alongside them.
    let attempt_lines = match (o_opt, &args.events) {
        (Some(o), Some(_)) => &o.attempt_lines[..],
        _ => &[],
    };
    let usage_stanza = o_opt.and_then(|o| o.attempts).map(|totals| {
        let local = newt_core::owned_hosts::inference_is_local(
            kind == newt_core::BackendKind::Embedded,
            Some(&url),
        );
        let cost = cfg
            .pricing
            .clone()
            .unwrap_or_default()
            .estimate_attempts_cost(&model, local, &totals);
        let head = attempt_lines.last().map(|line| line.id.as_str());
        headless_contract::usage_stanza(&totals, cost, head)
    });
    headless_contract::conditional_stanza(&mut record, "usage", usage_stanza);
    // 7. W0 (#1511): the per-round parse-signal trace events plus EXACTLY ONE
    //    contract record (the `contract_version` key marks it — the external
    //    evaluator rejects a trace with zero or several), appended alongside
    //    the solve_result line above, never replacing it.
    let mut trace_lines: Vec<serde_json::Value> = attempt_lines
        .iter()
        .map(headless_contract::attempt_line)
        .collect();
    trace_lines.push(record);
    if let Some(o) = o_opt {
        trace_lines.extend(
            o.parse_signals
                .iter()
                .map(headless_contract::parse_signal_line),
        );
        trace_lines.extend(
            o.behavior_signals
                .iter()
                .map(headless_contract::behavior_signal_line),
        );
    }
    // effective_model: the response body's `model` field when the backend
    // reported one (the served reality), else the request model — a turn
    // that never got a response body has nothing truer to report.
    let effective_model = o_opt
        .and_then(|o| o.served_model.clone())
        .unwrap_or_else(|| model.clone());
    // Digest: operator-supplied only (flag > NEWT_MODEL_DIGEST env twin);
    // absent ⇒ the field is omitted — never fabricated.
    let digest_env = std::env::var("NEWT_MODEL_DIGEST").ok();
    let model_digest = resolve_model_digest(args.model_digest.as_deref(), digest_env.as_deref());
    trace_lines.push(headless_contract::contract_record(
        &headless_contract::ContractInputs {
            requested_model: &model,
            effective_model: &effective_model,
            model_digest: model_digest.as_deref(),
            backend_name: &backend.name,
            backend_kind: kind.label(),
            outcome: outcome_label,
            context_window: args.context_window,
            tenacity: tenacity_level,
            initiative: initiative_level,
            cognition: cognition_level,
            semantic_cognition: o_opt.map(|o| o.semantic_cognition),
            responses_capability: o_opt.and_then(|o| o.responses_capability.as_ref()),
            reasoning_effort: o_opt.and_then(|o| o.reasoning_effort),
            crew: crew_level,
            ocap: if lane == HeadlessLane::Yolo {
                "off"
            } else {
                "on"
            },
            max_rounds,
            wall_ms,
            gen_tokens: o_opt
                .and_then(|o| o.usage.as_ref())
                .map(|u| u64::from(u.output_tokens)),
            smart_harness: smart_manifest.as_ref(),
            output_allowance: o_opt.and_then(|o| o.output_allowance),
            run_allowance,
            features: o_opt.map(|o| o.features),
            durable_grants: &ocap_admitted_grants,
            // The headless driver always arms action nudges; whether the loop
            // has a gate at all depends on the wire and SmartHarness.
            verification: Some(newt_core::agentic::verification_receipt(
                newt_core::agentic::verification_gate_present(kind, api, smart_manifest.is_some()),
                newt_core::agentic::self_verify_enabled(),
                newt_core::agentic::verify_outcomes_requested(),
            )),
            scratchpad_seed: scratchpad.as_ref().map(|(_, seed)| seed.as_str()),
            required: &args.require_feature,
        },
    ));
    // #2372: the Chat and Ollama loops return their answer unprinted, so show it
    // here, before anything that can fail — the model's final claim is what a
    // transcript tail must carry. `▸` marks a model claim only; harness-written
    // text (an empty-response note, a cap fallback) is a harness notice.
    if let Some(o) = o_opt.filter(|o| !o.was_streamed && !o.reply.is_empty()) {
        if o.harness_reply {
            newt_core::agentic::print_harness_notice(&o.reply, false);
        } else {
            println!("▸  {}", o.reply);
        }
    }
    if let Some(path) = &args.events {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening --events {}", path.display()))?;
        for line in &trace_lines {
            writeln!(f, "{line}").context("writing events line")?;
        }
    }
    // Always echo the trace to stdout too, so a manual bootstrap run is legible.
    for line in &trace_lines {
        println!("{line}");
    }

    // Clean completion → 0; any failure (inference error carried on the outcome,
    // or a spawn/thread Err) → 1.
    Ok(if clean { 0 } else { 1 })
}

/// Load an explicit `--profile` FILE and run it through the fallible
/// backend assembly — identity/destination validation, CLI `--backend-*`
/// request, route/kind normalization, receipts — WITHOUT publishing any
/// process-global state, so callers (and tests) can validate first and
/// publish explicitly.
fn resolve_profile(path: &std::path::Path) -> Result<newt_core::ResolvedConfig> {
    use anyhow::Context as _;
    crate::migration_notices::read(|report| Config::load(path, report))
        .with_context(|| format!("loading --profile {}", path.display()))?
        .prepare_runtime()
        .with_context(|| format!("preparing --profile {}", path.display()))
}

/// Pick the backend to drive. Delegates to the **shared, typed** selection
/// contract (#1320, PR-3) so `headless` selects exactly as chat + the worker
/// do: `NEWT_PROVIDER` > `default_backend` > sole > prefer-OpenAI, else
/// first routable — and FAILS, naming the selector, when the explicit
/// selection cannot be honored. Headless must never silently run a
/// fallback the operator did not select: an unknown name, a
/// destination-less backend, and a provider (which `headless` cannot drive)
/// are each their own actionable error, not a cue to pick something else.
fn pick_backend(
    resolved: &newt_core::ResolvedConfig,
) -> anyhow::Result<newt_core::ResolvedBackend<'_>> {
    use newt_core::config::{SelectedBackend, SelectionOutcome};
    match resolved.select_backend() {
        SelectionOutcome::Selected(SelectedBackend::Configured(backend)) => {
            // The shared contract treats an embedded (model_path) backend as
            // fully routable — but `newt headless` only instantiates the HTTP
            // turn driver, and an empty endpoint would fall into the Ollama
            // loop against nothing. Refuse typed instead.
            if backend.endpoint.is_empty() || backend.kind == Some(BackendKind::Embedded) {
                anyhow::bail!(
                    "backend `{}` is an embedded (model_path) backend — `newt headless` \
                     drives HTTP backends only; select an HTTP backend, or give this \
                     one an endpoint",
                    backend.name
                );
            }
            // The SAME shared index selector pairs the pick with its
            // provenance receipt — never a name lookup.
            Ok(resolved
                .selected_backend()
                .expect("the shared index selector just picked a configured backend"))
        }
        SelectionOutcome::Selected(SelectedBackend::Provider(p)) => anyhow::bail!(
            "the backend selection resolves to provider `{}` — `newt headless` drives \
             configured [[backends]] only; point $NEWT_PROVIDER/default_backend at a \
             backend (or unset them)",
            p.name
        ),
        SelectionOutcome::UnknownNamed(name) => anyhow::bail!(
            "$NEWT_PROVIDER/default_backend names `{name}`, which matches nothing \
             configured — fix the selector (headless will not silently run another backend)"
        ),
        SelectionOutcome::UnroutableNamed(name) => anyhow::bail!(
            "$NEWT_PROVIDER/default_backend names `{name}`, which has neither an \
             endpoint nor a model_path — give it a destination (headless will not \
             silently run another backend)"
        ),
        SelectionOutcome::Unset => anyhow::bail!(
            "no usable backend in config (set one in the --profile [[backends]] or via \
             --backend-endpoint)"
        ),
    }
}

/// The workspace-fenced authority for the OCAP-ON bench lane.
///
/// Reads / exec / net stay fully open (a bench task legitimately reads the
/// whole tree, runs arbitrary toolchains, and installs packages over the
/// network), but **writes are fenced** to the workspace, the configured scratch
/// root (see [`fence_scratch_roots`]) and any explicit `NEWT_WRITE_PATHS` grant.
/// The mutable system roots and `$HOME` are opt-in by grant (#1487's bench
/// rationale preserved: a container passes them; a default must be safe on a
/// developer host). The fence is a `Scope::Only`, **never** `Scope::All`: that is
/// the whole point of the lane (a `Scope::Only` write auto-consents at the tool
/// gate, so in-fence writes run promptless while a write to an un-granted
/// absolute path fails closed), and it is what the 0.7.6 OCAP-parity gate
/// measures.
///
/// Matching at the enforcement site (`tools::tui_permits_path`) is by
/// lexically-normalized path **prefix**, so a root entry covers everything
/// beneath it (`/usr` grants `/usr/lib/python3/...`).
fn confined_bench_caveats(workspace: &str, scratch: &[String]) -> Caveats {
    // The per-task extra write grants the harness may pass (same env the
    // interactive `--write` grants flow through). `split_paths` keeps a Windows
    // drive-letter grant intact rather than shattering it on `:`. Read here so
    // the pure core below stays env-free (and unit-testable without racing the
    // process-global environment).
    let extra: Vec<String> = std::env::var_os("NEWT_WRITE_PATHS")
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    confined_bench_caveats_with_grants(workspace, scratch, &extra)
}

/// The fence's scratch root(s), resolved at run start. Scratch is CONFIGURATION
/// (`NEWT_SCRATCH_DIR` / `[scratch] dir`, via the one existing resolver), never a
/// literal; see [`fence_scratch_roots`].
fn resolve_fence_scratch() -> Vec<String> {
    fence_scratch_roots(&newt_core::scratch::scratch_dir(), &std::env::temp_dir())
}

/// The fence's scratch root(s): the configured scratch dir when it is ABSOLUTE
/// (an operator relocated it, e.g. to `/var/tmp` or a PVC), else the platform
/// temp dir (`temp_dir`, which honours `TMPDIR`). A relative dir — including the
/// `.scratch` default — already lives inside the workspace, so it is "nothing
/// configured" for this purpose. Pure. There is no way to drop the scratch root
/// through configuration today; see RESULT-2501 for the proposed key.
fn fence_scratch_roots(scratch_dir: &str, temp_dir: &std::path::Path) -> Vec<String> {
    if std::path::Path::new(scratch_dir).is_absolute() {
        vec![scratch_dir.to_string()]
    } else {
        vec![temp_dir.to_string_lossy().into_owned()]
    }
}

/// Fold this run's VERIFIED OCAP `approve.toml` entries (#2524 item 1) into
/// `caveats`: attenuate-only, never a bypass. `newt_core::widen_caveats`
/// only ever *inserts into an already-`Scope::Only` axis* — an axis left at
/// `Scope::All` (reads/exec/net in the default confined lane,
/// [`confined_bench_caveats`]) is untouched, so folding can never turn an
/// open axis into something narrower, and it can never turn a fenced axis
/// open either. It only reaches a fenced (`Scope::Only`) axis — exactly the
/// smart-harness lane's `confined_exec::build_tool_caveats` fence, the
/// "canvas" case the brief names (a child MCP server needs a path outside the
/// workspace opened in `fs_read`).
///
/// `store` must already be the output of [`newt_core::ocap_store::load_store`],
/// which drops any unsigned/bad-signature approve loudly at load
/// (fail-closed); this only re-shapes what survived that check via
/// [`newt_core::ocap_store::approved_grants`] (pure), so it never itself
/// widens past what the signature covers, and it never even sees an
/// unverified entry to fold. Returns the admitted grants as
/// `"<axis>:<target>"` labels for the contract receipt (#2524 item 1
/// observability requirement) — never invents a second receipt shape.
fn fold_ocap_grants(
    caveats: &mut Caveats,
    store: &newt_core::ocap_store::PolicySet,
) -> Vec<String> {
    // Defence in depth (#2532 review, should-fix 2): `load_store` only checks
    // the signature, not danger — `sign_ocap` refuses a High-danger target
    // today, but a differently-signed store (an older build, a script with
    // the key) is not re-checked here. Filter through the SAME production
    // danger predicate `sign_ocap` already blesses against, so headless can
    // never admit a signed `/`/`$HOME`/interpreter grant the TUI would drop
    // (`recalled_grants`, `permissions.rs`).
    let is_high_danger = newt_tui::ocap_high_danger_predicate();
    let grants: Vec<_> = newt_core::ocap_store::approved_grants(store)
        .into_iter()
        .filter(|(kind, target)| !is_high_danger(denial_kind_to_capability_class(*kind), target))
        .collect();
    if grants.is_empty() {
        return Vec::new();
    }
    // Contract honesty (#2532 review, item 3): a grant onto an axis that is
    // already `Scope::All` (or already covers this exact target) changes
    // nothing — recording it plainly as "admitted" implies the operator's
    // signature is why the tool can act, when the confined lane already
    // allowed it. Check "would this actually widen?" BEFORE folding (the
    // same `CaveatsExt::permits_*` the enforcement path checks against) and
    // label a no-op instead of dropping it, so the store entry is still
    // visible on the contract but not mistaken for the reason access worked.
    let already_permitted: Vec<_> = grants
        .iter()
        .map(|(kind, target)| grant_already_permitted(caveats, *kind, target))
        .collect();
    *caveats = newt_core::widen_caveats(caveats, &grants);
    grants
        .into_iter()
        .zip(already_permitted)
        .map(|((kind, target), no_op)| {
            let label = format!("{}:{target}", ocap_grant_axis_label(kind));
            if no_op {
                format!("{label} (no-op: already permitted)")
            } else {
                label
            }
        })
        .collect()
}

/// Would folding this grant change anything? Mirrors [`CaveatsExt`]'s
/// per-axis check so the contract only records a grant that actually widened
/// a fenced axis (see [`fold_ocap_grants`]).
fn grant_already_permitted(caveats: &Caveats, kind: newt_core::DenialKind, target: &str) -> bool {
    use newt_core::caveats::CaveatsExt;
    match kind {
        newt_core::DenialKind::FsRead => caveats.permits_fs_read(target),
        newt_core::DenialKind::FsWrite => caveats.permits_fs_write(target),
        newt_core::DenialKind::Exec => caveats.permits_exec(target),
        newt_core::DenialKind::Net => caveats.permits_net(target),
        _ => false,
    }
}

/// Map an [`approved_grants`](newt_core::ocap_store::approved_grants) axis
/// back to the [`CapabilityClass`](newt_core::ocap_store::CapabilityClass)
/// `ocap_high_danger_predicate` expects. `FsRead` and `FsWrite` both fold to
/// `Fs` — the predicate already classifies fs as a write (the conservative
/// reading of a durable fs grant); the axes this function never sees
/// (`RemoteTool`/`GitWrite`/`Build`) are not produced by `approved_grants`.
fn denial_kind_to_capability_class(
    kind: newt_core::DenialKind,
) -> newt_core::ocap_store::CapabilityClass {
    match kind {
        newt_core::DenialKind::Exec => newt_core::ocap_store::CapabilityClass::Exec,
        newt_core::DenialKind::Net => newt_core::ocap_store::CapabilityClass::Net,
        _ => newt_core::ocap_store::CapabilityClass::Fs,
    }
}

/// The contract-record axis label for a folded grant — `newt_core::DenialKind`
/// has no `Display`/label of its own outside the danger table, and this is
/// the one place that needs a short, stable string.
fn ocap_grant_axis_label(kind: newt_core::DenialKind) -> &'static str {
    match kind {
        newt_core::DenialKind::Exec => "exec",
        newt_core::DenialKind::FsRead => "fs_read",
        newt_core::DenialKind::FsWrite => "fs_write",
        newt_core::DenialKind::Net => "net",
        newt_core::DenialKind::RemoteTool => "remote_tool",
        newt_core::DenialKind::GitWrite => "git_write",
        newt_core::DenialKind::Build => "build",
    }
}

/// Load the GLOBAL operator OCAP store (`~/.newt/ocap/*.toml`, via
/// `Config::user_config_path` — **never** a repo `.newt/config.toml**: the
/// ambient/project config path is a different resolver entirely, so this
/// simply never consults it, the same guard class as the lifecycle-pin
/// ambient-config exclusion). A missing config dir (no `$HOME`, isolated
/// test env with nothing configured) yields an empty store, not an error —
/// headless without OCAP configured behaves exactly as it does today.
fn resolve_ocap_store() -> (newt_core::ocap_store::PolicySet, Vec<String>) {
    let Some(config_path) = newt_core::Config::user_config_path() else {
        return (newt_core::ocap_store::PolicySet::default(), Vec::new());
    };
    // Read-only: `load_user_key` errors (never mints) when the key is absent,
    // so a headless run on a fresh host/CI/scratch `--config-dir` never
    // writes an identity.pem as a side effect of checking for approves — "no
    // key" folds down to "no approves", already `load_store`'s rule for a
    // missing/invalid root key.
    let root_vk = newt_identity::default_key_path()
        .ok()
        .and_then(|p| newt_identity::load_user_key(&p).ok())
        .map(|user| user.public().as_bytes());
    newt_core::ocap_store::load_store(&config_path, root_vk)
}

/// Pure core of [`confined_bench_caveats`]: the workspace fence plus explicit
/// extra write roots, with no environment access (so it is deterministic and
/// parallel-safe to test).
fn confined_bench_caveats_with_grants(
    workspace: &str,
    scratch_roots: &[String],
    extra_write_roots: &[String],
) -> Caveats {
    // The workspace, the resolved scratch root(s) passed in, and explicit grants.
    // `$HOME`, `/root` and the system roots are OPT-IN via `NEWT_WRITE_PATHS`: a
    // confined `run_command` once rewrote the operator's `~/.rustup` because
    // `/home` was a built-in root. Disposable bench containers that need package
    // installs ask for the broad roots.
    let mut write_roots: Vec<String> = vec![workspace.to_string()];
    write_roots.extend(scratch_roots.iter().cloned());
    write_roots.extend(extra_write_roots.iter().cloned());
    // Built axis-by-axis rather than narrowing `Caveats::top()`: a headless
    // dispatch path must not even MENTION `Caveats::top()` (the `#94` no-top-leak
    // guard, `newt-acp-worker/tests/no_top_leak.rs`). Reads/exec/net and the call
    // budget are the open top of their axes; ONLY fs_write is fenced.
    Caveats {
        fs_read: Scope::All,
        fs_write: Scope::only(write_roots),
        exec: Scope::All,
        net: Scope::All,
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

/// The union/per-repo/unprobed shape shared by [`nested_delta_by_repo`] and
/// [`nested_current_by_repo`] — one named struct instead of a 3-element
/// tuple (`clippy::type_complexity`).
struct NestedByRepo {
    files: Vec<String>,
    by_repo: Vec<(String, Vec<String>)>,
    unprobed: Vec<String>,
}

/// `(files, by_repo)` for a nested-repo DELTA, shared by [`files_changed_delta`]'s
/// `NotARepo` and `InsideRepo` branches — the ONE place that turns
/// `nested_files_changed_between` plus a per-repo diff into the `by_repo`
/// breakdown, so the two branches cannot drift in how they compute it.
fn nested_delta_by_repo(
    before: &[newt_core::agentic::NestedRepoSnapshot],
    after: &[newt_core::agentic::NestedRepoSnapshot],
) -> NestedByRepo {
    let (files, unprobed) = newt_core::agentic::nested_files_changed_between(before, after);
    let by_repo = after
        .iter()
        .map(|repo| {
            let before_status = before
                .iter()
                .find(|b| b.repo == repo.repo)
                .and_then(|b| b.status.as_ref());
            let files = match (before_status, repo.status.as_ref()) {
                (Some(b), Some(a)) => newt_core::agentic::files_changed_between(b, a),
                _ => Vec::new(),
            };
            (repo.repo.clone(), files)
        })
        .collect();
    NestedByRepo {
        files,
        by_repo,
        unprobed,
    }
}

/// `(files, by_repo)` for a nested-repo CURRENT set (not a delta), shared by
/// [`uncommitted_repo_file_list`]'s `NotARepo` and `InsideRepo` branches.
fn nested_current_by_repo(snapshots: &[newt_core::agentic::NestedRepoSnapshot]) -> NestedByRepo {
    let (files, unprobed) = newt_core::agentic::nested_current_paths(snapshots);
    let by_repo = snapshots
        .iter()
        .map(|repo| {
            let files = repo
                .status
                .as_ref()
                .map(|s| s.keys().cloned().collect())
                .unwrap_or_default();
            (repo.repo.clone(), files)
        })
        .collect();
    NestedByRepo {
        files,
        by_repo,
        unprobed,
    }
}

/// Multi-repo recon PR2 / #2552 round 3: the hand-back's `files_changed`
/// input — the workspace root's own status DELTA when it IS a repo toplevel
/// (unchanged), the SUBTREE-scoped delta (workspace-relative, UNION'd with
/// any nested repos the subdirectory itself contains) when it is a
/// subdirectory of a larger repo, or the union of nested-repo deltas alone
/// when it is not a repo at all. `nested_before` empty/`None` at either
/// non-root case, with no subtree edit either, stays `"unavailable"`,
/// matching the `None`-means-nothing-to-report convention rather than
/// fabricating an empty list.
fn files_changed_delta(
    status_before: &Option<newt_core::agentic::StatusSnapshot>,
    subtree_before: &Option<newt_core::agentic::StatusSnapshot>,
    repo_location: &newt_core::agentic::WorkspaceRepoLocation,
    nested_before: &Option<Vec<newt_core::agentic::NestedRepoSnapshot>>,
    workspace: &str,
    read_scope: &newt_core::Scope<String>,
) -> Option<headless_contract::RepoFileList> {
    if let Some(before) = status_before {
        let after = newt_core::agentic::snapshot_workspace(workspace, read_scope)?;
        return Some(headless_contract::RepoFileList::Root(
            newt_core::agentic::files_changed_between(before, &after),
        ));
    }
    if let newt_core::agentic::WorkspaceRepoLocation::InsideRepo { prefix } = repo_location {
        let before = subtree_before.as_ref()?;
        let after = newt_core::agentic::snapshot_workspace_subtree(workspace, prefix, read_scope)?;
        let mut files = newt_core::agentic::files_changed_between(before, &after);
        let (by_repo, unprobed) = match nested_before.as_ref() {
            Some(nb) if !nb.is_empty() => {
                let after_nested = newt_core::agentic::snapshot_nested_repos(workspace, read_scope);
                let nested = nested_delta_by_repo(nb, &after_nested);
                files.extend(nested.files);
                (nested.by_repo, nested.unprobed)
            }
            _ => (Vec::new(), Vec::new()),
        };
        files.sort();
        return Some(headless_contract::RepoFileList::Subtree {
            files,
            by_repo,
            unprobed,
        });
    }
    let before = nested_before.as_ref()?;
    if before.is_empty() {
        return None;
    }
    let after = newt_core::agentic::snapshot_nested_repos(workspace, read_scope);
    let nested = nested_delta_by_repo(before, &after);
    Some(headless_contract::RepoFileList::Nested {
        files: nested.files,
        by_repo: nested.by_repo,
        unprobed: nested.unprobed,
    })
}

/// Multi-repo recon PR2 / #2552 round 3: the hand-back's `uncommitted_files`
/// input — the workspace root's CURRENT dirty set (via the embedded git
/// engine, unchanged) at a repo toplevel, the SUBTREE-scoped current set
/// (UNION'd with any nested repos the subdirectory itself contains) at a
/// subdirectory of a larger repo, or the union of each nested repo's own
/// current dirty set alone when there is no repo at all.
fn uncommitted_repo_file_list(
    uncommitted_at_exit: Option<Vec<String>>,
    repo_location: &newt_core::agentic::WorkspaceRepoLocation,
    nested_before: &Option<Vec<newt_core::agentic::NestedRepoSnapshot>>,
    workspace: &str,
    read_scope: &newt_core::Scope<String>,
) -> Option<headless_contract::RepoFileList> {
    match repo_location {
        newt_core::agentic::WorkspaceRepoLocation::OwnRoot => {
            uncommitted_at_exit.map(headless_contract::RepoFileList::Root)
        }
        newt_core::agentic::WorkspaceRepoLocation::InsideRepo { prefix } => {
            let subtree =
                newt_core::agentic::snapshot_workspace_subtree(workspace, prefix, read_scope)?;
            let mut files: Vec<String> = subtree.into_keys().collect();
            let (by_repo, unprobed) = match nested_before.as_ref() {
                Some(nb) if !nb.is_empty() => {
                    let snapshots =
                        newt_core::agentic::snapshot_nested_repos(workspace, read_scope);
                    let nested = nested_current_by_repo(&snapshots);
                    files.extend(nested.files);
                    (nested.by_repo, nested.unprobed)
                }
                _ => (Vec::new(), Vec::new()),
            };
            files.sort();
            Some(headless_contract::RepoFileList::Subtree {
                files,
                by_repo,
                unprobed,
            })
        }
        newt_core::agentic::WorkspaceRepoLocation::NotARepo => {
            let before = nested_before.as_ref()?;
            if before.is_empty() {
                return None;
            }
            let snapshots = newt_core::agentic::snapshot_nested_repos(workspace, read_scope);
            let nested = nested_current_by_repo(&snapshots);
            Some(headless_contract::RepoFileList::Nested {
                files: nested.files,
                by_repo: nested.by_repo,
                unprobed: nested.unprobed,
            })
        }
    }
}

/// #2537: the workspace's CURRENT staged+unstaged+untracked paths, sorted
/// and deduplicated — the hand-back's "uncommitted at exit" list. `None`
/// only when `engine.status` itself fails (the caller already turns "no
/// repo at all" into `None` by `GitEngine::open` failing first).
fn uncommitted_paths(
    engine: &newt_git::GitEngine,
    caveats: &newt_core::git_caveats::GitCaveats,
) -> Option<Vec<String>> {
    let s = engine.status(caveats).ok()?;
    let mut paths: Vec<String> = s
        .staged
        .iter()
        .chain(s.unstaged.iter())
        .map(|f| f.path.clone())
        .chain(s.untracked)
        .collect();
    paths.sort();
    paths.dedup();
    Some(paths)
}

/// #2537: commit id(s) created since `before_oid`, on whatever ref is
/// currently checked out — works the same under a detached HEAD, since
/// [`newt_git::GitEngine::head_snapshot`] reads it either way. `Some(vec![])`
/// when HEAD did not move (nothing to commit); `None` when either endpoint
/// could not be resolved (no prior HEAD to diff from — an unborn repo, or
/// the engine could not be opened at all).
fn commits_since(
    engine: &newt_git::GitEngine,
    caveats: &newt_core::git_caveats::GitCaveats,
    before_oid: Option<&str>,
) -> Option<Vec<String>> {
    let before_oid = before_oid?;
    let after_oid = engine.head_snapshot(caveats).ok()?.head?;
    if after_oid == before_oid {
        return Some(Vec::new());
    }
    engine
        .log(
            caveats,
            headless_contract::HANDBACK_MAX_FILES,
            Some(&format!("{before_oid}..{after_oid}")),
            &[],
        )
        .ok()
        .map(|commits| commits.into_iter().map(|c| c.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::config::BackendConfig;

    /// #2552 round 3: `commits_since` — the exact function `handback`'s
    /// `commits` field is built from — is reached whenever `workspace` is
    /// inside SOME repo (`WorkspaceRepoLocation::InsideRepo`, not just
    /// `OwnRoot`), and correctly names a commit made from the subdirectory.
    ///
    /// Grounds a real limitation found while writing this test: an
    /// end-to-end test through the FULL `newt headless` binary, with the
    /// MODEL making the commit via a tool call, is not currently
    /// constructible — `newt_core::agentic::driver::TurnDriver` hardcodes
    /// `git_tool: None` (no builder wires a `GitTool` impl for any
    /// `TurnDriver`-based run, headless included), and a shell `git commit`
    /// via `run_command` is deliberately refused (`tools.rs`: "refusing to
    /// create a git commit via the shell — that bypasses harness-managed
    /// commit attribution"). So no headless run — subdirectory or repo
    /// root alike — can populate `commits` from a MODEL action today; this
    /// is orthogonal to and predates #2552. This test instead grounds
    /// `commits_since` itself against a REAL git repo and the REAL embedded
    /// `newt_git::GitEngine` (never mocked), the same call
    /// `uncommitted_delta`'s exit-time block makes — proving the mechanism
    /// this PR widened to the `InsideRepo` case is correct, at the level
    /// that is actually testable.
    #[test]
    fn commits_since_names_a_commit_made_from_a_repo_subdirectory() {
        let repo = tempfile::tempdir().expect("repo root");
        let git = |dir: &std::path::Path, args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git")
                .status
                .success());
        };
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.email", "t@example.com"]);
        git(repo.path(), &["config", "user.name", "t"]);
        let crate_dir = repo.path().join("crate");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(crate_dir.join("lib.rs"), "one\n").unwrap();
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "init"]);

        let read_scope = newt_core::Scope::All;
        let crate_str = crate_dir.to_string_lossy().into_owned();
        assert!(
            matches!(
                newt_core::agentic::locate_workspace_repo(&crate_str, &read_scope),
                newt_core::agentic::WorkspaceRepoLocation::InsideRepo { .. }
            ),
            "crate/ must resolve as a SUBDIRECTORY of the enclosing repo"
        );

        let engine = newt_git::GitEngine::open(&crate_dir, &read_scope)
            .expect("the engine must open for a repo subdirectory too (#2552 blocker 1)");
        let caveats = newt_core::git_caveats::GitCaveats::read_only();
        let before = engine
            .head_snapshot(&caveats)
            .expect("head_snapshot must succeed");

        std::fs::write(crate_dir.join("lib.rs"), "one\ntwo\n").unwrap();
        git(&crate_dir, &["add", "-A"]);
        git(&crate_dir, &["commit", "-q", "-m", "during the run"]);
        let after_head = git_rev_parse_head(repo.path());

        let commits = commits_since(&engine, &caveats, before.head.as_deref())
            .expect("commits_since must resolve, not unavailable");
        assert_eq!(
            commits,
            vec![after_head],
            "the commit made from crate/ must be named"
        );
    }

    #[cfg(test)]
    fn git_rev_parse_head(dir: &std::path::Path) -> String {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .expect("git rev-parse HEAD");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// #2314: the seed id is of the entries, not the file's spelling, and a
    /// seed the store could not faithfully hold is refused before any run.
    #[test]
    fn a_scratchpad_seed_is_identified_by_its_entries_and_validated() {
        use newt_core::ScratchpadStore;
        let (store, seed) = seed_scratchpad(r#"{"b": "2", "a": "1"}"#).unwrap();
        assert_eq!(store.get("a").as_deref(), Some("1"));
        let (_, respelled) = seed_scratchpad("{\"a\":\"1\",\n \"b\":\"2\"}").unwrap();
        assert_eq!(seed, respelled, "same entries, same seed");
        let (_, other) = seed_scratchpad(r#"{"a": "1"}"#).unwrap();
        assert_ne!(seed, other, "different entries, different seed");
        assert!(seed_scratchpad(r#"{"a": 1}"#).is_err(), "non-string value");
        assert!(seed_scratchpad(r#"["a"]"#).is_err(), "not an object");
        assert!(seed_scratchpad(r#"{" ": "v"}"#).is_err(), "blank key");
    }

    /// F16 (red first): the seeded message names the real root and is a
    /// `system` message, so it precedes the task in the wire transcript
    /// exactly like newt-tui's `Workspace: {path}` line.
    #[test]
    fn workspace_root_system_message_names_the_real_root() {
        let msg = workspace_root_system_message("/home/dev/proj");
        assert_eq!(msg.role, newt_core::memory::Role::System);
        assert!(
            msg.content.contains("/home/dev/proj"),
            "message must name the real workspace root, got: {}",
            msg.content
        );
    }

    #[test]
    fn smart_runs_are_resumable_unless_hermetic_was_explicit() {
        let legacy = newt_launch::LaunchConfig::default();
        assert!(!smart_launch(&legacy, false, false)
            .continuity
            .is_resumable());
        assert!(smart_launch(&legacy, true, false).continuity.is_resumable());
        assert!(!smart_launch(&legacy, true, true).continuity.is_resumable());
        let resume = newt_launch::LaunchConfig {
            continuity: newt_launch::Continuity::Resume {
                from: Some("head".into()),
            },
        };
        assert_eq!(
            smart_launch(&resume, true, false).continuity.parent_frame(),
            Some("head")
        );
    }

    fn backend(name: &str, endpoint: &str) -> BackendConfig {
        BackendConfig {
            name: name.into(),
            endpoint: endpoint.into(),
            ..Default::default()
        }
    }

    /// Parity (#1819 finding 4): serving=None + a known effective model is
    /// `SelectedModel` — the same principal chat derives — so an exact
    /// bound card activates capabilities AND typed family identically in
    /// both lanes; a mismatched model stays typed-inactive with no family
    /// (the control proving no false activation).
    #[test]
    fn headless_principal_parity_for_unset_serving() {
        use newt_core::model_card::{
            CardApplicability, CardBindingSeed, ResolvedCapabilities, ServingPrincipal,
        };
        assert!(matches!(
            headless_principal(None, "bound-model"),
            ServingPrincipal::SelectedModel("bound-model")
        ));
        assert!(matches!(
            headless_principal(Some(newt_core::Serving::Instance), "m"),
            ServingPrincipal::Instance
        ));
        // Behavioral: an exact card on a serving-less backend activates.
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = dir.path().join("profile.toml");
        std::fs::write(&profile, "# profile\n").expect("write profile");
        std::fs::create_dir_all(dir.path().join("models")).expect("models dir");
        std::fs::write(
            dir.path().join("models/team.toml"),
            "name = \"team\"\nbackend = \"vllm\"\nfamily = \"nemotron\"\n\n[vllm]\nserved_name = \"team\"\n\n[capability]\nemits_leading_reasoning = true\n",
        )
        .expect("write card");
        let b = newt_core::BackendConfig {
            name: "carded".into(),
            endpoint: "http://local:8000".into(),
            model: Some("bound-model".into()),
            card: Some("team".into()),
            ..Default::default()
        };
        // The seed constructed LITERALLY (the receipt supplies it in
        // production; the banned from_backend re-derivation stays banned).
        let seed = CardBindingSeed {
            card: b.card.clone(),
            bound_model: b.model.clone(),
            bound_destination: newt_core::BackendDestination::of(&b),
        };
        let caps = ResolvedCapabilities::resolve(&b, &seed, Some(&profile)).expect("resolves");
        let destination = newt_core::BackendDestination::of(&b);
        // Exact effective model: capabilities + family both engage.
        let d = caps.for_route(&destination, headless_principal(None, "bound-model"));
        assert!(d.emits_leading_reasoning(), "exact association activates");
        assert_eq!(
            caps.family_for_route(&destination, headless_principal(None, "bound-model")),
            Some("nemotron")
        );
        // Retarget control: a different effective model must NOT activate.
        let d = caps.for_route(&destination, headless_principal(None, "other-model"));
        assert!(!d.emits_leading_reasoning(), "no false activation");
        assert!(matches!(
            d.applicability(),
            CardApplicability::InactiveModel { .. }
        ));
        assert_eq!(
            caps.family_for_route(&destination, headless_principal(None, "other-model")),
            None,
            "no family on a retargeted principal"
        );
    }

    #[test]
    fn pick_backend_skips_endpointless_and_takes_the_first_usable() {
        // Deterministic: no NEWT_PROVIDER selection path. The guard's shared
        // lock serializes every NEWT_PROVIDER-touching test in the workspace
        // and restores the prior value on drop.
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        // SAFETY: guard held; restored on drop.
        unsafe { std::env::remove_var("NEWT_PROVIDER") };
        let cfg = Config {
            backends: vec![
                backend("embedded-no-endpoint", ""),
                backend("dgx", "http://router:8080"),
                backend("other", "http://other:9000"),
            ],
            ..Default::default()
        };
        let cfg = newt_core::ResolvedConfig::unrequested(cfg);
        let chosen = pick_backend(&cfg).expect("a usable backend exists");
        assert_eq!(
            chosen.backend.name, "dgx",
            "skip the endpointless one, take the first usable"
        );
    }

    #[test]
    fn pick_backend_errors_when_no_endpoints() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        // SAFETY: guard held; restored on drop.
        unsafe { std::env::remove_var("NEWT_PROVIDER") };
        let cfg = Config {
            backends: vec![backend("a", ""), backend("b", "")],
            ..Default::default()
        };
        let cfg = newt_core::ResolvedConfig::unrequested(cfg);
        let err = pick_backend(&cfg).expect_err("nothing routable");
        assert!(err.to_string().contains("no usable backend"), "{err}");
    }

    /// W0 (#1511): digest resolution is flag > env twin, blank falls through,
    /// and NOTHING is ever derived — no value in ⇒ no digest out.
    #[test]
    fn resolve_model_digest_flag_beats_env_and_never_invents() {
        assert_eq!(
            resolve_model_digest(Some("sha-flag"), Some("sha-env")).as_deref(),
            Some("sha-flag")
        );
        assert_eq!(
            resolve_model_digest(None, Some(" sha-env ")).as_deref(),
            Some("sha-env"),
            "env twin used when no flag; whitespace trimmed"
        );
        assert_eq!(
            resolve_model_digest(Some("  "), Some("sha-env")).as_deref(),
            Some("sha-env"),
            "a blank flag falls through to the env twin"
        );
        assert_eq!(resolve_model_digest(None, None), None);
        assert_eq!(resolve_model_digest(Some(""), Some("")), None);
    }

    #[test]
    fn pick_backend_honors_default_backend_over_first_endpoint() {
        // #1320: `--config X` sets `default_backend`; headless must route to it, not to
        // the first endpoint-bearing entry (the coincidence that masked the bug).
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        // SAFETY: guard held; restored on drop.
        unsafe { std::env::remove_var("NEWT_PROVIDER") };
        let cfg = Config {
            default_backend: Some("sol".to_string()),
            backends: vec![
                backend("other", "https://other:9000"), // first + endpoint-bearing
                backend("sol", "https://api.openai.com"),
            ],
            ..Default::default()
        };
        let cfg = newt_core::ResolvedConfig::unrequested(cfg);
        assert_eq!(
            pick_backend(&cfg).expect("a backend").backend.name,
            "sol",
            "default_backend wins over the first endpoint-bearing entry"
        );
    }

    #[test]
    fn pick_backend_default_backend_unroutable_is_a_hard_error_not_a_fallback() {
        // A `default_backend` naming a destination-less entry used to be
        // silently skipped for the first usable backend — running something
        // the operator did not select. It is now the typed UnroutableNamed
        // error: fix the backend or the selector.
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        // SAFETY: guard held; restored on drop.
        unsafe { std::env::remove_var("NEWT_PROVIDER") };
        let cfg = Config {
            default_backend: Some("ghost".to_string()),
            backends: vec![backend("ghost", ""), backend("real", "http://r:1")],
            ..Default::default()
        };
        let cfg = newt_core::ResolvedConfig::unrequested(cfg);
        let err = pick_backend(&cfg).expect_err("no silent fallback to `real`");
        assert!(
            err.to_string().contains("ghost") && err.to_string().contains("destination"),
            "{err}"
        );
    }

    #[test]
    fn pick_backend_unknown_env_name_is_a_hard_error_not_a_fallback() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        // SAFETY: guard held; restored on drop.
        unsafe { std::env::set_var("NEWT_PROVIDER", "no-such-backend") };
        let cfg = Config {
            backends: vec![backend("real", "http://r:1")],
            ..Default::default()
        };
        let cfg = newt_core::ResolvedConfig::unrequested(cfg);
        let err = pick_backend(&cfg).expect_err("no silent fallback to `real`");
        assert!(err.to_string().contains("no-such-backend"), "{err}");
    }

    #[test]
    fn pick_backend_provider_named_env_is_a_hard_error_not_a_fallback() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        // SAFETY: guard held; restored on drop.
        unsafe { std::env::set_var("NEWT_PROVIDER", "acme") };
        let cfg = Config {
            backends: vec![backend("real", "http://r:1")],
            providers: vec![newt_core::config::ProviderConfig {
                name: "acme".into(),
                command: "newt-provider-openai".into(),
                model: None,
                env_pass: vec![],
                tiers: vec![],
            }],
            ..Default::default()
        };
        let cfg = newt_core::ResolvedConfig::unrequested(cfg);
        let err = pick_backend(&cfg).expect_err("headless cannot drive a provider");
        assert!(err.to_string().contains("provider `acme`"), "{err}");
    }

    /// G (#1819): the shared contract treats embedded (model_path) backends
    /// as routable, but headless only drives HTTP — an embedded selection must
    /// be a typed refusal, never an empty-endpoint fall into the Ollama
    /// loop. Covered for the sole, default-named, and env-named selections.
    #[test]
    fn pick_backend_rejects_embedded_model_path_backends() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let embedded = || newt_core::BackendConfig {
            name: "emb".into(),
            model_path: Some("/models/x.gguf".into()),
            kind: Some(newt_core::BackendKind::Embedded),
            ..Default::default()
        };
        // Sole.
        // SAFETY: guard held; restored on drop.
        unsafe { std::env::remove_var("NEWT_PROVIDER") };
        let cfg = Config {
            backends: vec![embedded()],
            ..Default::default()
        };
        let cfg = newt_core::ResolvedConfig::unrequested(cfg);
        let err = pick_backend(&cfg).expect_err("sole embedded refused");
        assert!(err.to_string().contains("HTTP backends only"), "{err}");
        // default_backend names it.
        let cfg = Config {
            backends: vec![backend("http", "http://h:1"), embedded()],
            default_backend: Some("emb".into()),
            ..Default::default()
        };
        let cfg = newt_core::ResolvedConfig::unrequested(cfg);
        let err = pick_backend(&cfg).expect_err("default-named embedded refused");
        assert!(err.to_string().contains("emb"), "{err}");
        // $NEWT_PROVIDER names it.
        // SAFETY: guard held; restored on drop.
        unsafe { std::env::set_var("NEWT_PROVIDER", "emb") };
        let cfg = Config {
            backends: vec![backend("http", "http://h:1"), embedded()],
            ..Default::default()
        };
        let cfg = newt_core::ResolvedConfig::unrequested(cfg);
        let err = pick_backend(&cfg).expect_err("env-named embedded refused");
        assert!(err.to_string().contains("emb"), "{err}");
    }

    /// #1819: a `--profile` runs through the FALLIBLE backend assembly —
    /// duplicate or empty backend names are hard errors, not silently
    /// loaded configs.
    #[test]
    fn a_profile_with_invalid_backend_identity_is_a_hard_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dup = dir.path().join("dup.toml");
        std::fs::write(
            &dup,
            "[[backends]]\nname = \"twin\"\nendpoint = \"http://a:1\"\n\n\
             [[backends]]\nname = \"twin\"\nendpoint = \"http://b:2\"\n",
        )
        .expect("write profile");
        let err = resolve_profile(&dup).expect_err("duplicate names refuse");
        assert!(format!("{err:#}").contains("twin"), "{err:#}");
        let empty = dir.path().join("empty.toml");
        std::fs::write(
            &empty,
            "[[backends]]\nname = \"\"\nendpoint = \"http://a:1\"\n",
        )
        .expect("write profile");
        let err = resolve_profile(&empty).expect_err("empty names refuse");
        assert!(format!("{err:#}").contains("no name"), "{err:#}");
        // A valid profile resolves, receipts and all — no publish happened.
        let ok = dir.path().join("ok.toml");
        std::fs::write(
            &ok,
            "[[backends]]\nname = \"a\"\nendpoint = \"http://a:1\"\n",
        )
        .expect("write profile");
        let resolved = resolve_profile(&ok).expect("valid profile");
        assert_eq!(resolved.receipts().len(), 1);
    }

    #[test]
    fn headless_solve_applies_context_config_to_turn_driver() {
        let mut driver = TurnDriverConfig::new(
            "http://example.invalid",
            "model",
            BackendKind::Openai,
            "/workspace",
        );
        let context = newt_core::ContextConfig {
            estimation: newt_core::tokens::TokenEstimation::new(3),
            summary_input_cap_floor_chars: 4_096,
            input_ceiling_pct: 70,
            low_budget_pct: 20,
            compaction_trigger_policy: newt_core::CompactionTriggerPolicy::MessageCount,
            ..Default::default()
        };

        apply_context_config(&mut driver, Some(&context));

        assert_eq!(driver.estimation.chars_per_token, 3);
        assert_eq!(driver.summary_input_cap_floor_chars, 4_096);
        assert_eq!(driver.input_ceiling_pct, 70);
        assert_eq!(driver.low_budget_pct, 20);
        assert_eq!(
            driver.compaction_trigger_policy,
            newt_core::CompactionTriggerPolicy::MessageCount
        );
    }

    #[test]
    fn context_window_uses_configured_input_ceiling() {
        let mut driver = TurnDriverConfig::new(
            "http://example.invalid",
            "model",
            BackendKind::Openai,
            "/workspace",
        );
        driver.input_ceiling_pct = 83;

        apply_context_window(&mut driver, 65_536);

        assert_eq!(driver.num_ctx, Some(65_536));
        assert_eq!(driver.safe_context, Some(54_394));
        assert_eq!(driver.max_ok_input, Some(54_394));
    }

    #[test]
    fn contract_dials_record_the_effective_cli_overrides() {
        let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
        newt_core::initiative::set_initiative_config(Default::default());
        newt_core::initiative::set_cli_initiative(newt_core::Initiative::Decisive);
        newt_core::tenacity::set_cli_tenacity(newt_core::Tenacity::Relentless);

        let runtime = resolve_runtime_posture(&Config::default(), None);
        assert_eq!(runtime.initiative, newt_core::Initiative::Decisive);
        assert_eq!(runtime.tenacity, newt_core::Tenacity::Relentless);
    }

    #[test]
    fn explicit_relentless_tenacity_expands_solve_unless_max_rounds_wins() {
        use newt_core::tenacity::RELENTLESS_TOOL_ROUND_TARGET;
        use newt_core::Tenacity;

        assert_eq!(
            headless_tool_round_limit(40, Some(Tenacity::Relentless), None),
            RELENTLESS_TOOL_ROUND_TARGET
        );
        assert_eq!(
            headless_tool_round_limit(40, Some(Tenacity::Relentless), Some(2)),
            2
        );
        assert_eq!(
            headless_tool_round_limit(40, None, None),
            40,
            "automatic family tenacity must not silently expand the safety cap"
        );
    }

    #[test]
    fn contract_cognition_is_the_level_the_backend_can_receive() {
        let level = Some(Cognition::Meticulous);
        let capable = ChatCompletionsCapability {
            cognition: Some(true),
            ..Default::default()
        };

        assert_eq!(
            projected_cognition(
                level,
                BackendKind::Openai,
                OpenAiApi::Responses,
                Default::default()
            ),
            level,
            "Responses always has a cognition projection"
        );
        assert_eq!(
            projected_cognition(
                level,
                BackendKind::Openai,
                OpenAiApi::ChatCompletions,
                capable,
            ),
            level,
            "an explicitly capable Chat endpoint receives the local policy"
        );
        assert_eq!(
            projected_cognition(
                level,
                BackendKind::Openai,
                OpenAiApi::ChatCompletions,
                Default::default(),
            ),
            None,
            "an unknown Chat endpoint receives no cognition fields"
        );
        assert_eq!(
            projected_cognition(level, BackendKind::Ollama, OpenAiApi::Responses, capable,),
            None,
            "the Ollama wire has no cognition projection"
        );
    }

    #[test]
    fn resolve_lane_precedence_and_trim() {
        use HeadlessLane::*;
        // The 3rd arg is now the EXPLICIT `--unsafe-host-exec` opt-in.
        // --confined flag wins regardless of the unsafe request.
        assert_eq!(resolve_lane(true, None, true), Confined);
        assert_eq!(resolve_lane(true, Some("off"), false), Confined);
        // NEWT_BENCH_OCAP=on (trimmed, case-insensitive) selects the confined
        // lane — even over an unsafe request (confinement is never dropped).
        assert_eq!(resolve_lane(false, Some("on"), true), Confined);
        assert_eq!(resolve_lane(false, Some(" ON "), true), Confined);
        assert_eq!(resolve_lane(false, Some("On"), false), Confined);
        // The explicit unsafe opt-in is the SOLE route to Yolo.
        assert_eq!(resolve_lane(false, Some("1"), true), Yolo);
        assert_eq!(resolve_lane(false, None, true), Yolo);
        // A non-`on` env value alone neither confines nor goes unsafe → default
        // Confined (OCAP on).
        assert_eq!(resolve_lane(false, Some("1"), false), Confined);
        assert_eq!(resolve_lane(false, Some("true"), false), Confined);
    }

    /// P3 (`noninteractive-launch-policy`) regression: `--non-interactive` must
    /// change interaction only, never authority. Modelled at the lane-resolution
    /// choke: with no explicit unsafe/confined signal (a plain
    /// `newt headless --non-interactive`), the lane is Confined (OCAP ON) — NEVER
    /// the OCAP-off Yolo lane. Only an INDEPENDENT explicit `--unsafe-host-exec`
    /// reaches Yolo. Before P3, `resolve_lane(false, None, /*non_interactive*/ true)`
    /// returned Yolo — the bug this closes.
    #[test]
    fn non_interactive_never_relaxes_authority() {
        assert_eq!(resolve_lane(false, None, false), HeadlessLane::Confined);
        assert_eq!(
            resolve_lane(false, Some("off"), false),
            HeadlessLane::Confined
        );
        // Independent explicit opt-in is required for host access.
        assert_eq!(resolve_lane(false, None, true), HeadlessLane::Yolo);
    }

    use newt_core::caveats::CaveatsExt;

    /// The load-bearing invariant of the OCAP-on lane: writes are FENCED, never
    /// unrestricted. A `Scope::All` fs_write would (a) drop us back to the
    /// unconfined `--yolo` behaviour and (b) route through the y/N-gated
    /// `confirm_unrestricted_fs_mutation` path (which, with `permission_gate:
    /// None`, silently DENIES) — the exact WS1 trap the lane exists to avoid.
    #[test]
    fn confined_caveats_fence_writes_never_all() {
        let cv =
            confined_bench_caveats_with_grants("/app/task", &["/srv/scratch".to_string()], &[]);
        assert!(
            !matches!(cv.fs_write, Scope::All),
            "confined lane must NEVER grant fs_write = Scope::All"
        );
        assert!(
            matches!(cv.fs_write, Scope::Only(_)),
            "confined fs_write must be an explicit Scope::Only fence"
        );
        // Reads / exec / net stay wide so the bench isn't crippled — parity
        // isolates the enforcement PATH, not fence tightness.
        assert!(matches!(cv.fs_read, Scope::All), "reads stay open");
        assert!(matches!(cv.exec, Scope::All), "exec stays open");
        assert!(matches!(cv.net, Scope::All), "net stays open");
    }

    #[test]
    fn confined_caveats_permit_workspace_and_scratch_writes() {
        let cv =
            confined_bench_caveats_with_grants("/app/task", &["/srv/scratch".to_string()], &[]);
        // The workspace root and the standard mutable roots are writable
        // (exact-match here; the enforcement site adds prefix coverage).
        assert!(cv.permits_fs_write("/app/task"), "workspace root writable");
        // Changed ON PURPOSE (was: `/tmp` writable as a literal). The scratch root
        // now comes in as a parameter, resolved from configuration by the caller.
        assert!(
            cv.permits_fs_write("/srv/scratch"),
            "configured scratch writable"
        );
        assert!(
            !cv.permits_fs_write("/tmp"),
            "/tmp is no longer a built-in root"
        );
        // Changed ON PURPOSE (was: `/usr` writable). The default lane is a default on a
        // developer host, so system roots are opt-in via NEWT_WRITE_PATHS.
        assert!(!cv.permits_fs_write("/usr"), "system roots are opt-in");
        // An un-granted absolute path outside every root is NOT writable.
        assert!(
            !cv.permits_fs_write("/boot/vmlinuz"),
            "a path outside every granted root fails closed"
        );
    }

    /// The default lane's fence must not contain `$HOME`, `/root` or system
    /// roots: a confined `run_command` (`rustup default nightly`) rewrote the
    /// operator's `~/.rustup` because `/home` was a built-in write root.
    #[test]
    fn confined_fence_excludes_home_and_system_roots_by_default() {
        use newt_core::caveats::permits_path;
        let cv =
            confined_bench_caveats_with_grants("/app/task", &["/srv/scratch".to_string()], &[]);
        for p in [
            "/home/u/.rustup/settings.toml",
            "/home/u/.ssh/id",
            "/root/x",
            "/usr/bin/x",
            "/etc/passwd",
        ] {
            assert!(!permits_path(&cv.fs_write, p), "{p} must not be writable");
        }
        for p in ["/app/task/src/x", "/srv/scratch/x"] {
            assert!(permits_path(&cv.fs_write, p), "{p} must be writable");
        }
    }

    #[test]
    fn confined_fence_broad_roots_are_opt_in_by_grant() {
        use newt_core::caveats::permits_path;
        let grants = ["/data/scratch".to_string(), "/home".to_string()];
        let cv =
            confined_bench_caveats_with_grants("/app/task", &["/srv/scratch".to_string()], &grants);
        assert!(permits_path(&cv.fs_write, "/data/scratch/f"));
        assert!(permits_path(&cv.fs_write, "/home/u/x"));
    }

    /// The smart-harness lane narrows the default fence, never widens it: its
    /// isolation forbids a model-writable ancestor (`/tmp`), so it cannot be
    /// EQUAL, but every root it grants must already be inside the default's.
    #[test]
    fn smart_fence_is_within_the_default_fence() {
        use newt_core::caveats::permits_path;
        let default =
            confined_bench_caveats_with_grants("/app/task", &["/srv/scratch".to_string()], &[]);
        let smart = newt_core::confined_exec::build_tool_caveats("/app/task".as_ref());
        let Scope::Only(roots) = &smart.fs_write else {
            panic!("smart fs_write must be an explicit Scope::Only");
        };
        for root in roots {
            assert!(
                permits_path(&default.fs_write, root),
                "{root} outside default"
            );
        }
    }

    /// Three Cs (configuration over a hardcoded constant): the fence's scratch
    /// root is the CONFIGURED scratch dir when it is absolute (an operator moved
    /// it), else the platform temp dir (which honours `TMPDIR`). `/tmp` is never
    /// a literal: not every distro uses it the way we think.
    #[test]
    fn fence_scratch_root_comes_from_configuration_then_temp_dir() {
        use newt_core::caveats::permits_path;
        // `fence_scratch_roots` asks the HOST whether a dir is absolute, so the
        // "configured absolute dir" and the fallback must be absolute ON THE
        // RUNNING HOST: `/srv/scratch` has no drive on Windows and would be
        // "relative" there. Built from the platform temp dir, they are absolute
        // everywhere; only the fence paths are compared as component paths.
        let base = std::env::temp_dir();
        let configured = base.join("newt-scratch-test");
        let temp = base.join("newt-fallback-test");
        let (configured_s, temp_s) = (
            configured.to_string_lossy().into_owned(),
            temp.to_string_lossy().into_owned(),
        );
        let writable = |cv: &Caveats, dir: &std::path::Path| {
            permits_path(&cv.fs_write, &dir.join("x").to_string_lossy())
        };
        // Configured absolute dir wins, and the temp dir is NOT also granted.
        let roots = fence_scratch_roots(&configured_s, &temp);
        assert_eq!(roots, std::slice::from_ref(&configured_s));
        let cv = confined_bench_caveats_with_grants("/app/task", &roots, &[]);
        assert!(writable(&cv, &configured));
        assert!(!writable(&cv, &temp));
        assert!(!writable(&cv, &base), "the temp dir itself is not granted");
        // Unset (the relative `.scratch` default lives in the workspace already):
        // fall back to the platform temp dir, not a literal `/tmp`.
        let roots = fence_scratch_roots(".scratch", &temp);
        assert_eq!(roots, [temp_s]);
        let cv = confined_bench_caveats_with_grants("/app/task", &roots, &[]);
        assert!(writable(&cv, &temp));
        assert!(!writable(&cv, &configured));
        // No scratch root at all: workspace + explicit grants only.
        let cv = confined_bench_caveats_with_grants("/app/task", &[], &[]);
        assert!(!writable(&cv, &temp));
        assert!(permits_path(&cv.fs_write, "/app/task/x"));
    }

    #[test]
    fn confined_caveats_honor_write_paths_grant() {
        let cv = confined_bench_caveats_with_grants(
            "/app/task",
            &["/srv/scratch".to_string()],
            &["/data/scratch".to_string()],
        );
        assert!(
            cv.permits_fs_write("/data/scratch"),
            "a per-task NEWT_WRITE_PATHS grant joins the fence"
        );
    }

    /// RED-FIRST evidence for #2524 item 1's verify-first claim (a): before
    /// this PR, nothing in `headless.rs` read `ocap_store` at all (grep finds
    /// no `ocap_store` reference outside this test) — a signed `[[fs]]` read
    /// grant for a path outside the workspace could not reach a confined
    /// headless `fs_read`/`fs_write` even in principle. This test pins the
    /// fix: `fold_ocap_grants` widens a fenced axis with a verified store's
    /// approve entries and NEVER touches an axis already `Scope::All`.
    #[test]
    fn fold_ocap_grants_widens_a_fenced_axis_but_never_touches_an_open_one() {
        use newt_core::ocap_store::{build_store, Verdict};
        let (store, warnings) = build_store(&[(
            Verdict::Approve,
            Some("[[fs]]\npath = \"/opt/canvas-token\"\n".to_string()),
        )]);
        assert!(warnings.is_empty(), "{warnings:?}");
        // Confined default lane: fs_read is already Scope::All (open) — must
        // stay untouched (folding must never even mention widening an open
        // axis, matching the `#94` no-top-leak posture).
        let mut open = confined_bench_caveats_with_grants("/app/task", &[], &[]);
        assert_eq!(open.fs_read, Scope::All);
        let admitted = fold_ocap_grants(&mut open, &store);
        assert_eq!(open.fs_read, Scope::All, "an open axis must stay open");
        // #2532 review, item 3: the axis was already open, so this changed
        // nothing — still listed (store provenance stays visible), labeled a
        // no-op rather than implying the signature widened anything.
        assert_eq!(
            admitted,
            vec!["fs_read:/opt/canvas-token (no-op: already permitted)".to_string()]
        );
        // Smart-lane fenced axis: the grant must actually widen it — this is
        // the canvas gap the brief names (an MCP child's token file, outside
        // the workspace).
        let mut fenced =
            newt_core::confined_exec::build_tool_caveats(std::path::Path::new("/app/task"));
        assert!(!fenced.permits_fs_read("/opt/canvas-token"));
        fold_ocap_grants(&mut fenced, &store);
        assert!(fenced.permits_fs_read("/opt/canvas-token"));
        // Read-only entry: fs_write must NOT gain the grant.
        assert!(!fenced.permits_fs_write("/opt/canvas-token"));
    }

    /// #2532 review, should-fix 2, RED FIRST: `load_store` verifies only the
    /// SIGNATURE (`verify_approves`), never danger — `sign_ocap` refuses a
    /// High-danger target today, but a `PolicySet` reaching `fold_ocap_grants`
    /// from any other path (an older build, a script holding the key) is not
    /// re-checked. Before the fix, a validly-signed `/` fs entry folded
    /// straight into headless caveats though the TUI's `recalled_grants`
    /// drops any `DangerTier::High` target (`permissions.rs`). This test
    /// forces exactly that shape past `sign_approves`'s own refusal (`|_, _|
    /// false` as the `is_high_danger` predicate, mirroring the disposable-key
    /// signing helper other tests in this module already use) and pins that
    /// `fold_ocap_grants` still refuses it.
    #[test]
    fn fold_ocap_grants_refuses_a_signed_high_danger_root() {
        use newt_core::ocap_store::PolicyFile;
        let key = newt_identity::UserKey::generate();
        let mut file = PolicyFile::parse("[[fs]]\npath = '/'\nwrite = true\n").expect("parse");
        let (signed, refused) = newt_core::ocap_store::sign_approves(
            &mut file,
            |_, _| false,
            |p| key.sign(p).to_bytes(),
        );
        assert_eq!(signed, 1, "the forged signature must still verify");
        assert!(refused.is_empty());
        let (store, warnings) = newt_core::ocap_store::build_store(&[(
            newt_core::ocap_store::Verdict::Approve,
            Some(file.to_toml().expect("serialize")),
        )]);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut caveats =
            newt_core::confined_exec::build_tool_caveats(std::path::Path::new("/app/task"));
        let admitted = fold_ocap_grants(&mut caveats, &store);
        assert!(
            admitted.is_empty(),
            "a High-danger root must never be admitted, signature or not: {admitted:?}"
        );
        assert!(!caveats.permits_fs_write("/etc/shadow"));
    }

    /// An unsigned/unverified `approve.toml` is never even IN the `PolicySet`
    /// this function is handed in production (`ocap_store::load_store`
    /// verifies before returning); `fold_ocap_grants` itself does no
    /// verification, so an empty (unverified-dropped) store folds nothing.
    #[test]
    fn fold_ocap_grants_of_an_empty_store_admits_nothing() {
        let store = newt_core::ocap_store::PolicySet::default();
        let mut caveats =
            newt_core::confined_exec::build_tool_caveats(std::path::Path::new("/app/task"));
        let admitted = fold_ocap_grants(&mut caveats, &store);
        assert!(admitted.is_empty());
        assert!(!caveats.permits_fs_read("/opt/canvas-token"));
    }
}
