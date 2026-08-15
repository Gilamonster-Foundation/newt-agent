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
pub(crate) struct Switched {
    /// Whether the backend endpoint moved, so the caller re-probes DGX
    /// telemetry exactly as every other backend switch does.
    pub url_changed: bool,
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
    fn reset_and_overlay(&mut self) -> bool {
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

/// Activate the tab at `target`, staged per the ADR.
///
/// Aborts before mutating anything if the incoming conversation cannot be read
/// (Stage 0). Activating the already-active tab is a no-op that still succeeds:
/// `/tab 1` while on tab 1 is not an error, and doing the full teardown would
/// pointlessly reset posture the operator did not ask to reset.
pub(crate) fn perform_tab_switch(
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
        return Ok(Switched { url_changed: false });
    }
    // Stage 0: prove the incoming row is readable BEFORE touching anything.
    // `restore_conversation_into_session` below would also fail, but only after
    // the outgoing tab had already been flushed and stashed.
    ctx.store
        .load(&incoming_id)
        .map_err(|_| TabError::OutOfRange { open: tabs.len() })?;

    ctx.deactivate(tabs);
    let handoff = tabs.activate(target)?;
    let url_changed = hydrate_active(ctx, tabs, &incoming_id);
    handoff.apply();
    Ok(Switched { url_changed })
}

/// Stages 3–5 against whichever tab is now active.
fn hydrate_active(ctx: &mut TabSwitchCtx<'_>, tabs: &TabSet, incoming_id: &str) -> bool {
    let mut restore = crate::ConversationCommandContext {
        store: ctx.store,
        persona_store: ctx.persona_store,
        workspace: ctx.workspace,
        memory: ctx.memory,
        system: ctx.system,
        active_persona: ctx.active_persona,
        active_conversation_id: ctx.active_conversation_id,
        compress_state: ctx.compress_state,
        scratchpad: ctx.scratchpad,
        step_ledger: ctx.step_ledger,
        active_prompt_context: ctx.active_prompt_context,
        mode_states: ctx.mode_states,
    };
    match crate::restore_conversation_into_session(&mut restore, incoming_id) {
        Ok((_, warning)) => {
            if let Some(w) = warning {
                crate::print_newt(&format!("warning: {w}"), ctx.color, ctx.verbose);
            }
        }
        Err(e) => {
            // Stage 0 proved the row readable, so this is a late failure. Say
            // so rather than pretending the switch was clean.
            crate::print_newt(
                &format!("warning: restoring tab state failed: {e}"),
                ctx.color,
                ctx.verbose,
            );
        }
    }
    let url_changed = ctx.reset_and_overlay();
    ctx.hydrate_sidecar(tabs);
    url_changed
}

/// Open a new tab on a fresh conversation and activate it.
///
/// The outgoing tab is deactivated (flush + stash) but **keeps its claim** —
/// that is what makes it a tab rather than a replaced conversation.
pub(crate) fn open_tab(
    ctx: &mut TabSwitchCtx<'_>,
    tabs: &mut TabSet,
    conversation_id: String,
) -> OwnerHandoff {
    ctx.deactivate(tabs);
    let (_, handoff) = tabs.open(newt_core::lifecycle::new_session_id(), &conversation_id);
    *ctx.active_conversation_id = conversation_id;
    // A fresh conversation has no pin, so the overlay is a no-op and this is
    // purely the reset half — the new tab starts at the invocation baseline
    // rather than inheriting the outgoing tab's dials.
    ctx.reset_and_overlay();
    ctx.hydrate_sidecar(tabs);
    handoff.apply();
    handoff
}

/// Close a tab. Closing the ACTIVE tab activates a neighbor first.
///
/// The order is the contract: the neighbor becomes the lifecycle owner
/// **before** the closed tab's claim is released, so there is never an instant
/// where the process owns no session or still names a tab that is gone.
///
/// Close is **release-without-end**: the conversation stays open and
/// `/resume`-able. `/end` remains the verb that ends a conversation.
pub(crate) fn close_tab(
    ctx: &mut TabSwitchCtx<'_>,
    tabs: &mut TabSet,
    target: usize,
) -> Result<Closed, TabError> {
    if tabs.len() == 1 {
        return Err(TabError::LastTab);
    }
    let closing_active = target == tabs.active_index();
    if closing_active {
        // Deactivate the tab we are about to drop, so its pin and sidecar are
        // flushed to its row before it leaves the bar.
        ctx.deactivate(tabs);
    }
    let closed = tabs.close(target)?;
    if let Some(handoff) = &closed.handoff {
        let neighbor = tabs.active().conversation_id().to_string();
        *ctx.active_conversation_id = neighbor.clone();
        hydrate_active(ctx, tabs, &neighbor);
        handoff.apply();
    }
    // Release the closed tab's claim LAST — after ownership moved.
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
    };

    match action {
        TabAction::List => {
            list_tabs(ctx, tabs);
            false
        }
        TabAction::New => {
            let fresh = newt_core::new_conversation_id();
            let _ = open_tab(ctx, tabs, fresh);
            crate::print_newt(
                &format!("tab {} — new conversation", tabs.active_index() + 1),
                color,
                verbose,
            );
            // A fresh conversation resolves to the baseline backend, which the
            // reset already applied; no endpoint move to re-probe.
            false
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
    match perform_tab_switch(ctx, tabs, target) {
        Ok(switched) => {
            crate::print_newt(
                &format!(
                    "tab {} — {}",
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
