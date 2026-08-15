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

use crate::tabs::{Closed, OwnerHandoff, TabError, TabSet};

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
#[derive(Debug)]
pub(crate) struct Switched {
    /// Whether the backend endpoint moved, so the caller re-probes DGX
    /// telemetry exactly as every other backend switch does.
    pub url_changed: bool,
    /// `Some` when the activated tab's pin could not be established, so it is
    /// running at baseline and must refuse turns until resolved.
    pub degraded: Option<crate::PinDegraded>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Incoming {
    /// A durable row exists and reads cleanly.
    Materialized,
    /// Claimed, no row yet: activation resets to a clean conversation under
    /// that id rather than loading one.
    Fresh,
}

/// Stage 0 — prove the incoming conversation can be activated, mutating nothing.
///
/// A store error is NOT "absent" (the #1030 hazard): a transient SQLITE_BUSY or
/// NFS IO error must abort the switch, never be mistaken for a fresh tab and
/// silently reset a conversation that actually has content.
fn preflight(store: &newt_core::ConversationStore, id: &str) -> Result<Incoming, TabError> {
    #[cfg(test)]
    if let Some(reason) = test_seam::forced_failure(id) {
        return Err(TabError::PreflightFailed { reason });
    }
    match store.exists(id) {
        Ok(true) => match store.load(id) {
            Ok(_) => Ok(Incoming::Materialized),
            Err(e) => Err(TabError::PreflightFailed {
                reason: format!("its conversation could not be read ({e})"),
            }),
        },
        Ok(false) => Ok(Incoming::Fresh),
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

    /// Stages 3–5 for whichever tab is now active.
    fn hydrate(
        &mut self,
        tabs: &TabSet,
        incoming_id: &str,
        incoming: Incoming,
    ) -> crate::PinRestore {
        match incoming {
            Incoming::Materialized => {
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
                if let Ok((_, Some(w))) =
                    crate::restore_conversation_into_session(&mut restore, incoming_id)
                {
                    crate::print_newt(&format!("warning: {w}"), self.color, self.verbose);
                }
            }
            Incoming::Fresh => {
                *self.active_conversation_id = incoming_id.to_string();
                self.reset_to_clean_conversation();
            }
        }
        let restored = self.reset_and_overlay();
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
) -> Result<Switched, TabError> {
    let incoming_id = tabs
        .get(target)
        .ok_or(TabError::OutOfRange { open: tabs.len() })?
        .conversation_id()
        .to_string();
    if target == tabs.active_index() {
        return Ok(Switched {
            url_changed: false,
            degraded: tabs.active().pin_degraded.clone(),
        });
    }
    let incoming = preflight(ctx.store, &incoming_id)?;

    ctx.deactivate(tabs);
    let handoff = tabs.activate(target)?;
    let restored = ctx.hydrate(tabs, &incoming_id, incoming);
    tabs.active_mut().pin_degraded = restored.degraded.clone();
    handoff.apply();
    Ok(Switched {
        url_changed: restored.url_changed,
        degraded: restored.degraded,
    })
}

/// **Create a fresh tab** — a genuinely new conversation, not a relabelled one.
///
/// The claim is taken BEFORE the outgoing tab is deactivated, so a refused
/// claim (vanishingly unlikely for a brand-new id, but not impossible) leaves
/// the session exactly where it was.
pub(crate) fn create_fresh_tab(
    ctx: &mut TabSwitchCtx<'_>,
    tabs: &mut TabSet,
) -> Result<OwnerHandoff, TabError> {
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
    ctx.deactivate(tabs);
    let (_, handoff) = tabs.open(newt_core::lifecycle::new_session_id(), &fresh);
    // A fresh tab is a NEW conversation: same reset `/new` performs, so it
    // cannot inherit conversation-shaped state from the tab it was opened
    // from. `Incoming::Fresh` routes through exactly that primitive.
    let restored = ctx.hydrate(tabs, &fresh, Incoming::Fresh);
    tabs.active_mut().pin_degraded = restored.degraded;
    handoff.apply();
    Ok(handoff)
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Adopted {
    /// It was already open in another tab; that tab was ACTIVATED. No second
    /// tab now points at it.
    ActivatedExistingTab { index: usize, url_changed: bool },
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
        let switched = activate_tab(ctx, tabs, index)?;
        return Ok(Adopted::ActivatedExistingTab {
            index,
            url_changed: switched.url_changed,
        });
    }
    if *ctx.active_conversation_id == target {
        return Ok(Adopted::AlreadyHere);
    }
    Ok(Adopted::ProceedInActiveTab)
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
) -> Result<Closed, TabError> {
    if target >= tabs.len() {
        return Err(TabError::OutOfRange { open: tabs.len() });
    }
    if tabs.len() == 1 {
        return Err(TabError::LastTab);
    }
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
        activate_tab(ctx, tabs, neighbor)?;
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
    Ok(closed)
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
) -> bool {
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
            false
        }
        TabAction::New => {
            match create_fresh_tab(ctx, tabs) {
                Ok(_) => crate::print_newt(
                    &format!("tab {} — new conversation", tabs.active_index() + 1),
                    color,
                    verbose,
                ),
                Err(e) => refuse(e, tabs),
            }
            // A fresh conversation resolves to the baseline backend, which the
            // reset already applied; no endpoint move to re-probe.
            false
        }
        TabAction::Retry => {
            // Re-run the reset ⊕ overlay for the active tab. Success clears the
            // degraded marker and turns are allowed again; failure re-reports,
            // so the operator can iterate without losing the conversation.
            let restored = ctx.reset_and_overlay();
            let still_degraded = restored.degraded.is_some();
            tabs.active_mut().pin_degraded = restored.degraded.clone();
            match restored.degraded {
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
            restored.url_changed
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
                    closed.handoff.is_some()
                }
                Err(e) => {
                    refuse(e, tabs);
                    false
                }
            }
        }
        TabAction::Move { from, to } => {
            match tabs.move_tab(from, to) {
                Ok(()) => list_tabs(ctx, tabs),
                Err(e) => refuse(e, tabs),
            }
            false
        }
        TabAction::Rename { index, title } => {
            let target = index.unwrap_or_else(|| tabs.active_index());
            match tabs.get(target).map(|t| t.conversation_id().to_string()) {
                Some(id) => {
                    let persona = ctx.active_persona.as_ref().map(|p| p.name.clone());
                    match crate::rename_conversation(ctx.store, &id, &title, persona.as_deref()) {
                        Ok(()) => crate::print_newt(
                            &format!("tab {} renamed to '{title}'", target + 1),
                            color,
                            verbose,
                        ),
                        Err(e) => crate::print_newt(&format!("rename failed: {e}"), color, verbose),
                    }
                    false
                }
                None => {
                    refuse(TabError::OutOfRange { open: tabs.len() }, tabs);
                    false
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
) -> bool {
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
            switched.url_changed
        }
        Err(e) => {
            refuse(e, tabs);
            false
        }
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
        crate::print_newt(
            &format!(
                "{marker} {}. {}{identity}",
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
            let incoming = preflight(ctx.store, id).expect("fixture row must preflight");
            ctx.hydrate(tabs, id, incoming);
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
        fn snapshot(&self) -> Snapshot {
            Snapshot {
                conversation_id: self.active_conversation_id.clone(),
                provider: std::env::var("NEWT_PROVIDER").ok(),
                model: std::env::var("NEWT_DGX_MODEL").ok(),
                cognition: newt_core::cognition::cli_cognition(),
                tenacity: newt_core::tenacity::cli_tenacity(),
                inf_url: self.inf_url.clone(),
                inf_model: self.inf_model.clone(),
                inf_kind: self.inf_kind,
                choice_name: self.choice.name.clone(),
                turns: self.turns_this_conversation,
                roadmap: self.active_roadmap_id.clone(),
                input_stash: self.input_stash.clone(),
                system: self.system.clone(),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Snapshot {
        conversation_id: String,
        provider: Option<String>,
        model: Option<String>,
        cognition: newt_core::cognition::CognitionOverride,
        tenacity: Option<newt_core::Tenacity>,
        inf_url: String,
        inf_model: String,
        inf_kind: newt_core::BackendKind,
        choice_name: String,
        turns: usize,
        roadmap: Option<String>,
        input_stash: String,
        system: String,
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
        let a_before = h.snapshot();

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
            h.snapshot(),
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
        let before = h.snapshot();
        let active_before = tabs.active_index();
        let owner_before = newt_core::lifecycle::active_session();

        // An out-of-range target is refused at Stage 0, before anything moves.
        let err = {
            let mut ctx = h.ctx();
            activate_tab(&mut ctx, &mut tabs, 9).unwrap_err()
        };
        assert!(matches!(err, TabError::OutOfRange { .. }));
        assert_eq!(h.snapshot(), before, "a refused switch mutates nothing");
        assert_eq!(tabs.active_index(), active_before);
        assert_eq!(newt_core::lifecycle::active_session(), owner_before);
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
        let before = h.snapshot();
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
        assert_eq!(h.snapshot(), before, "B's live state is untouched");
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
