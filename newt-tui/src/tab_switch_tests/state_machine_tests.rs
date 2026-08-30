use super::*;
use crate::tabs::{TabAction, TabSet};
use newt_core::lifecycle::new_session_id;

/// Every backend shares one endpoint AND one model, so no route change ever
/// fires `refresh_backend`'s served-adoption probe — the tier stays
/// network-free while still exercising the real posture machinery.
fn cfg_with(names: &[&str]) -> newt_core::ResolvedConfig {
    newt_core::ResolvedConfig::unrequested(newt_core::Config {
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
    })
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
    cfg: newt_core::ResolvedConfig,
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
        let choice = crate::resolve_backend_choice(&cfg).expect("test configs resolve");
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
                persona_store: crate::PersonaStore::new(std::path::PathBuf::from("/nonexistent")),
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
                inf_model: choice.active_model.clone().unwrap_or_default(),
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

/// The seed's LIFETIME, which was previously only an argument: once the row
/// materializes, the seed is dropped and can never re-apply stale state.
///
/// The hazard it rules out: a tab whose row appeared (first prompt, or a
/// rename) still carrying a seed would, on its next activation, overwrite
/// the persona the store now owns with the one captured at `/tab new`.
/// Since `persona` is write-once at row birth, a stale re-apply would
/// disagree with the durable record permanently.
#[test]
fn the_seed_is_dropped_once_the_row_materializes_and_cannot_re_apply() {
    let _g = guard();
    let (mut h, mut tabs) = Harness::new(&["sol"]);
    let a = h.active_conversation_id.clone();
    h.store.create_with_id(&a, "A", None).unwrap();

    h.active_persona = Some(persona_named("seeded"));
    {
        let mut ctx = h.ctx();
        let _ = create_fresh_tab(&mut ctx, &mut tabs).unwrap();
    }
    let b = h.active_conversation_id.clone();
    assert!(tabs.active().fresh_seed.is_some(), "rowless: seed present");

    // The row materializes with a DIFFERENT persona than the seed captured
    // — as it would if the operator switched persona before the first turn.
    h.store.create_with_id(&b, "B", Some("durable")).unwrap();

    // Any switch away deactivates, which is where the seed is retired.
    {
        let mut ctx = h.ctx();
        activate_tab(&mut ctx, &mut tabs, 0).unwrap();
    }
    assert!(
        tabs.get(1).unwrap().fresh_seed.is_none(),
        "the seed is retired the moment the row exists"
    );

    // Coming back reads the STORE, not the stale seed.
    {
        let mut ctx = h.ctx();
        activate_tab(&mut ctx, &mut tabs, 1).unwrap();
    }
    assert_eq!(
        h.active_persona.as_ref().map(|p| p.name.as_str()),
        None,
        "the durable record owns the persona now — `durable` is not loadable from \
             this test's empty persona dir, so the restore reports no persona rather \
             than resurrecting the seed's `seeded`"
    );
    assert!(tabs.active().fresh_seed.is_none());
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
    let mut base = cfg_with(&["sol", "other"]).into_config();
    base.backends[1].endpoint = "http://elsewhere.test:2".to_string();
    let (mut h, mut tabs) = Harness::new(&["sol"]);
    h.cfg = newt_core::ResolvedConfig::unrequested(base);
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
