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
    pub cfg: &'a newt_core::ResolvedConfig,
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
                // Built WITHOUT the stores: commit cannot perform a fallible
                // read because it has nothing to read from.
                let mut commit = crate::CommitContext {
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
                let (_, warning) = crate::commit_conversation_restore(&mut commit, *prepared);
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
#[path = "tab_switch_tests/state_machine_tests.rs"]
mod state_machine_tests;
