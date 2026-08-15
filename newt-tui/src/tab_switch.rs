//! The staged tab switch and the one tab-action handler (#1669 PR-A).
//!
//! [`crate::tabs`] is the pure model; this is where a switch actually happens
//! against live session state. Split out because `chat.rs` is already large and
//! because the staging discipline below is the whole correctness argument —
//! it deserves to be readable in one screen rather than buried in `run_chat`.
//!
//! # The stages, and why the order is the contract
//!
//! Per the ADR's state-transition table:
//!
//! 1. **Stage 0 — fallible read, mutate nothing.** Load the incoming
//!    conversation first. If the row is missing or corrupt, the switch aborts
//!    with the outgoing tab still fully active and *nothing* touched. A switch
//!    that half-applied would leave the session holding one conversation's
//!    memory under another's posture.
//! 2. **Deactivate the outgoing tab** — flush its pin (so the row owns the
//!    operator's latest dials) and stash its sidecar + unsubmitted input.
//! 3. **Hydrate the incoming conversation** through the one trusted restore
//!    path every other conversation switch uses.
//! 4. **Baseline reset ⊕ incoming pin overlay.** Activation is
//!    *history-independent*: the result must not depend on which tab you came
//!    from, so every axis resets to the invocation baseline before the incoming
//!    pin is layered on. This is what distinguishes **activate** from
//!    **resume** — resume is a sparse overlay over live state, activation is
//!    not — and the ADR is explicit that the two verbs must not be merged.
//! 5. **Restore the incoming tab's sidecar + input stash.**
//! 6. **Hand off lifecycle ownership** so subsequent ambient events attribute
//!    to the newly active tab.
//!
//! Claims are held throughout: both the outgoing and incoming conversations
//! stay claimed across a switch. Only **close** and **exit** release.

use crate::tabs::{TabError, TabSet};

/// The live session state a switch reads and rewrites.
///
/// Flat rather than nested because the two existing context structs
/// (`ConversationCommandContext`, `ConversationPreferenceSwitch`) both want the
/// conversation id — one mutably, one by shared reference — so they cannot be
/// held at the same time. They are built here, in sequence, from these fields.
pub(crate) struct TabSwitchCtx<'a> {
    // ── conversation restore ──
    pub store: &'a newt_core::ConversationStore,
    pub persona_store: &'a crate::PersonaStore,
    pub workspace: &'a str,
    pub memory: &'a mut newt_core::MemoryManager,
    pub system: &'a mut String,
    pub active_persona: &'a mut Option<crate::Persona>,
    pub active_conversation_id: &'a mut String,
    pub compress_state: &'a mut newt_core::CompressState,
    pub scratchpad: &'a dyn newt_core::ScratchpadStore,
    pub step_ledger: &'a dyn newt_core::StepLedger,
    pub active_prompt_context: &'a mut Option<newt_core::TurnPromptContext>,
    pub mode_states: &'a crate::ConversationModeStates,
    // ── posture: baseline reset ⊕ incoming pin ──
    pub baseline: &'a crate::PreferenceBaseline,
    pub pending: &'a mut newt_core::PreferenceActions,
    pub base_provider: &'a mut Option<String>,
    pub base_model: &'a mut Option<String>,
    pub cfg: &'a newt_core::Config,
    pub choice: &'a mut crate::BackendChoice,
    pub inf_url: &'a mut String,
    pub inf_model: &'a mut String,
    pub inf_kind: &'a mut newt_core::BackendKind,
    pub inf_key: &'a mut Option<String>,
    pub inf_context_window: &'a mut Option<u32>,
    // ── the sidecar, live while this tab is active ──
    pub turns_this_conversation: &'a mut usize,
    pub last_resume_listing: &'a mut Vec<String>,
    pub active_roadmap_id: &'a mut Option<String>,
    pub interrupted_objective: &'a mut Option<newt_core::TurnPromptContext>,
    pub input_stash: &'a mut String,
    pub color: bool,
    pub verbose: bool,
}

/// What a completed switch tells the caller.
/// The single result shape for ANY operation that can make a tab active.
///
/// #1669 PR-A (P1). `create_fresh_tab` used to return only an `OwnerHandoff`
/// and discard its `PinRestore`, so `/tab new` always reported `url_changed =
/// false` — and a tab pinned to endpoint X followed by a fresh baseline tab at
/// endpoint Y is an endpoint change whose telemetry never re-probed. Endpoint
/// movement is a fact the restore reports; it must never be inferred from
/// whether ownership happened to change.
#[derive(Debug, Default)]
pub(crate) struct TransitionOutcome {
    /// The endpoint moved — the caller re-probes DGX telemetry.
    pub url_changed: bool,
    /// `Some` when the now-active tab's pin could not be established, so it is
    /// running at baseline and must refuse turns until resolved.
    pub degraded: Option<crate::PinDegraded>,
}

impl TransitionOutcome {
    fn from_restore(restored: &crate::PinRestore) -> Self {
        Self {
            url_changed: restored.url_changed,
            degraded: restored.degraded.clone(),
        }
    }
}

impl TabSwitchCtx<'_> {
    /// Stash the ACTIVE tab's live state into its `TabState`, and flush its pin.
    ///
    /// Called on the outgoing half of every create / activate / close / exit.
    /// The flush is unconditional and failure is non-fatal: the row keeps its
    /// last-good pin and the switch proceeds, because refusing to switch
    /// because a write failed would strand the operator in a tab they asked to
    /// leave.
    fn deactivate(&mut self, tabs: &mut TabSet) {
        if let Err(warning) = crate::persist_preference_actions(
            Some(self.store),
            self.active_conversation_id,
            self.pending,
            self.base_provider,
            self.base_model,
        ) {
            crate::print_newt(&format!("warning: {warning}"), self.color, self.verbose);
        }
        // The rowless interval, handled explicitly. If the row has since
        // materialized, the seed's job is done and it is dropped — its persona
        // was stamped at row birth and its pin actions now have somewhere
        // durable to go. If it has NOT, capture what the store cannot hold:
        // this tab's persona and its unwritten preference actions.
        let materialized = self
            .store
            .exists(tabs.active().conversation_id())
            .unwrap_or(false);
        if let Some(seed) = tabs.active_mut().fresh_seed.as_mut() {
            if materialized {
                // fall through: cleared below
            } else {
                seed.persona = self.active_persona.clone();
                seed.pending = std::mem::take(self.pending);
            }
        }
        if materialized {
            tabs.active_mut().fresh_seed = None;
        }
        let outgoing = tabs.active_mut();
        outgoing.sidecar.turns_this_conversation = *self.turns_this_conversation as u32;
        outgoing.sidecar.last_resume_listing = std::mem::take(self.last_resume_listing);
        outgoing.sidecar.active_roadmap_id = self.active_roadmap_id.take();
        outgoing.sidecar.interrupted_objective = self.interrupted_objective.take();
        outgoing.input_stash = std::mem::take(self.input_stash);
    }

    /// Load the ACTIVE tab's stashed state into the live locals.
    fn hydrate_sidecar(&mut self, tabs: &TabSet) {
        let incoming = tabs.active();
        *self.turns_this_conversation = incoming.sidecar.turns_this_conversation as usize;
        *self.last_resume_listing = incoming.sidecar.last_resume_listing.clone();
        *self.active_roadmap_id = incoming.sidecar.active_roadmap_id.clone();
        *self.interrupted_objective = incoming.sidecar.interrupted_objective.clone();
        *self.input_stash = incoming.input_stash.clone();
    }

    /// Reset every posture axis to the invocation baseline, then overlay the
    /// incoming conversation's pin — the activation rule, as distinct from
    /// resume's sparse overlay.
    fn reset_and_overlay(&mut self) -> crate::PinRestore {
        crate::restore_preference_pin(crate::ConversationPreferenceSwitch {
            store: Some(self.store),
            conversation_id: self.active_conversation_id,
            baseline: self.baseline,
            persona: self.active_persona.as_ref(),
            pending: self.pending,
            base_provider: self.base_provider,
            base_model: self.base_model,
            cfg: self.cfg,
            choice: self.choice,
            inf_url: self.inf_url,
            inf_model: self.inf_model,
            inf_kind: self.inf_kind,
            inf_key: self.inf_key,
            inf_context_window: self.inf_context_window,
            color: self.color,
            verbose: self.verbose,
        })
    }
}

/// What a preflight proved about the incoming conversation.
///
/// #1669 PR-A. `ConversationStore` claims a conversation before any row
/// necessarily exists — rows materialize on the first saved turn — so a tab
/// opened by `/tab new` and never prompted is legitimately **claimed but
/// unmaterialized**. Activation must not assume a row is there; assuming it was
/// the bug that made `startup / tab new / tab 1 / tab 2` fail on the last step.
///
/// Teaching activation about the fresh case (rather than materializing an empty
/// row at tab creation) is the design that matches the store's existing lazy
/// semantics: nothing else in newt writes a row before there is something to
/// record, and an empty row would show up in `/resume` listings as a
/// conversation that never happened.
enum PreparedIncoming {
    /// A durable row, fully read and validated — applying it cannot fail.
    Materialized(Box<crate::PreparedConversationRestore>),
    /// Claimed, no row yet: activation resets to a clean conversation under
    /// that id rather than loading one. Nothing to prepare, nothing to fail.
    Fresh,
}

/// Stage 0 — prove the incoming conversation can be activated, mutating nothing.
///
/// #1669 PR-A (P0). This used to be `exists() + load()`, which is a weaker
/// claim than it appeared: `restore_conversation_into_session` ALSO validated
/// the prompt receipt and its context, so a conversation whose row loaded but
/// whose receipt did not would fail after the outgoing tab had already been
/// deactivated and the active pointer moved. Preflight now performs the whole
/// prepare, so what Stage 0 proves is exactly what commit needs.
///
/// A store error is NOT "absent" (the #1030 hazard): a transient SQLITE_BUSY or
/// NFS IO error must abort the switch, never be mistaken for a fresh tab and
/// silently reset a conversation that actually has content.
fn preflight(
    store: &newt_core::ConversationStore,
    persona_store: &crate::PersonaStore,
    id: &str,
) -> Result<PreparedIncoming, TabError> {
    #[cfg(test)]
    if let Some(reason) = test_seam::forced_failure(id) {
        return Err(TabError::PreflightFailed { reason });
    }
    match store.exists(id) {
        Ok(true) => match crate::prepare_conversation_restore(store, persona_store, id) {
            Ok(prepared) => Ok(PreparedIncoming::Materialized(Box::new(prepared))),
            Err(e) => Err(TabError::PreflightFailed {
                reason: format!("its conversation could not be read ({e})"),
            }),
        },
        Ok(false) => Ok(PreparedIncoming::Fresh),
        Err(e) => Err(TabError::PreflightFailed {
            reason: format!("the conversation store could not be queried ({e})"),
        }),
    }
}

impl TabSwitchCtx<'_> {
    /// Put the live session on a CLEAN conversation under the id already in
    /// `active_conversation_id`.
    ///
    /// Shares `handle_new_conversation`'s scoped clear with `/new` so the two
    /// cannot drift: memory, system prompt, mode states, compression
    /// anti-thrash, scratchpad, plan ledger, prompt receipt. Task-scoped
    /// session resources the ADR marks global across tabs (semantic index,
    /// experiential ledger, nav warmup) are deliberately untouched — clearing
    /// them would let opening a tab discard work another tab depends on.
    fn reset_to_clean_conversation(&mut self) {
        self.compress_state.reset();
        let mut reset = crate::ConversationResetContext {
            memory: self.memory,
            system: self.system,
            conversation_id: self.active_conversation_id,
            mode_states: self.mode_states,
        };
        crate::reset_conversation(self.workspace, self.active_persona.as_ref(), &mut reset);
        crate::ConversationScopedState {
            scratchpad: self.scratchpad,
            step_ledger: self.step_ledger,
            active_prompt_context: self.active_prompt_context,
        }
        .clear();
    }

    /// Stages 4–6 for whichever tab is now active.
    ///
    /// Takes the PREPARED incoming state by value, so there is no fallible
    /// store read here and therefore no restore error for this function to
    /// swallow — the earlier shape logged a late failure and carried on, which
    /// is precisely the half-applied switch the split removes.
    fn commit_incoming(
        &mut self,
        tabs: &TabSet,
        incoming_id: &str,
        incoming: PreparedIncoming,
    ) -> crate::PinRestore {
        match incoming {
            PreparedIncoming::Materialized(prepared) => {
                let mut restore = crate::ConversationCommandContext {
                    store: self.store,
                    persona_store: self.persona_store,
                    workspace: self.workspace,
                    memory: self.memory,
                    system: self.system,
                    active_persona: self.active_persona,
                    active_conversation_id: self.active_conversation_id,
                    compress_state: self.compress_state,
                    scratchpad: self.scratchpad,
                    step_ledger: self.step_ledger,
                    active_prompt_context: self.active_prompt_context,
                    mode_states: self.mode_states,
                };
                let (_, warning) = crate::commit_conversation_restore(&mut restore, *prepared);
                if let Some(w) = warning {
                    crate::print_newt(&format!("warning: {w}"), self.color, self.verbose);
                }
            }
            PreparedIncoming::Fresh => {
                *self.active_conversation_id = incoming_id.to_string();
                self.reset_to_clean_conversation();
                // A rowless tab's persona lives in its seed, because the store
                // has no row to read it back from. Without this the tab would
                // silently keep whichever persona the OUTGOING tab had — and
                // then be stamped with it, permanently, at materialization.
                let seeded = tabs
                    .active()
                    .fresh_seed
                    .as_ref()
                    .and_then(|s| s.persona.clone());
                *self.active_persona = seeded;
                newt_core::cognition::set_persona_cognition(
                    self.active_persona
                        .as_ref()
                        .and_then(|p| p.profile.cognition),
                );
                *self.system = crate::rebuild_system_prompt(
                    self.workspace,
                    self.memory,
                    self.active_persona.as_ref(),
                    self.active_conversation_id,
                );
            }
        }
        let restored = self.reset_and_overlay();
        // AFTER the overlay, which zeroes `pending` on every switch: a rowless
        // tab's unwritten preference actions are restored here so a `/model` or
        // `/backends` choice made before the first prompt survives a switch.
        if let Some(seed) = tabs.active().fresh_seed.as_ref() {
            *self.pending = seed.pending.clone();
        }
        self.hydrate_sidecar(tabs);
        restored
    }
}

/// **Activate a tab** — the ONE transactional switch. Every front door uses it:
/// `/tab <n>`, `/tab next|prev`, adopting a conversation already open in
/// another tab, and closing the active tab.
///
/// Order is the contract:
/// 1. preflight the incoming conversation (mutates nothing; aborts cleanly);
/// 2. deactivate the outgoing tab — flush its pin, stash sidecar + input;
/// 3. commit the active pointer;
/// 4. hydrate the incoming conversation (load, or reset if it is fresh);
/// 5. baseline reset ⊕ incoming pin — **history-independent**, which is what
///    makes activation different from resume's sparse overlay;
/// 6. record whether the pin actually took, so a tab that could not establish
///    its posture is degraded rather than silently running at baseline;
/// 7. hand off lifecycle ownership.
///
/// A Stage-0 failure returns before step 2, so the outgoing tab stays fully
/// active and nothing is touched.
pub(crate) fn activate_tab(
    ctx: &mut TabSwitchCtx<'_>,
    tabs: &mut TabSet,
    target: usize,
) -> Result<TransitionOutcome, TabError> {
    let incoming_id = tabs
        .get(target)
        .ok_or(TabError::OutOfRange { open: tabs.len() })?
        .conversation_id()
        .to_string();
    if target == tabs.active_index() {
        return Ok(TransitionOutcome {
            url_changed: false,
            degraded: tabs.active().pin_degraded.clone(),
        });
    }
    let incoming = preflight(ctx.store, ctx.persona_store, &incoming_id)?;

    ctx.deactivate(tabs);
    let handoff = tabs.activate(target)?;
    let restored = ctx.commit_incoming(tabs, &incoming_id, incoming);
    tabs.active_mut().pin_degraded = restored.degraded.clone();
    handoff.apply();
    Ok(TransitionOutcome::from_restore(&restored))
}

/// **Create a fresh tab** — a genuinely new conversation, not a relabelled one.
///
/// The claim is taken BEFORE the outgoing tab is deactivated, so a refused
/// claim (vanishingly unlikely for a brand-new id, but not impossible) leaves
/// the session exactly where it was.
pub(crate) fn create_fresh_tab(
    ctx: &mut TabSwitchCtx<'_>,
    tabs: &mut TabSet,
) -> Result<TransitionOutcome, TabError> {
    let fresh = newt_core::new_conversation_id();
    match ctx.store.claim(&fresh) {
        Ok(newt_core::ClaimOutcome::Claimed) => {}
        Ok(newt_core::ClaimOutcome::HeldBy { host, pid }) => {
            return Err(TabError::PreflightFailed {
                reason: format!("a new conversation id was already held (pid {pid} on {host})"),
            })
        }
        Err(e) => {
            return Err(TabError::PreflightFailed {
                reason: format!("the new conversation could not be claimed ({e})"),
            })
        }
    }
    // Captured BEFORE deactivate, which may itself change nothing but reads
    // clearer as "the persona this tab is born with".
    let inherited_persona = ctx.active_persona.clone();
    ctx.deactivate(tabs);
    let (_, handoff) = tabs.open(newt_core::lifecycle::new_session_id(), &fresh);
    // A fresh tab keeps the persona active when it was opened — the same rule
    // `/new` follows — and OWNS it from here, so another tab cannot overwrite
    // it and no other tab's persona can be stamped onto this conversation.
    tabs.active_mut().fresh_seed = Some(crate::tabs::FreshSeed {
        persona: inherited_persona,
        pending: newt_core::PreferenceActions::default(),
    });
    // A fresh tab is a NEW conversation: same reset `/new` performs, so it
    // cannot inherit conversation-shaped state from the tab it was opened
    // from. `Incoming::Fresh` routes through exactly that primitive.
    let restored = ctx.commit_incoming(tabs, &fresh, PreparedIncoming::Fresh);
    tabs.active_mut().pin_degraded = restored.degraded.clone();
    handoff.apply();
    // The endpoint CAN move here: leaving a tab pinned to X for a fresh tab at
    // the baseline Y is a real route change, and the caller must re-probe.
    Ok(TransitionOutcome::from_restore(&restored))
}

/// A deterministic way to make one conversation fail Stage 0.
///
/// The brief asks for "failed active-tab close leaves the old tab intact" to be
/// proven, and the failure must be deterministic — no sleeps, no corrupting a
/// SQLite file and hoping. A thread-local seam is the honest instrument: it
/// makes exactly one conversation unpreflightable for exactly one test thread,
/// and it is compiled out of the shipped binary.
#[cfg(test)]
mod test_seam {
    use std::cell::RefCell;

    thread_local! {
        static FAILING: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// Make `id` fail preflight for the rest of this test thread.
    pub(super) fn fail_preflight_for(id: &str) {
        FAILING.with(|f| *f.borrow_mut() = Some(id.to_string()));
    }

    pub(super) fn clear() {
        FAILING.with(|f| *f.borrow_mut() = None);
    }

    pub(super) fn forced_failure(id: &str) -> Option<String> {
        FAILING.with(|f| {
            f.borrow()
                .as_deref()
                .filter(|failing| *failing == id)
                .map(|_| "its conversation could not be read (test seam)".to_string())
        })
    }
}

/// The operator-facing text for a refused tab operation.
///
/// One vocabulary for every front door: the `/tab` handler and the adoption
/// seam's callers print the same words for the same refusal.
pub(crate) fn refusal_text(e: &TabError) -> String {
    match e {
        TabError::OutOfRange { open } => {
            let plural = if *open == 1 { "tab" } else { "tabs" };
            format!("no such tab — {open} {plural} open (1..={open})")
        }
        TabError::LastTab => "can't close the last tab — `:q` is how you leave newt".to_string(),
        TabError::PreflightFailed { reason } => format!("staying on this tab — {reason}"),
    }
}

/// What adopting a conversation did.
#[derive(Debug)]
pub(crate) enum Adopted {
    /// It was already open in another tab; that tab was ACTIVATED. No second
    /// tab now points at it.
    ActivatedExistingTab {
        index: usize,
        outcome: TransitionOutcome,
    },
    /// It is the conversation the active tab already holds.
    AlreadyHere,
    /// No tab holds it — the caller may adopt it into the ACTIVE tab using its
    /// own verb's semantics (resume's sparse overlay, which is deliberately not
    /// activation's reset ⊕ overlay).
    ProceedInActiveTab,
}

/// **The one tab-aware conversation-adoption seam.**
///
/// #1669 PR-A blocker 2. Every path that can select or replace the active
/// conversation asks this FIRST — `/resume`, `/conversation restore`, roadmap
/// navigation — so the uniqueness invariant lives in one place instead of being
/// re-derived (or forgotten) per command.
///
/// **Why process-level claims are not enough:** `ConversationStore::claim`
/// returns `Claimed` for "a re-affirmation of its own claim", so a second tab
/// in the SAME process can claim a conversation the first tab already holds and
/// see success. Tab-level uniqueness must therefore be enforced here, against
/// the tab set, not delegated to the store.
///
/// Invariant: **one conversation → at most one open tab.**
pub(crate) fn adopt_conversation(
    ctx: &mut TabSwitchCtx<'_>,
    tabs: &mut TabSet,
    target: &str,
) -> Result<Adopted, TabError> {
    if let Some(index) = tabs.find_by_conversation(target) {
        if index == tabs.active_index() {
            return Ok(Adopted::AlreadyHere);
        }
        let outcome = activate_tab(ctx, tabs, index)?;
        return Ok(Adopted::ActivatedExistingTab { index, outcome });
    }
    if *ctx.active_conversation_id == target {
        return Ok(Adopted::AlreadyHere);
    }
    Ok(Adopted::ProceedInActiveTab)
}

/// **Session exit** — flush the active tab, then release every claim.
///
/// #1669 PR-A (ADR "exit flushes then releases ALL claims"). Two halves, and
/// the FLUSH half was missing entirely: exit released claims but never
/// deactivated the active tab, so the operator's last unwritten preference
/// actions — a `/model` or `/psyche` change made after the final turn — were
/// dropped on the way out. Every other way of leaving a tab flushes; leaving
/// newt did not.
///
/// Order matters for the same reason it does in `close_tab`: flush while the
/// tab is still ours, release after.
pub(crate) fn exit_release_all(ctx: &mut TabSwitchCtx<'_>, tabs: &mut TabSet) -> Vec<String> {
    ctx.deactivate(tabs);
    let held: Vec<String> = tabs
        .claimed_conversations()
        .into_iter()
        .map(str::to_string)
        .collect();
    for id in &held {
        if let Err(e) = ctx.store.release(id) {
            crate::print_newt(
                &format!("warning: releasing `{id}` on exit failed: {e}"),
                ctx.color,
                ctx.verbose,
            );
        }
    }
    held
}

/// Why the `/tab` family is unavailable in this session, if it is.
///
/// #1669 PR-A. Extracted from an inline `match` arm so the refusal is a tested
/// value rather than a string literal buried in `run_chat` — the audit found
/// all three refusals unreachable from any test as written.
pub(crate) fn tab_surface_refusal(
    surface_is_rich: bool,
    ephemeral: bool,
    has_store: bool,
) -> Option<&'static str> {
    if !surface_is_rich {
        // Tabs are RichTUI presentation over conversation switching; lean
        // expresses the same capability as scrolled lines. Never silence, never
        // unknown-command — the namespace stays discoverable and the doctrine
        // line explicit.
        return Some(
            "tabs are a rich-TUI feature; this session is single-conversation — \
             use /resume, /new and /rename",
        );
    }
    if ephemeral {
        return Some(
            "tabs need conversation persistence; this session is ephemeral and \
             leaves no trace by design",
        );
    }
    if !has_store {
        return Some("tabs need conversation persistence; this session has no store");
    }
    None
}

/// The message shown when a degraded tab refuses a turn, or `None` when the
/// pin is in force and the turn may proceed.
///
/// Extracted for the same reason: the refusal was inline in `run_chat` and the
/// audit found no test asserting it — the claim "which is what refuses turns"
/// was an assertion about untested code.
pub(crate) fn degraded_turn_refusal(degraded: Option<&crate::PinDegraded>) -> Option<String> {
    degraded.map(|d| {
        format!(
            "{} — this tab's pinned posture is not in force, so the prompt was not accepted. \
             Fix what the pin names (usually a [[backends]] entry) then `/tab retry`, or \
             `/psyche` to repin.",
            d.summary()
        )
    })
}

/// The switch-level result of a close: the removed tab plus the neighbor
/// activation's [`TransitionOutcome`]. Kept here rather than on the pure
/// model's `Closed`, which must not know about pins or endpoints.
#[derive(Debug)]
pub(crate) struct ClosedTab {
    pub tab: crate::tabs::TabState,
    pub outcome: TransitionOutcome,
}

/// **Close a tab.** Closing the ACTIVE one is a normal activation of a
/// neighbor, followed by removal — never a second restoration path.
///
/// The order is the contract, and it is transactional:
/// 1. refuse the last tab;
/// 2. if the target is active, **activate the neighbor through
///    [`activate_tab`]** — if that fails, return the error with the target
///    still active, still claimed, and fully hydrated;
/// 3. only then remove the (now inactive) target from the set;
/// 4. release its claim **last**.
///
/// So a neighbor that cannot be restored leaves the session operational on the
/// tab the operator tried to close, rather than half-torn-down between the two.
///
/// Close is release-without-end: the conversation stays open and
/// `/resume`-able. `/end` remains the verb that ends a conversation.
pub(crate) fn close_tab(
    ctx: &mut TabSwitchCtx<'_>,
    tabs: &mut TabSet,
    target: usize,
) -> Result<ClosedTab, TabError> {
    if target >= tabs.len() {
        return Err(TabError::OutOfRange { open: tabs.len() });
    }
    if tabs.len() == 1 {
        return Err(TabError::LastTab);
    }
    let mut outcome = TransitionOutcome::default();
    if target == tabs.active_index() {
        // The neighbor vim would pick: the tab to the right, else the left.
        let neighbor = if target + 1 < tabs.len() {
            target + 1
        } else {
            target - 1
        };
        // Full transactional activation. On failure NOTHING has been removed,
        // released, or re-anchored — the tab the operator tried to close is
        // still the live one.
        outcome = activate_tab(ctx, tabs, neighbor)?;
    }
    let closed = tabs.close(target)?;
    // Released LAST, after ownership has demonstrably moved.
    if let Err(e) = ctx.store.release(closed.tab.conversation_id()) {
        crate::print_newt(
            &format!("warning: releasing the closed tab's claim failed: {e}"),
            ctx.color,
            ctx.verbose,
        );
    }
    Ok(ClosedTab {
        tab: closed.tab,
        outcome,
    })
}

/// The ONE tab-action handler.
///
/// Four front doors call this — the `/tab` slash family here in PR-A, the vi
/// ex-line and `gt`/`gT` in PR-C, and the bar menu in PR-D — so no front door
/// can grow its own semantics. Errors print through `print_newt`, in the same
/// vocabulary the parser produced.
///
/// Returns whether the backend endpoint moved, so the caller re-probes DGX
/// telemetry exactly as every other backend switch does.
pub(crate) fn handle_tab_action(
    action: crate::tabs::TabAction,
    ctx: &mut TabSwitchCtx<'_>,
    tabs: &mut TabSet,
) -> TransitionOutcome {
    use crate::tabs::TabAction;
    let color = ctx.color;
    let verbose = ctx.verbose;
    let refuse = |e: TabError, tabs: &TabSet| match e {
        TabError::OutOfRange { open } => {
            let plural = if open == 1 { "tab" } else { "tabs" };
            crate::print_newt(
                &format!("no such tab — {open} {plural} open (1..={open})"),
                color,
                verbose,
            );
        }
        TabError::LastTab => {
            let _ = tabs;
            crate::print_newt(
                "can't close the last tab — `:q` is how you leave newt",
                color,
                verbose,
            );
        }
        // Stage 0 refused, so nothing moved. Say that explicitly: an operator
        // who sees only "could not switch" has no way to know whether they are
        // now half-way between two tabs.
        TabError::PreflightFailed { reason } => {
            crate::print_newt(&format!("staying on this tab — {reason}"), color, verbose);
        }
    };

    match action {
        TabAction::List => {
            list_tabs(ctx, tabs);
            TransitionOutcome::default()
        }
        TabAction::New => match create_fresh_tab(ctx, tabs) {
            Ok(outcome) => {
                crate::print_newt(
                    &format!("tab {} — new conversation", tabs.active_index() + 1),
                    color,
                    verbose,
                );
                outcome
            }
            Err(e) => {
                refuse(e, tabs);
                TransitionOutcome::default()
            }
        },
        TabAction::Retry => {
            // Re-run the reset ⊕ overlay for the active tab. Success clears the
            // degraded marker and turns are allowed again; failure re-reports,
            // so the operator can iterate without losing the conversation.
            let restored = ctx.reset_and_overlay();
            let still_degraded = restored.degraded.is_some();
            tabs.active_mut().pin_degraded = restored.degraded.clone();
            match restored.degraded.as_ref() {
                None => crate::print_newt(
                    "pin applied — this tab is no longer degraded",
                    color,
                    verbose,
                ),
                Some(d) => crate::print_newt(
                    &format!("{} — still not in force", d.summary()),
                    color,
                    verbose,
                ),
            }
            let _ = still_degraded;
            TransitionOutcome::from_restore(&restored)
        }
        TabAction::Next => switch_to(ctx, tabs, (tabs.active_index() + 1) % tabs.len(), refuse),
        TabAction::Prev(n) => {
            let len = tabs.len();
            let back = n % len;
            switch_to(ctx, tabs, (tabs.active_index() + len - back) % len, refuse)
        }
        TabAction::Goto(index) => switch_to(ctx, tabs, index, refuse),
        TabAction::Close(which) => {
            let target = which.unwrap_or_else(|| tabs.active_index());
            match close_tab(ctx, tabs, target) {
                Ok(closed) => {
                    crate::print_newt(
                        &format!(
                            "closed tab — `{}` stays open, resume it with /resume",
                            closed.tab.conversation_id()
                        ),
                        color,
                        verbose,
                    );
                    // Endpoint movement comes from the neighbor's restore, not
                    // from whether ownership changed.
                    report_degraded(ctx, tabs);
                    closed.outcome
                }
                Err(e) => {
                    refuse(e, tabs);
                    TransitionOutcome::default()
                }
            }
        }
        TabAction::Move { from, to } => {
            match tabs.move_tab(from, to) {
                Ok(()) => list_tabs(ctx, tabs),
                Err(e) => refuse(e, tabs),
            }
            TransitionOutcome::default()
        }
        TabAction::Rename { index, title } => {
            let target = index.unwrap_or_else(|| tabs.active_index());
            match tabs.get(target).map(|t| t.conversation_id().to_string()) {
                Some(id) => {
                    // `rename_conversation` CREATES the row for a rowless tab,
                    // stamping its persona permanently. Use the TARGET tab's
                    // own persona — its seed when rowless, else the live one
                    // for the active tab — never whatever happens to be active
                    // while renaming some other tab.
                    let persona = tabs
                        .get(target)
                        .and_then(|t| t.fresh_seed.as_ref())
                        .map(|seed| seed.persona.as_ref().map(|p| p.name.clone()))
                        .unwrap_or_else(|| {
                            if target == tabs.active_index() {
                                ctx.active_persona.as_ref().map(|p| p.name.clone())
                            } else {
                                None
                            }
                        });
                    match crate::rename_conversation(ctx.store, &id, &title, persona.as_deref()) {
                        Ok(()) => {
                            // The row now exists, so the seed has been consumed
                            // — its persona is stamped and its pin actions have
                            // a durable home.
                            if let Some(t) = tabs.tabs_mut().get_mut(target) {
                                t.fresh_seed = None;
                            }
                            crate::print_newt(
                                &format!("tab {} renamed to '{title}'", target + 1),
                                color,
                                verbose,
                            );
                        }
                        Err(e) => crate::print_newt(&format!("rename failed: {e}"), color, verbose),
                    }
                    TransitionOutcome::default()
                }
                None => {
                    refuse(TabError::OutOfRange { open: tabs.len() }, tabs);
                    TransitionOutcome::default()
                }
            }
        }
    }
}

fn switch_to(
    ctx: &mut TabSwitchCtx<'_>,
    tabs: &mut TabSet,
    target: usize,
    refuse: impl Fn(TabError, &TabSet),
) -> TransitionOutcome {
    match activate_tab(ctx, tabs, target) {
        Ok(switched) => {
            // A tab whose pin did not take says so on arrival, every time —
            // the operator must never discover it by having a turn refused.
            let marker = match &switched.degraded {
                Some(d) => format!("  [{}]", d.summary()),
                None => String::new(),
            };
            crate::print_newt(
                &format!(
                    "tab {} — {}{marker}",
                    tabs.active_index() + 1,
                    crate::tab_label(ctx.store, tabs.active().conversation_id())
                ),
                ctx.color,
                ctx.verbose,
            );
            switched
        }
        Err(e) => {
            refuse(e, tabs);
            TransitionOutcome::default()
        }
    }
}

/// Say `!pin` out loud for the tab that is active NOW.
///
/// #1669 PR-A (P1). Direct `/tab <n>` already reported it; adoption and
/// active-close did not, so `/resume`ing into a degraded tab or closing into a
/// degraded neighbor left the operator to discover it by having a turn refused.
/// The refusal must be the SECOND thing they learn, never the first.
pub(crate) fn report_degraded(ctx: &TabSwitchCtx<'_>, tabs: &TabSet) {
    if let Some(d) = tabs.active().pin_degraded.as_ref() {
        crate::print_newt(
            &format!(
                "{} — turns are refused on this tab until the pin is in force (`/tab retry`)",
                d.summary()
            ),
            ctx.color,
            ctx.verbose,
        );
    }
}

/// The tab list — and, with no pixels in this slice, THE view of tab state.
/// Labels are computed fresh from the store rather than stored, so `/rename`
/// honesty is free and a title can never go stale in the list.
fn list_tabs(ctx: &TabSwitchCtx<'_>, tabs: &TabSet) {
    crate::print_newt("open tabs:", ctx.color, ctx.verbose);
    for (i, tab) in tabs.tabs().iter().enumerate() {
        let marker = if i == tabs.active_index() { "*" } else { " " };
        // Verbose names the tab's SessionId too: it is the identity Herdr
        // attributes events to, so an operator debugging pane attribution can
        // see which tab owns what without guessing.
        let identity = if ctx.verbose {
            format!("  [{}]", tab.session_id())
        } else {
            String::new()
        };
        // With no pixels in this slice the list IS the tab-state view, so a
        // degraded tab must be visible here — not only on the switch that
        // produced it, which the operator may have scrolled past.
        let degraded = match tab.pin_degraded.as_ref() {
            Some(d) => format!("  {}", d.summary()),
            None => String::new(),
        };
        crate::print_newt(
            &format!(
                "{marker} {}. {}{degraded}{identity}",
                i + 1,
                crate::tab_label(ctx.store, tab.conversation_id())
            ),
            ctx.color,
            ctx.verbose,
        );
    }
}

/// #1669 PR-A — the state-machine proofs.
///
/// These drive the REAL transitions against a REAL `ConversationStore`, because
/// every property here is about what happens to live session state across a
/// switch. A pure-model test cannot see a half-applied hydrate, a claim
/// released too early, or a pin that quietly did not take.
#[cfg(test)]
mod state_machine_tests {
    use super::*;
    use crate::tabs::{TabAction, TabSet};
    use newt_core::lifecycle::new_session_id;

    /// Every backend shares one endpoint AND one model, so no route change ever
    /// fires `refresh_backend`'s served-adoption probe — the tier stays
    /// network-free while still exercising the real posture machinery.
    fn cfg_with(names: &[&str]) -> newt_core::Config {
        newt_core::Config {
            default_backend: names.first().map(|n| (*n).to_string()),
            backends: names
                .iter()
                .map(|name| newt_core::BackendConfig {
                    name: (*name).to_string(),
                    endpoint: "http://backend.test:1".to_string(),
                    model: Some("m0".to_string()),
                    kind: Some(newt_core::BackendKind::Openai),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    /// Owns every piece of live session state a switch touches, so a test can
    /// drive `activate_tab` / `create_fresh_tab` / `adopt_conversation` /
    /// `close_tab` exactly as `run_chat` does.
    struct Harness {
        _root: tempfile::TempDir,
        _ws: tempfile::TempDir,
        store: newt_core::ConversationStore,
        persona_store: crate::PersonaStore,
        workspace: String,
        memory: newt_core::MemoryManager,
        system: String,
        active_persona: Option<crate::Persona>,
        active_conversation_id: String,
        compress_state: newt_core::CompressState,
        scratchpad: newt_core::SessionScratchpadStore,
        step_ledger: newt_core::SessionStepLedger,
        active_prompt_context: Option<newt_core::TurnPromptContext>,
        mode_states: crate::ConversationModeStates,
        baseline: crate::PreferenceBaseline,
        pending: newt_core::PreferenceActions,
        base_provider: Option<String>,
        base_model: Option<String>,
        cfg: newt_core::Config,
        choice: crate::BackendChoice,
        inf_url: String,
        inf_model: String,
        inf_kind: newt_core::BackendKind,
        inf_key: Option<String>,
        inf_context_window: Option<u32>,
        turns_this_conversation: usize,
        last_resume_listing: Vec<String>,
        active_roadmap_id: Option<String>,
        interrupted_objective: Option<newt_core::TurnPromptContext>,
        input_stash: String,
    }

    impl Harness {
        fn new(backends: &[&str]) -> (Self, TabSet) {
            let root = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            let store = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
            let cfg = cfg_with(backends);
            let choice = crate::resolve_backend_choice(&cfg);
            let first = newt_core::new_conversation_id();
            store.claim(&first).unwrap();
            let workspace = ws.path().to_string_lossy().into_owned();
            let memory = newt_core::MemoryManager::new();
            let system = String::new();
            let tabs = TabSet::new(new_session_id(), &first);
            (
                Self {
                    _root: root,
                    _ws: ws,
                    store,
                    persona_store: crate::PersonaStore::new(std::path::PathBuf::from(
                        "/nonexistent",
                    )),
                    workspace,
                    memory,
                    system,
                    active_persona: None,
                    active_conversation_id: first,
                    compress_state: newt_core::CompressState::new(),
                    scratchpad: newt_core::SessionScratchpadStore::default(),
                    step_ledger: newt_core::SessionStepLedger::default(),
                    active_prompt_context: None,
                    mode_states: crate::ConversationModeStates::default(),
                    baseline: crate::PreferenceBaseline::snapshot(None, None),
                    pending: newt_core::PreferenceActions::default(),
                    base_provider: None,
                    base_model: None,
                    inf_url: choice.url.clone(),
                    inf_model: choice.model.clone(),
                    inf_kind: choice.kind,
                    inf_key: choice.api_key.clone(),
                    inf_context_window: choice.context_window,
                    choice,
                    cfg,
                    turns_this_conversation: 0,
                    last_resume_listing: Vec::new(),
                    active_roadmap_id: None,
                    interrupted_objective: None,
                    input_stash: String::new(),
                },
                tabs,
            )
        }

        fn ctx(&mut self) -> TabSwitchCtx<'_> {
            TabSwitchCtx {
                store: &self.store,
                persona_store: &self.persona_store,
                workspace: &self.workspace,
                memory: &mut self.memory,
                system: &mut self.system,
                active_persona: &mut self.active_persona,
                active_conversation_id: &mut self.active_conversation_id,
                compress_state: &mut self.compress_state,
                scratchpad: &self.scratchpad,
                step_ledger: &self.step_ledger,
                active_prompt_context: &mut self.active_prompt_context,
                mode_states: &self.mode_states,
                baseline: &self.baseline,
                pending: &mut self.pending,
                base_provider: &mut self.base_provider,
                base_model: &mut self.base_model,
                cfg: &self.cfg,
                choice: &mut self.choice,
                inf_url: &mut self.inf_url,
                inf_model: &mut self.inf_model,
                inf_kind: &mut self.inf_kind,
                inf_key: &mut self.inf_key,
                inf_context_window: &mut self.inf_context_window,
                turns_this_conversation: &mut self.turns_this_conversation,
                last_resume_listing: &mut self.last_resume_listing,
                active_roadmap_id: &mut self.active_roadmap_id,
                interrupted_objective: &mut self.interrupted_objective,
                input_stash: &mut self.input_stash,
                color: false,
                verbose: false,
            }
        }

        /// Open a second tab already holding `id`, **through the real
        /// transitions**: deactivate the outgoing tab first (so its state is
        /// stashed into ITS `TabState`), then open and hydrate.
        ///
        /// Calling `TabSet::open` directly skips the deactivate, so the
        /// outgoing tab's live state is stashed into the INCOMING tab — a
        /// fixture bug that silently invalidates every round-trip assertion.
        /// It cost me two red tests to notice, which is the argument for the
        /// helper.
        fn open_tab_on(&mut self, tabs: &mut TabSet, id: &str) {
            self.store.claim(id).unwrap();
            let mut ctx = self.ctx();
            ctx.deactivate(tabs);
            let (_, handoff) = tabs.open(new_session_id(), id);
            *ctx.active_conversation_id = id.to_string();
            let incoming =
                preflight(ctx.store, ctx.persona_store, id).expect("fixture row must preflight");
            ctx.commit_incoming(tabs, id, incoming);
            handoff.apply();
        }

        /// A durable conversation row (rows materialize on the first saved turn).
        fn durable(&self, title: &str) -> String {
            let id = self.store.create(title, None).unwrap();
            self.store.append_turn(&id, "q", "a").unwrap();
            id
        }

        /// Everything the ADR promises is restored exactly, as one comparable
        /// value — so a round-trip assertion cannot quietly omit a field.
        fn snapshot(&self, tabs: &TabSet) -> Snapshot {
            use newt_core::{ScratchpadStore, StepLedger};
            let scratchpad: Vec<(String, String)> = self.scratchpad.entries().into_iter().collect();
            Snapshot {
                conversation_id: self.active_conversation_id.clone(),
                inf_url: self.inf_url.clone(),
                inf_model: self.inf_model.clone(),
                inf_kind: self.inf_kind,
                inf_key_fingerprint: fingerprint(self.inf_key.as_deref()),
                inf_context_window: self.inf_context_window,
                choice_name: self.choice.name.clone(),
                provider: std::env::var("NEWT_PROVIDER").ok(),
                model: std::env::var("NEWT_DGX_MODEL").ok(),
                cognition: newt_core::cognition::cli_cognition(),
                tenacity: newt_core::tenacity::cli_tenacity(),
                persona_cognition: newt_core::cognition::persona_cognition(),
                persona: self.active_persona.as_ref().map(|p| p.name.clone()),
                scratchpad,
                plan_steps: self.step_ledger.steps(),
                has_prompt_context: self.active_prompt_context.is_some(),
                system: self.system.clone(),
                turns: self.turns_this_conversation,
                roadmap: self.active_roadmap_id.clone(),
                resume_listing: self.last_resume_listing.clone(),
                input_stash: self.input_stash.clone(),
                degraded: tabs.active().pin_degraded.as_ref().map(|d| d.summary()),
            }
        }
    }

    /// The whole state surface a transition may touch.
    ///
    /// Every field here is compared exactly. `inf_key` is represented by a
    /// non-reversible fingerprint rather than its value, so a mismatch can be
    /// DETECTED without a secret ever reaching a test log or a panic message —
    /// an assertion that prints the API key on failure would be a worse bug
    /// than the one it was written to catch.
    #[derive(Debug, Clone, PartialEq)]
    struct Snapshot {
        conversation_id: String,
        // backend quintet
        inf_url: String,
        inf_model: String,
        inf_kind: newt_core::BackendKind,
        inf_key_fingerprint: Option<u64>,
        inf_context_window: Option<u32>,
        choice_name: String,
        // projected dials + route env
        provider: Option<String>,
        model: Option<String>,
        cognition: newt_core::cognition::CognitionOverride,
        tenacity: Option<newt_core::Tenacity>,
        persona_cognition: Option<newt_core::role_profile::Cognition>,
        // conversation-owned live state
        persona: Option<String>,
        scratchpad: Vec<(String, String)>,
        plan_steps: Vec<newt_core::Step>,
        has_prompt_context: bool,
        system: String,
        // sidecar + input
        turns: usize,
        roadmap: Option<String>,
        resume_listing: Vec<String>,
        input_stash: String,
        degraded: Option<String>,
    }

    /// Non-reversible, stable within a process — enough to prove "unchanged" or
    /// "changed" without ever materializing the secret.
    fn fingerprint(secret: Option<&str>) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        secret.map(|s| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            s.hash(&mut h);
            h.finish()
        })
    }

    fn guard() -> newt_core::test_guard::GlobalSettingsGuard {
        newt_core::test_guard::GlobalSettingsGuard::acquire()
    }

    // ── 1. `/tab new` produces genuinely isolated state ───────────────────

    /// Blocker 1. A fresh tab must not inherit conversation-shaped state from
    /// the tab it was opened from. Minting an id and relabelling the tab would
    /// leave the outgoing conversation's memory, plan, scratchpad and prompt
    /// receipt in place under a new name.
    #[test]
    fn tab_new_produces_genuinely_isolated_state() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        // Dirty everything a conversation owns.
        {
            use newt_core::{ScratchpadStore, StepLedger};
            h.scratchpad.set("current_task", "tab A's task".into());
            h.step_ledger.restore(&newt_core::PlanSnapshot::default());
            assert!(h.scratchpad.get("current_task").is_some());
        }
        h.turns_this_conversation = 9;
        h.active_roadmap_id = Some("road-a".into());
        h.interrupted_objective = None;
        let outgoing = h.active_conversation_id.clone();

        let mut ctx = h.ctx();
        let _ = create_fresh_tab(&mut ctx, &mut tabs).expect("a brand-new id always claims");

        assert_ne!(h.active_conversation_id, outgoing, "a NEW conversation");
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs.active().conversation_id(), h.active_conversation_id);
        {
            use newt_core::ScratchpadStore;
            assert!(
                h.scratchpad.get("current_task").is_none(),
                "a fresh tab must not inherit the outgoing conversation's scratchpad"
            );
        }
        assert_eq!(h.turns_this_conversation, 0, "fresh turn count");
        assert_eq!(h.active_roadmap_id, None, "fresh roadmap binding");
        assert!(h.active_prompt_context.is_none(), "fresh prompt receipt");
        // `/tab new` deliberately does NOT opt the session out of auto-resume
        // the way `/new` does: it does not abandon the resumed conversation —
        // that one is still open in its own tab — so next launch's auto-resume
        // is none of this command's business. The switch machinery therefore
        // never touches that flag, which is why it is not in `TabSwitchCtx`.
    }

    // ── 2. switching back to a never-used fresh tab works ─────────────────

    /// Blocker 1's edge case, verbatim: `startup / tab new / tab 1 / tab 2`.
    ///
    /// Tab 2 has never been prompted, so its conversation is **claimed but has
    /// no row** — rows materialize on the first saved turn. Activation must not
    /// assume a row exists.
    #[test]
    fn startup_then_tab_new_then_tab_1_then_tab_2() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let tab1_conversation = h.active_conversation_id.clone();

        {
            let mut ctx = h.ctx();
            let _ = create_fresh_tab(&mut ctx, &mut tabs).unwrap();
        }
        let tab2_conversation = h.active_conversation_id.clone();
        assert!(
            !h.store.exists(&tab2_conversation).unwrap(),
            "fixture precondition: the fresh tab has no materialized row"
        );

        // /tab 1
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).expect("/tab 1");
        }
        assert_eq!(h.active_conversation_id, tab1_conversation);

        // /tab 2 — the step that used to fail
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 1)
                .expect("/tab 2 must work even though tab 2 has no conversation row yet");
        }
        assert_eq!(h.active_conversation_id, tab2_conversation);
        assert_eq!(tabs.active_index(), 1);
    }

    // ── P0-2: the claimed-but-unmaterialized interval ─────────────────────

    fn persona_named(name: &str) -> crate::Persona {
        crate::test_persona(name, "be helpful", std::path::PathBuf::from("/dev/null"))
    }

    /// A fresh tab owns its persona rather than borrowing the session's.
    ///
    /// Create B while persona **P** is active, visit a conversation whose
    /// persona is **Q**, return to still-rowless B: B must be back on P.
    /// Before the seed, the `Fresh` arm restored no persona at all, so B
    /// silently ran under Q — and would have been STAMPED with Q at its first
    /// prompt, permanently, since the store has no persona UPDATE path.
    #[test]
    fn a_rowless_tab_keeps_its_own_persona_across_a_visit_to_another() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();

        // P is active when B is created.
        h.active_persona = Some(persona_named("P"));
        {
            let mut ctx = h.ctx();
            let _ = create_fresh_tab(&mut ctx, &mut tabs).unwrap();
        }
        let b = h.active_conversation_id.clone();
        assert!(!h.store.exists(&b).unwrap(), "B is rowless");
        assert_eq!(
            h.active_persona.as_ref().map(|p| p.name.as_str()),
            Some("P"),
            "a fresh tab keeps the persona active when it was opened, as /new does"
        );

        // Visit A, which has no persona at all (the Q=None case), then return.
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }
        assert_eq!(h.active_persona.as_ref().map(|p| p.name.as_str()), None);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 1).unwrap();
        }
        assert_eq!(
            h.active_persona.as_ref().map(|p| p.name.as_str()),
            Some("P"),
            "returning to the rowless tab restores ITS persona, not the other tab's"
        );
        assert!(
            !h.store.exists(&b).unwrap(),
            "and visiting a tab still creates no ghost /resume row"
        );
    }

    /// Renaming an INACTIVE rowless tab materializes its row — and must stamp
    /// that tab's persona, not whatever is active in the tab doing the renaming.
    #[test]
    fn renaming_an_inactive_rowless_tab_does_not_capture_the_live_persona() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();

        // B is created with NO persona.
        h.active_persona = None;
        {
            let mut ctx = h.ctx();
            let _ = create_fresh_tab(&mut ctx, &mut tabs).unwrap();
        }
        let b = h.active_conversation_id.clone();
        // Back to A, and make Q active there.
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }
        h.active_persona = Some(persona_named("Q"));

        // Rename the inactive rowless tab B from here.
        {
            let mut ctx = h.ctx();
            handle_tab_action(
                TabAction::Rename {
                    index: Some(1),
                    title: "B titled".into(),
                },
                &mut ctx,
                &mut tabs,
            );
        }
        assert!(h.store.exists(&b).unwrap(), "rename materialized B's row");
        let record = h.store.load(&b).unwrap();
        assert_eq!(
            record.persona, None,
            "B was created with no persona; renaming it from a Q tab must not stamp Q"
        );
        assert_eq!(record.title, "B titled");
        assert!(
            tabs.get(1).unwrap().fresh_seed.is_none(),
            "the seed is consumed once the row exists"
        );
    }

    /// A preference change made in a rowless tab survives a switch away and
    /// back. `persist_preference_actions` cannot write it (no row) and the
    /// overlay zeroes `pending` on every switch, so without the seed it was
    /// silently discarded by the next `/tab`.
    #[test]
    fn a_preference_change_in_a_rowless_tab_survives_a_switch() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol", "other"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();
        {
            let mut ctx = h.ctx();
            let _ = create_fresh_tab(&mut ctx, &mut tabs).unwrap();
        }
        let b = h.active_conversation_id.clone();

        // Operator picks a backend in the rowless tab.
        newt_core::runtime::mark_backend_pick("other");
        {
            let mut ctx = h.ctx();
            *ctx.pending = newt_core::runtime::drain_preference_actions();
            activate_tab(&mut ctx, &mut tabs, 0).expect("switch away");
        }
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 1).expect("switch back");
        }
        assert!(
            !h.pending.is_empty(),
            "the rowless tab's unwritten preference action came back with it"
        );
        assert!(!h.store.exists(&b).unwrap(), "still no ghost row");

        // And once the row materializes, the held action lands on it.
        h.store.create_with_id(&b, "B", None).unwrap();
        crate::persist_preference_actions(
            Some(&h.store),
            &b,
            &mut h.pending,
            &mut h.base_provider,
            &mut h.base_model,
        )
        .unwrap();
        assert_eq!(
            h.store.preference_pin(&b).unwrap().and_then(|p| p.backend),
            Some("other".to_string()),
            "the first durable write materializes the preference chosen while rowless"
        );
    }

    /// `/tab new` alone still creates no `/resume`-visible row — the rule the
    /// seed exists to preserve rather than work around.
    #[test]
    fn tab_new_alone_creates_no_ghost_resume_row() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let before = h.store.list().map(|v| v.len()).unwrap_or(0);
        {
            let mut ctx = h.ctx();
            let _ = create_fresh_tab(&mut ctx, &mut tabs).unwrap();
            let _ = create_fresh_tab(&mut ctx, &mut tabs).unwrap();
        }
        assert_eq!(
            h.store.list().map(|v| v.len()).unwrap_or(0),
            before,
            "two fresh tabs, zero new rows in the resume listing"
        );
    }

    // ── 3. A -> B -> A exact restoration ──────────────────────────────────

    /// The ADR's round-trip property, over every field the contract promises —
    /// compared as one struct so a field cannot be silently omitted.
    #[test]
    fn a_to_b_to_a_restores_a_exactly() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol", "other"]);
        let a_conversation = h.active_conversation_id.clone();
        h.store.create_with_id(&a_conversation, "A", None).unwrap();
        let b_conversation = h.durable("B");

        h.open_tab_on(&mut tabs, &b_conversation);
        // Back to A and hydrate it once, so the baseline snapshot is taken of a
        // tab that has actually been activated — not of a half-built fixture.
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).expect("settle on A");
        }
        h.turns_this_conversation = 4;
        h.active_roadmap_id = Some("road-a".into());
        h.input_stash = "half typed in A".into();
        let a_before = h.snapshot(&tabs);

        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 1).expect("A→B");
        }
        h.turns_this_conversation = 77;
        h.active_roadmap_id = Some("road-b".into());
        h.input_stash = "half typed in B".into();

        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).expect("B→A");
        }
        assert_eq!(
            h.snapshot(&tabs),
            a_before,
            "A must come back exactly as it was left"
        );
        assert_eq!(h.active_conversation_id, a_conversation);
    }

    // ── 4. no cross-tab leakage of dials / pins ───────────────────────────

    /// Unpinned baseline semantics: an unpinned tab must resolve to the
    /// invocation baseline, never to whatever the previous tab pinned.
    #[test]
    fn an_unpinned_tab_does_not_inherit_another_tabs_pin() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol", "other"]);
        let unpinned = h.active_conversation_id.clone();

        // B is pinned to `other` with a dial.
        let pinned = h.durable("pinned");
        h.store.claim(&pinned).unwrap();
        h.store
            .update_preference_pin(
                &pinned,
                &newt_core::OperatorPreferencePin {
                    backend: Some("other".into()),
                    tenacity: Some(newt_core::Tenacity::Relentless),
                    ..Default::default()
                },
            )
            .unwrap();
        h.open_tab_on(&mut tabs, &pinned);
        // Settle on the unpinned tab first, so activating the pinned one is a
        // real switch rather than a no-op on the already-active tab.
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).expect("settle on the unpinned tab");
            activate_tab(&mut ctx, &mut tabs, 1).expect("activate the pinned tab");
        }
        assert_eq!(
            std::env::var("NEWT_PROVIDER").ok().as_deref(),
            Some("other"),
            "the pinned tab routes to its pin"
        );
        assert_eq!(
            newt_core::tenacity::cli_tenacity(),
            Some(newt_core::Tenacity::Relentless)
        );

        // Back to the UNPINNED tab: baseline, not B's pin.
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).expect("back to the unpinned tab");
        }
        assert_eq!(h.active_conversation_id, unpinned);
        assert!(
            std::env::var("NEWT_PROVIDER").is_err(),
            "an unpinned tab resolves to the baseline, never the other tab's backend"
        );
        assert_eq!(
            newt_core::tenacity::cli_tenacity(),
            None,
            "and never the other tab's dials"
        );
    }

    /// Dial change → immediate switch → return restores the change.
    #[test]
    fn a_dial_change_survives_a_switch_away_and_back() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol", "other"]);
        let a = h.active_conversation_id.clone();
        // A durable row so A's flush has somewhere to land.
        h.store.create_with_id(&a, "A", None).unwrap();
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 1).unwrap();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }

        // Operator turns a dial on A, then leaves immediately.
        newt_core::runtime::mark_tenacity_choice(Some(newt_core::Tenacity::Relentless));
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 1).expect("switch away");
        }
        assert_eq!(
            newt_core::tenacity::cli_tenacity(),
            None,
            "B is unpinned, so it shows the baseline — not A's change"
        );
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).expect("switch back");
        }
        assert_eq!(
            newt_core::tenacity::cli_tenacity(),
            Some(newt_core::Tenacity::Relentless),
            "the dial change was flushed on deactivation and comes back with A"
        );
    }

    /// The ADR's #1668 contract test: **two tabs with DIFFERENT pins**.
    ///
    /// The unpinned-vs-pinned case above proves a tab does not inherit; this
    /// proves the stronger thing — two tabs each carrying their own pin swap
    /// the whole route quintet and both dials cleanly, in both directions, with
    /// no residue from the other.
    #[test]
    fn two_tabs_with_different_pins_swap_cleanly_in_both_directions() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol", "other"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();
        h.store
            .update_preference_pin(
                &a,
                &newt_core::OperatorPreferencePin {
                    backend: Some("sol".into()),
                    tenacity: Some(newt_core::Tenacity::Relentless),
                    ..Default::default()
                },
            )
            .unwrap();
        let b = h.durable("B");
        h.store
            .update_preference_pin(
                &b,
                &newt_core::OperatorPreferencePin {
                    backend: Some("other".into()),
                    cognition: Some("contemplating".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        h.open_tab_on(&mut tabs, &b);

        let posture = |h: &Harness| {
            (
                std::env::var("NEWT_PROVIDER").ok(),
                h.choice.name.clone(),
                h.inf_url.clone(),
                h.inf_model.clone(),
                h.inf_kind,
                newt_core::cognition::cli_cognition(),
                newt_core::tenacity::cli_tenacity(),
            )
        };

        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }
        let on_a = posture(&h);
        assert_eq!(on_a.0.as_deref(), Some("sol"));
        assert_eq!(on_a.6, Some(newt_core::Tenacity::Relentless));
        assert_eq!(
            newt_core::cognition::cli_cognition(),
            newt_core::cognition::CognitionOverride::Unset,
            "A pins no cognition, so it must show the baseline — not B's"
        );

        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 1).unwrap();
        }
        let on_b = posture(&h);
        assert_eq!(on_b.0.as_deref(), Some("other"));
        assert_eq!(
            on_b.6, None,
            "B pins no tenacity, so A's Relentless must NOT survive the switch"
        );

        // And back — history-independent, so A's posture is byte-identical.
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }
        assert_eq!(
            posture(&h),
            on_a,
            "A→B→A restores A's whole posture exactly"
        );
    }

    /// The outgoing tab keeps its claim across a switch — that is what makes it
    /// a tab rather than a replaced conversation. Only close and exit release.
    #[test]
    fn the_outgoing_tabs_claim_is_held_across_a_switch() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
            activate_tab(&mut ctx, &mut tabs, 1).unwrap();
        }
        // Both conversations are still this session's, and both tabs still name
        // them — a switch released nothing.
        let mut held = tabs.claimed_conversations();
        held.sort_unstable();
        let mut expected = vec![a.as_str(), b.as_str()];
        expected.sort_unstable();
        assert_eq!(held, expected, "both claims survive the switch");
    }

    /// Close is release-WITHOUT-end: the conversation stays open and resumable.
    #[test]
    fn closing_a_tab_leaves_its_conversation_resumable() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
            close_tab(&mut ctx, &mut tabs, 1).unwrap();
        }
        assert!(
            h.store.exists(&b).unwrap(),
            "close must not end the conversation — /end is that verb"
        );
        // Released, so it can be claimed and resumed again.
        assert!(matches!(
            h.store.claim(&b).unwrap(),
            newt_core::ClaimOutcome::Claimed
        ));
        assert!(
            tabs.find_by_conversation(&b).is_none(),
            "and no tab still names it"
        );
    }

    // ── 5-7. duplicate conversations are impossible ───────────────────────

    /// Blocker 2, front door one: `/resume`'s target.
    #[test]
    fn resume_of_a_conversation_open_elsewhere_activates_that_tab() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let a = h.active_conversation_id.clone();
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }

        let adopted = {
            let mut ctx = h.ctx();
            adopt_conversation(&mut ctx, &mut tabs, &b).unwrap()
        };
        assert!(
            matches!(adopted, Adopted::ActivatedExistingTab { index: 1, .. }),
            "got {adopted:?}"
        );
        assert_eq!(h.active_conversation_id, b);
        assert_eq!(tabs.len(), 2, "no second tab was opened for the same row");
        assert_eq!(h.active_conversation_id, tabs.active().conversation_id());
        assert_ne!(a, b);
    }

    /// Blocker 2, front door two: the same conversation reached through the
    /// roadmap/`/conversation restore` path. Both call the same seam, so this
    /// proves the invariant is a property of the SEAM, not of one command.
    #[test]
    fn a_second_front_door_also_activates_rather_than_duplicating() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }
        // Whatever the caller is, it asks the seam.
        for _ in 0..2 {
            let adopted = {
                let mut ctx = h.ctx();
                adopt_conversation(&mut ctx, &mut tabs, &b).unwrap()
            };
            assert!(matches!(
                adopted,
                Adopted::ActivatedExistingTab { .. } | Adopted::AlreadyHere
            ));
        }
        assert_eq!(tabs.len(), 2);
    }

    /// The invariant itself: no two tabs may name the same conversation.
    ///
    /// Note this cannot be delegated to the store — `claim` returns `Claimed`
    /// for a re-affirmation by the SAME process, so a second tab would be told
    /// its duplicate claim succeeded.
    #[test]
    fn two_tabs_can_never_hold_the_same_conversation() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let shared = h.durable("shared");
        // The store happily re-affirms our own claim — this is the trap.
        assert!(matches!(
            h.store.claim(&shared).unwrap(),
            newt_core::ClaimOutcome::Claimed
        ));
        assert!(matches!(
            h.store.claim(&shared).unwrap(),
            newt_core::ClaimOutcome::Claimed
        ));

        h.open_tab_on(&mut tabs, &shared);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }
        let adopted = {
            let mut ctx = h.ctx();
            adopt_conversation(&mut ctx, &mut tabs, &shared).unwrap()
        };
        assert!(matches!(adopted, Adopted::ActivatedExistingTab { .. }));

        let mut ids: Vec<&str> = tabs.claimed_conversations();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(
            ids.len(),
            before,
            "every open tab names a DISTINCT conversation"
        );
    }

    // ── 8-10. failure containment ─────────────────────────────────────────

    /// Stage-0 failure is non-mutating: a tab whose conversation cannot be
    /// preflighted leaves the session exactly as it was.
    #[test]
    fn a_failed_activation_leaves_the_current_tab_untouched() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }
        h.input_stash = "typed in A".into();
        h.turns_this_conversation = 3;
        let before = h.snapshot(&tabs);
        let active_before = tabs.active_index();
        let owner_before = newt_core::lifecycle::active_session();

        // An out-of-range target is refused at Stage 0, before anything moves.
        let err = {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 9).unwrap_err()
        };
        assert!(matches!(err, TabError::OutOfRange { .. }));
        assert_eq!(
            h.snapshot(&tabs),
            before,
            "a refused switch mutates nothing"
        );
        assert_eq!(tabs.active_index(), active_before);
        assert_eq!(newt_core::lifecycle::active_session(), owner_before);
    }

    /// **P0 — Stage 0 is a transaction, not an ordering.**
    ///
    /// The row `load()`s cleanly and a LATER validation — the prompt receipt's
    /// context — fails. Under the previous shape, preflight proved only
    /// `exists + load`, so this failure surfaced INSIDE the restore, after the
    /// outgoing tab had been deactivated and the active pointer moved; the
    /// hydrate then logged a warning and carried on, leaving the session with
    /// one conversation's memory under another's identity.
    ///
    /// Asserts the whole state surface is byte-identical: active index,
    /// lifecycle owner, conversation id, claims, memory/system, persona,
    /// scratchpad, plan, prompt context, the backend quintet, projected dials,
    /// sidecar, and input.
    #[test]
    fn a_late_validation_failure_leaves_the_session_completely_untouched() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol", "other"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).expect("settle on A");
        }
        // Give A a full, distinguishable live state.
        {
            use newt_core::ScratchpadStore;
            h.scratchpad.set("current_task", "A's task".into());
        }
        h.turns_this_conversation = 6;
        h.active_roadmap_id = Some("road-a".into());
        h.last_resume_listing = vec!["row-1".into()];
        h.input_stash = "half typed in A".into();

        let before = h.snapshot(&tabs);
        let active_before = tabs.active_index();
        let owner_before = newt_core::lifecycle::active_session();
        let claims_before = {
            let mut c = tabs.claimed_conversations();
            c.sort_unstable();
            c.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        };
        // The row loads; the receipt context does not validate.
        assert!(h.store.exists(&b).unwrap(), "B's row exists and loads");
        crate::restore_prepare_seam::fail_after_load_for(&b);

        let err = {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 1).unwrap_err()
        };
        crate::restore_prepare_seam::clear();

        assert!(
            matches!(err, TabError::PreflightFailed { .. }),
            "a late validation failure must abort at Stage 0, got {err:?}"
        );
        assert_eq!(
            h.snapshot(&tabs),
            before,
            "no field of the live session may change on a refused switch"
        );
        assert_eq!(tabs.active_index(), active_before, "active tab unchanged");
        assert_eq!(
            newt_core::lifecycle::active_session(),
            owner_before,
            "lifecycle ownership did not move"
        );
        let mut claims_after = tabs.claimed_conversations();
        claims_after.sort_unstable();
        assert_eq!(
            claims_after
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            claims_before,
            "no claim was taken or released"
        );
        // And the session is still usable: switching to a HEALTHY tab works.
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).expect("A is still activatable");
        }
    }

    /// The same failure reached through the ADOPTION seam rather than `/tab <n>`,
    /// because a transaction that only holds for one front door is not a
    /// transaction.
    #[test]
    fn a_late_validation_failure_through_adoption_also_changes_nothing() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }
        let before = h.snapshot(&tabs);
        let owner_before = newt_core::lifecycle::active_session();

        crate::restore_prepare_seam::fail_after_load_for(&b);
        let outcome = {
            let mut ctx = h.ctx();
            adopt_conversation(&mut ctx, &mut tabs, &b)
        };
        crate::restore_prepare_seam::clear();

        assert!(outcome.is_err(), "adoption must propagate the abort");
        assert_eq!(h.snapshot(&tabs), before);
        assert_eq!(newt_core::lifecycle::active_session(), owner_before);
        assert_eq!(tabs.active_index(), 0);
    }

    /// Closing the ACTIVE tab activates the neighbor BEFORE the closing tab's
    /// claim is released — never the other way round.
    #[test]
    fn closing_the_active_tab_activates_the_neighbor_before_releasing() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let a = h.active_conversation_id.clone();
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        // active = B (index 1); close it.
        let closed = {
            let mut ctx = h.ctx();
            close_tab(&mut ctx, &mut tabs, 1).expect("close the active tab")
        };
        assert_eq!(closed.tab.conversation_id(), b);
        assert_eq!(tabs.len(), 1);
        assert_eq!(
            h.active_conversation_id, a,
            "the neighbor is live before anything is released"
        );
        assert_eq!(
            newt_core::lifecycle::active_session().as_deref(),
            Some(tabs.active().session_id().as_str()),
            "ownership names a tab that is still open"
        );
        // Release-without-end: the conversation is still resumable.
        assert!(
            h.store.exists(&b).unwrap(),
            "closing a tab must not end its conversation"
        );
    }

    /// Blocker 3, the load-bearing case: if the NEIGHBOR cannot be activated,
    /// closing the active tab must leave the session operational on the tab the
    /// operator tried to close — not half-torn-down between the two.
    ///
    /// Before the transactional rewrite, close removed the tab from the set and
    /// *then* hydrated the neighbor, so this failure left the closed tab gone,
    /// the neighbor partially hydrated, and ownership already transferred.
    #[test]
    fn a_failed_neighbor_activation_leaves_the_closing_tab_intact() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        // Land on B (index 1) — the tab we will try to close.
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 1).unwrap();
        }
        h.input_stash = "typed in B".into();
        h.turns_this_conversation = 5;
        let before = h.snapshot(&tabs);
        let owner_before = newt_core::lifecycle::active_session();

        // A (the neighbor) becomes unactivatable.
        test_seam::fail_preflight_for(&a);
        let err = {
            let mut ctx = h.ctx();
            close_tab(&mut ctx, &mut tabs, 1).unwrap_err()
        };
        test_seam::clear();

        assert!(matches!(err, TabError::PreflightFailed { .. }), "{err:?}");
        assert_eq!(tabs.len(), 2, "nothing was removed");
        assert_eq!(tabs.active_index(), 1, "B is still the active tab");
        assert_eq!(h.snapshot(&tabs), before, "B's live state is untouched");
        assert_eq!(
            newt_core::lifecycle::active_session(),
            owner_before,
            "ownership did not transfer on a failed close"
        );
        assert!(
            h.store.exists(&b).unwrap(),
            "and B's conversation was never released or ended"
        );
    }

    /// Closing one tab must not release a claim another tab still needs.
    #[test]
    fn closing_one_tab_cannot_release_another_tabs_claim() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let a = h.active_conversation_id.clone();
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }
        // Close the INACTIVE tab B while A is live.
        {
            let mut ctx = h.ctx();
            close_tab(&mut ctx, &mut tabs, 1).unwrap();
        }
        assert_eq!(tabs.claimed_conversations(), vec![a.as_str()]);
        // A's claim is intact: re-claiming it still succeeds for this process
        // and no other tab lost anything.
        assert!(matches!(
            h.store.claim(&a).unwrap(),
            newt_core::ClaimOutcome::Claimed
        ));
    }

    // ── 11-12. pin failure is explicit and refuses turns ──────────────────

    /// Blocker 4. A tab pinned to a backend this process cannot resolve lands
    /// at baseline, is marked degraded, and the marker retains the pin so a
    /// retry is possible.
    #[test]
    fn an_unestablishable_pin_degrades_the_tab_instead_of_running_at_baseline() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let b = h.durable("B");
        h.store.claim(&b).unwrap();
        h.store
            .update_preference_pin(
                &b,
                &newt_core::OperatorPreferencePin {
                    // Not in `cfg.backends` — the config no longer defines it.
                    backend: Some("a-backend-that-is-gone".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }

        let switched = {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 1).expect("activation itself succeeds")
        };
        let degraded = switched
            .degraded
            .expect("a pin that cannot be established must degrade the tab");
        assert!(
            degraded.summary().starts_with("!pin"),
            "visible marker: {}",
            degraded.summary()
        );
        assert_eq!(
            degraded.pin.backend.as_deref(),
            Some("a-backend-that-is-gone"),
            "the pin is retained so a retry has something to retry"
        );
        assert_eq!(
            tabs.active().pin_degraded.as_ref().map(|d| d.summary()),
            Some(degraded.summary()),
            "the tab carries the degraded state, which is what refuses turns"
        );
        assert!(
            std::env::var("NEWT_PROVIDER").is_err(),
            "and the session sits at a KNOWN baseline, not a half-applied pin"
        );
    }

    /// The retry path: once the backend exists again, `/tab retry` clears it.
    #[test]
    fn a_degraded_tab_can_retry_once_the_pin_is_satisfiable() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let b = h.durable("B");
        h.store.claim(&b).unwrap();
        h.store
            .update_preference_pin(
                &b,
                &newt_core::OperatorPreferencePin {
                    backend: Some("later".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
            activate_tab(&mut ctx, &mut tabs, 1).unwrap();
        }
        assert!(tabs.active().pin_degraded.is_some());

        // The operator adds the backend the pin names, then retries.
        h.cfg = cfg_with(&["sol", "later"]);
        {
            let mut ctx = h.ctx();
            handle_tab_action(TabAction::Retry, &mut ctx, &mut tabs);
        }
        assert!(
            tabs.active().pin_degraded.is_none(),
            "a satisfiable pin clears the degraded state"
        );
        assert_eq!(
            std::env::var("NEWT_PROVIDER").ok().as_deref(),
            Some("later"),
            "and the pin is now actually in force"
        );
    }

    // ── P1: transition results and degraded visibility ───────────────────

    /// P1-3. A tab pinned to endpoint X, then `/tab new` onto the baseline
    /// endpoint Y, is an ENDPOINT CHANGE. `create_fresh_tab` discarded its
    /// `PinRestore`, so `/tab new` reported `url_changed = false` and telemetry
    /// kept pointing at the box the previous tab was talking to.
    /// Runs on a runtime because a REAL endpoint change is what we are proving,
    /// and `refresh_backend` fires its served-adoption probe on one — that probe
    /// is the "normal reprobe path" the brief asks this to exercise. The hosts
    /// are unresolvable, so it fails fast without touching a network.
    #[tokio::test(flavor = "multi_thread")]
    async fn tab_new_from_a_pinned_tab_reports_the_endpoint_change() {
        let _g = guard();
        // Two DIFFERENT endpoints, so a route change is a real url change.
        let mut cfg = cfg_with(&["sol", "other"]);
        cfg.backends[1].endpoint = "http://elsewhere.test:2".to_string();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        h.cfg = cfg;
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();
        h.store
            .update_preference_pin(
                &a,
                &newt_core::OperatorPreferencePin {
                    backend: Some("other".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        // Land on A so its pin is in force at endpoint Y.
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }
        assert_eq!(h.inf_url, "http://elsewhere.test:2", "A is pinned away");

        // `/tab new` returns to the baseline endpoint — that IS a url change.
        let outcome = {
            let mut ctx = h.ctx();
            create_fresh_tab(&mut ctx, &mut tabs).unwrap()
        };
        assert_eq!(
            h.inf_url, "http://backend.test:1",
            "fresh tab is at baseline"
        );
        assert!(
            outcome.url_changed,
            "a fresh tab that moves the endpoint must report it so telemetry re-probes"
        );
    }

    /// P1-4. Adopting an already-open DEGRADED tab must report `!pin`
    /// immediately — the operator must not first learn of it from a refused
    /// turn.
    #[test]
    fn adopting_a_degraded_tab_reports_the_degradation_in_its_outcome() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();
        let b = h.durable("B");
        h.store
            .update_preference_pin(
                &b,
                &newt_core::OperatorPreferencePin {
                    backend: Some("a-backend-that-is-gone".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }

        let adopted = {
            let mut ctx = h.ctx();
            adopt_conversation(&mut ctx, &mut tabs, &b).unwrap()
        };
        match adopted {
            Adopted::ActivatedExistingTab { index, outcome } => {
                assert_eq!(index, 1);
                let d = outcome
                    .degraded
                    .expect("adoption must carry the degradation, not drop it");
                assert!(d.summary().starts_with("!pin"), "{}", d.summary());
            }
            other => panic!("expected an activation, got {other:?}"),
        }
        assert!(
            tabs.active().pin_degraded.is_some(),
            "and the tab itself is marked, which is what refuses turns"
        );
    }

    /// P1-4. Closing into a degraded NEIGHBOR reports it too.
    #[test]
    fn closing_into_a_degraded_neighbor_reports_the_degradation() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();
        h.store
            .update_preference_pin(
                &a,
                &newt_core::OperatorPreferencePin {
                    backend: Some("also-gone".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        // active = B (index 1); close it, landing on the degraded A.
        let closed = {
            let mut ctx = h.ctx();
            close_tab(&mut ctx, &mut tabs, 1).unwrap()
        };
        assert_eq!(closed.tab.conversation_id(), b);
        let d = closed
            .outcome
            .degraded
            .expect("close must carry the neighbor's degradation");
        assert!(d.summary().starts_with("!pin"), "{}", d.summary());
        assert!(tabs.active().pin_degraded.is_some());
    }

    // ── ADR slice tests the audit found missing or vacuous ────────────────

    /// S7 — exit flushes the active tab AND releases every claim.
    ///
    /// The flush half did not exist: exit released claims but never deactivated
    /// the active tab, so a `/model` or `/psyche` change made after the last
    /// turn was dropped on the way out. Every other way of leaving a tab
    /// flushes; leaving newt did not.
    #[test]
    fn exit_flushes_the_active_tab_and_releases_every_claim() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol", "other"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();
        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).unwrap();
        }
        // An operator choice made after the last turn, not yet written.
        newt_core::runtime::mark_backend_pick("other");
        h.pending = newt_core::runtime::drain_preference_actions();
        assert!(!h.pending.is_empty(), "fixture: an unwritten action exists");

        let released = {
            let mut ctx = h.ctx();
            exit_release_all(&mut ctx, &mut tabs)
        };

        assert_eq!(
            h.store.preference_pin(&a).unwrap().and_then(|p| p.backend),
            Some("other".to_string()),
            "the active tab's last choice is flushed on exit, not dropped"
        );
        let mut expect = vec![a.clone(), b.clone()];
        expect.sort();
        let mut got = released;
        got.sort();
        assert_eq!(
            got, expect,
            "every open tab's claim is released, not just the active one"
        );
    }

    /// S9 — the lean / ephemeral / no-store refusals, now a tested value rather
    /// than a string literal inside an 8000-line function.
    #[test]
    fn the_tab_surface_refusals_are_explicit_and_ordered() {
        // Lean wins outright: it is a surface fact, true regardless of storage.
        let lean = tab_surface_refusal(false, false, true).expect("lean refuses");
        assert!(lean.contains("rich-TUI"), "{lean}");
        assert!(
            lean.contains("/resume"),
            "names the lean equivalent: {lean}"
        );
        // Ephemeral next.
        let eph = tab_surface_refusal(true, true, true).expect("ephemeral refuses");
        assert!(eph.contains("ephemeral"), "{eph}");
        // No store at all.
        let none = tab_surface_refusal(true, false, false).expect("a storeless session refuses");
        assert!(none.contains("no store"), "{none}");
        // And the one case that proceeds.
        assert!(tab_surface_refusal(true, false, true).is_none());
        // Lean + ephemeral reports the SURFACE reason — the operator cannot fix
        // ephemerality into tabs on a lean surface, so naming lean is the
        // actionable message.
        assert!(tab_surface_refusal(false, true, true)
            .expect("still refuses")
            .contains("rich-TUI"));
    }

    /// S3b — the turn refusal itself, which the audit found asserted only by a
    /// comment. The message must name the recovery path, because the operator's
    /// only way out is a command.
    #[test]
    fn a_degraded_tab_refuses_the_turn_and_names_the_way_out() {
        let clean = degraded_turn_refusal(None);
        assert!(clean.is_none(), "a healthy tab does not refuse");

        let degraded = crate::PinDegraded {
            reasons: vec!["pinned backend `gone` is not configured".into()],
            pin: newt_core::OperatorPreferencePin {
                backend: Some("gone".into()),
                ..Default::default()
            },
        };
        let msg = degraded_turn_refusal(Some(&degraded)).expect("a degraded tab refuses");
        assert!(msg.starts_with("!pin"), "visible marker first: {msg}");
        assert!(
            msg.contains("was not accepted"),
            "the contract is that the PROMPT is not accepted, not merely unsent: {msg}"
        );
        assert!(
            msg.contains("/tab retry"),
            "names the recovery command: {msg}"
        );
    }

    /// S8 — `/resume` onto a conversation NOT open in any tab stays a resume:
    /// the seam hands it back rather than converting it into an activation.
    ///
    /// The activation branch is covered elsewhere; this is the opposite
    /// property, and the regression it guards is exactly "someone simplifies
    /// resume and activate into one operation".
    #[test]
    fn resume_of_an_unopened_conversation_stays_a_resume() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();
        // A durable conversation that NO tab holds.
        let elsewhere = h.durable("not open anywhere");
        let before = h.snapshot(&tabs);

        let adopted = {
            let mut ctx = h.ctx();
            adopt_conversation(&mut ctx, &mut tabs, &elsewhere).unwrap()
        };
        assert!(
            matches!(adopted, Adopted::ProceedInActiveTab),
            "the seam must hand an unopened conversation back to the resume verb, \
             not activate it: got {adopted:?}"
        );
        assert_eq!(
            h.snapshot(&tabs),
            before,
            "and the seam itself mutates nothing — resume's sparse overlay runs at the caller"
        );
        assert_eq!(tabs.len(), 1, "no tab was opened for it either");
    }

    /// S1 — `interrupted_objective` really is per-tab. The audit found the
    /// existing coverage vacuous: no test ever set it to `Some(..)`, so it rode
    /// on a `TabSidecar::default()` comparison that could not fail.
    #[test]
    fn an_interrupted_objective_does_not_leak_into_another_tab() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol"]);
        let a = h.active_conversation_id.clone();
        h.store.create_with_id(&a, "A", None).unwrap();
        // A REAL prompt context, from a real receipt.
        let receipt = h
            .store
            .begin_prompt(
                &a,
                "A",
                None,
                newt_core::NewPrompt::operator(b"objective".to_vec(), b"objective".to_vec()),
            )
            .unwrap();
        h.interrupted_objective = Some(receipt);
        assert!(h.interrupted_objective.is_some(), "fixture precondition");

        let b = h.durable("B");
        h.open_tab_on(&mut tabs, &b);
        assert!(
            h.interrupted_objective.is_none(),
            "tab A's interrupted objective must not follow into B — otherwise B's bare \
             `continue` would silently resume A's work"
        );

        {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 0).expect("back to A");
        }
        assert!(
            h.interrupted_objective.is_some(),
            "and it comes back with A, which is why it is stashed rather than dropped"
        );
    }

    /// P1 — activation is history-independent over the PROJECTED posture, not
    /// merely over the tab model. `A→B` and `C→B` must land byte-identical.
    #[test]
    fn activation_is_history_independent_over_the_full_projected_state() {
        let _g = guard();
        let build = || {
            let (mut h, mut tabs) = Harness::new(&["sol", "other"]);
            let a = h.active_conversation_id.clone();
            h.store.create_with_id(&a, "A", None).unwrap();
            h.store
                .update_preference_pin(
                    &a,
                    &newt_core::OperatorPreferencePin {
                        backend: Some("other".into()),
                        tenacity: Some(newt_core::Tenacity::Relentless),
                        ..Default::default()
                    },
                )
                .unwrap();
            let b = h.durable("B");
            h.store
                .update_preference_pin(
                    &b,
                    &newt_core::OperatorPreferencePin {
                        backend: Some("sol".into()),
                        cognition: Some("contemplating".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            let c = h.durable("C");
            h.open_tab_on(&mut tabs, &b);
            h.open_tab_on(&mut tabs, &c);
            (h, tabs)
        };

        // Reached from A (index 0).
        let (mut from_a, mut tabs_a) = build();
        {
            let mut ctx = from_a.ctx();
            activate_tab(&mut ctx, &mut tabs_a, 0).unwrap();
            activate_tab(&mut ctx, &mut tabs_a, 1).unwrap();
        }
        let via_a = from_a.snapshot(&tabs_a);

        // Reached from C (index 2).
        let (mut from_c, mut tabs_c) = build();
        {
            let mut ctx = from_c.ctx();
            activate_tab(&mut ctx, &mut tabs_c, 2).unwrap();
            activate_tab(&mut ctx, &mut tabs_c, 1).unwrap();
        }
        let via_c = from_c.snapshot(&tabs_c);

        // Conversation ids differ per harness (fresh tempdirs), so compare
        // everything the ADR promises is history-independent.
        assert_eq!(via_a.provider, via_c.provider);
        assert_eq!(via_a.model, via_c.model);
        assert_eq!(via_a.cognition, via_c.cognition);
        assert_eq!(via_a.tenacity, via_c.tenacity);
        assert_eq!(via_a.persona_cognition, via_c.persona_cognition);
        assert_eq!(via_a.inf_url, via_c.inf_url);
        assert_eq!(via_a.inf_model, via_c.inf_model);
        assert_eq!(via_a.inf_kind, via_c.inf_kind);
        assert_eq!(via_a.choice_name, via_c.choice_name);
        assert_eq!(via_a.inf_context_window, via_c.inf_context_window);
        assert_eq!(via_a.persona, via_c.persona);
        assert_eq!(via_a.scratchpad, via_c.scratchpad);
        assert_eq!(via_a.plan_steps, via_c.plan_steps);
        assert_eq!(via_a.turns, via_c.turns);
        assert_eq!(via_a.roadmap, via_c.roadmap);
        assert_eq!(via_a.input_stash, via_c.input_stash);
        assert_eq!(via_a.degraded, via_c.degraded);
        assert_eq!(
            via_a.cognition,
            newt_core::cognition::CognitionOverride::Set(
                newt_core::role_profile::Cognition::Contemplating
            ),
            "B's own pin is in force either way"
        );
        assert_eq!(
            via_a.tenacity, None,
            "and A's tenacity never survives into B, from either predecessor"
        );
    }

    // ── 13. authority/security state is untouched by tab machinery ────────

    /// The ADR's security invariant — measured, not argued.
    ///
    /// Two independent proofs, because "authority never migrates via tab
    /// machinery" deserves better than a comment:
    ///
    /// 1. **By construction.** `TabSwitchCtx` has no authority, permission, or
    ///    credential field. `active_posture` is a `run_chat` local the switch
    ///    machinery cannot name, so no activation *can* mutate it — the code
    ///    that would do so does not compile.
    /// 2. **By measurement.** A pin names backends and dials ONLY, so applying
    ///    one must leave every authority-relevant process global bit-identical.
    ///    This walks a full switch sequence and compares them exactly.
    #[test]
    fn authority_state_is_bit_identical_across_any_switch_sequence() {
        let _g = guard();
        let (mut h, mut tabs) = Harness::new(&["sol", "other"]);
        let b = h.durable("B");
        h.store.claim(&b).unwrap();
        h.store
            .update_preference_pin(
                &b,
                &newt_core::OperatorPreferencePin {
                    backend: Some("other".into()),
                    cognition: Some("contemplating".into()),
                    tenacity: Some(newt_core::Tenacity::Relentless),
                    model: Some("m0".into()),
                },
            )
            .unwrap();
        h.open_tab_on(&mut tabs, &b);

        // Everything authority-shaped that a switch could plausibly disturb.
        // Deliberately exact comparison — no "close enough" for security state.
        let authority = || {
            (
                std::env::var("NEWT_TEAM").ok(),
                std::env::var(newt_core::denial_journal::DENIAL_JOURNAL_PATH_ENV).ok(),
                std::env::var("NEWT_POSTURE").ok(),
                std::env::var("NEWT_EPHEMERAL").ok(),
            )
        };
        let before = authority();

        for target in [0usize, 1, 0, 1, 0] {
            let mut ctx = h.ctx();
            let _ = activate_tab(&mut ctx, &mut tabs, target);
            assert_eq!(
                authority(),
                before,
                "no tab switch may widen or narrow what the agent is allowed to do"
            );
        }

        // And the pin itself cannot carry authority: its axes are exactly the
        // four preference axes, so there is nothing security-shaped to migrate.
        let pin = h.store.preference_pin(&b).unwrap().unwrap();
        let newt_core::OperatorPreferencePin {
            backend: _,
            model: _,
            cognition: _,
            tenacity: _,
        } = pin;
    }
}
