use super::*;
use newt_core::cognition::{cli_cognition, set_cli_cognition, CognitionOverride};
use newt_core::role_profile::Cognition;
use newt_core::runtime::{
    cli_preference_axes, mark_backend_pick, mark_cognition_choice, mark_model_pick,
    mark_tenacity_choice, record_cli_preference_axes,
};
use newt_core::tenacity::{clear_cli_tenacity, cli_tenacity, set_cli_tenacity, Tenacity};
use newt_core::test_guard::GlobalSettingsGuard;
use newt_core::{OperatorPreferencePin, PreferenceActions, PreferenceAxes};

/// Every backend shares one endpoint AND one model, so switching between
/// them never changes the resolved URL/model and therefore never fires
/// `refresh_backend`'s served-adoption probe — the unit tier stays
/// network-free. (Two named backends on one endpoint is a real
/// configuration: `sol` and `openai` both live on api.openai.com.) The
/// route is still observable: `NEWT_PROVIDER` and `choice.name` change.
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

fn store_in(root: &std::path::Path, ws: &std::path::Path) -> newt_core::ConversationStore {
    newt_core::ConversationStore::new(root, ws, 100).unwrap()
}

/// A durable conversation row (the pin write target) — the store creates
/// rows lazily on the first saved turn, so seed one turn.
fn durable_conversation(store: &newt_core::ConversationStore, title: &str) -> String {
    let id = store.create(title, None).unwrap();
    store.append_turn(&id, "q", "a").unwrap();
    id
}

/// The session locals a conversation switch mutates, so a test can drive
/// the REAL `restore_preference_pin` seam and then assert on them.
struct Session {
    base_provider: Option<String>,
    base_model: Option<String>,
    pending: PreferenceActions,
    choice: BackendChoice,
    inf_url: String,
    inf_model: String,
    inf_kind: newt_core::BackendKind,
    inf_key: Option<String>,
    inf_context_window: Option<u32>,
}

impl Session {
    /// Seeded to the CURRENT resolution so a same-target route never fires
    /// the served-adoption probe — the unit tier stays network-free.
    fn new(cfg: &newt_core::ResolvedConfig) -> Self {
        let choice = resolve_backend_choice(cfg).expect("test configs resolve");
        Self {
            base_provider: std::env::var("NEWT_PROVIDER").ok(),
            base_model: std::env::var("NEWT_DGX_MODEL").ok(),
            pending: PreferenceActions::default(),
            inf_url: choice.url.clone(),
            inf_model: choice.active_model.clone().unwrap_or_default(),
            inf_kind: choice.kind,
            inf_key: choice.api_key.clone(),
            inf_context_window: choice.context_window,
            choice,
        }
    }

    /// Switch this session onto `id` through the real seam.
    fn switch_to(
        &mut self,
        store: Option<&newt_core::ConversationStore>,
        id: &str,
        baseline: &PreferenceBaseline,
        cfg: &newt_core::ResolvedConfig,
    ) -> bool {
        self.switch_with_persona(store, id, baseline, cfg, None)
    }

    /// The startup half, through the REAL gate `run_chat` uses — so a test
    /// can drive a refused claim instead of hand-modelling its consequence
    /// (review-2 finding 6).
    fn switch_at_startup(
        &mut self,
        outcome: StartupConversation,
        store: Option<&newt_core::ConversationStore>,
        id: &str,
        baseline: &PreferenceBaseline,
        cfg: &newt_core::ResolvedConfig,
    ) -> bool {
        apply_startup_preference_pin(outcome, self.switch_args(store, id, baseline, cfg, None))
    }

    /// Switch with a persona active — the branch every earlier test left
    /// unexercised by always passing `persona: None` (review-2 finding 7),
    /// which is where the documented "a pin outranks the persona's declared
    /// backend" precedence actually lives.
    fn switch_with_persona(
        &mut self,
        store: Option<&newt_core::ConversationStore>,
        id: &str,
        baseline: &PreferenceBaseline,
        cfg: &newt_core::ResolvedConfig,
        persona: Option<&Persona>,
    ) -> bool {
        restore_preference_pin(self.switch_args(store, id, baseline, cfg, persona)).url_changed
    }

    fn switch_args<'a>(
        &'a mut self,
        store: Option<&'a newt_core::ConversationStore>,
        id: &'a str,
        baseline: &'a PreferenceBaseline,
        cfg: &'a newt_core::ResolvedConfig,
        persona: Option<&'a Persona>,
    ) -> ConversationPreferenceSwitch<'a> {
        ConversationPreferenceSwitch {
            store,
            conversation_id: id,
            baseline,
            persona,
            pending: &mut self.pending,
            base_provider: &mut self.base_provider,
            base_model: &mut self.base_model,
            cfg,
            choice: &mut self.choice,
            inf_url: &mut self.inf_url,
            inf_model: &mut self.inf_model,
            inf_kind: &mut self.inf_kind,
            inf_key: &mut self.inf_key,
            inf_context_window: &mut self.inf_context_window,
            color: false,
            verbose: false,
        }
    }

    /// One chat-loop iteration's drain — the real persistence seam.
    fn drain(&mut self, store: Option<&newt_core::ConversationStore>, id: &str) {
        persist_preference_actions(
            store,
            id,
            &mut self.pending,
            &mut self.base_provider,
            &mut self.base_model,
        )
        .expect("preference persistence must not fail in these tests");
    }
}

/// Clear every #1668 global a test's assertions depend on. The guard
/// restores them on drop; this makes the STARTING state explicit.
fn reset_globals() {
    let _ = newt_core::runtime::drain_preference_actions();
    set_cli_cognition(CognitionOverride::Unset);
    clear_cli_tenacity();
    newt_core::cognition::set_persona_cognition(None);
    newt_core::tenacity::set_persona_tenacity(None);
    // SAFETY: guarded single-threaded test (env restored on drop).
    unsafe {
        std::env::remove_var("NEWT_PROVIDER");
        std::env::remove_var("NEWT_DGX_MODEL");
    }
}

/// #1668 / review findings 1 + 7 + 8: a persona is active and routing the
/// backend, but the operator never acted — so nothing is pinned, turn
/// after turn. Under the old ambient capture, a bare `/backends` LISTING
/// (which refilled the operator baseline from the persona's env) put the
/// persona's backend into the pin permanently.
#[test]
fn a_persona_routed_session_with_no_operator_action_stays_unpinned() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = store_in(root.path(), ws.path());
    let id = durable_conversation(&store, "persona work");
    let cfg = cfg_with(&["sol", "muse"]);

    // The persona routes the backend and declares dials — ambient state a
    // per-turn snapshot would have swept up.
    // SAFETY: guarded single-threaded test.
    unsafe { std::env::set_var("NEWT_PROVIDER", "muse") };
    newt_core::cognition::set_persona_cognition(Some(Cognition::Contemplating));
    newt_core::tenacity::set_persona_tenacity(Some(Tenacity::Relentless));

    let mut session = Session::new(&cfg);
    // A bare `/backends` listing marks nothing (commands::model's listing
    // arm has no mark), and neither does a persona switch. Several turns:
    for _ in 0..3 {
        session.drain(Some(&store), &id);
    }
    assert_eq!(
        serde_json::to_string(&store.preference_pin(&id).unwrap().unwrap()).unwrap(),
        "{}",
        "a persona route + untouched dials must never pin"
    );
    assert_eq!(
        session.base_provider.as_deref(),
        Some("muse"),
        "the session's own baseline is unchanged by the drain"
    );
}

/// #1668 / review finding 7: `/model <name>` on a persona-routed backend
/// pins the MODEL axis only. The (provider, model) pair can never be
/// assembled from two independently-mutated sources, because each action
/// names exactly the axes it set.
#[test]
fn a_model_pick_on_a_persona_route_pins_the_model_axis_only() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = store_in(root.path(), ws.path());
    let id = durable_conversation(&store, "persona work");
    let cfg = cfg_with(&["sol", "muse"]);
    // SAFETY: guarded single-threaded test.
    unsafe { std::env::set_var("NEWT_PROVIDER", "muse") };

    let mut session = Session::new(&cfg);
    mark_model_pick("m9-turbo"); // what apply_model_choice marks past its gate
    session.drain(Some(&store), &id);

    let pin = store.preference_pin(&id).unwrap().unwrap();
    assert_eq!(pin.model.as_deref(), Some("m9-turbo"));
    assert_eq!(
        pin.backend, None,
        "the persona's route must not ride along as an operator pin"
    );
    assert_eq!(
        session.base_provider.as_deref(),
        Some("muse"),
        "a model pick leaves the provider baseline alone"
    );
    assert_eq!(session.base_model.as_deref(), Some("m9-turbo"));
}

/// #1668: a successful `/backends <name>` pins the backend and clears the
/// model — the pin merges per axis into the row, and the drain is the sole
/// owner of the operator baseline.
#[test]
fn a_backend_pick_pins_the_backend_and_clears_the_model() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = store_in(root.path(), ws.path());
    let id = durable_conversation(&store, "work");
    let cfg = cfg_with(&["sol"]);
    let mut session = Session::new(&cfg);

    mark_model_pick("m1");
    session.drain(Some(&store), &id);
    assert_eq!(
        store.preference_pin(&id).unwrap().unwrap().model.as_deref(),
        Some("m1")
    );

    mark_backend_pick("sol");
    session.drain(Some(&store), &id);
    let pin = store.preference_pin(&id).unwrap().unwrap();
    assert_eq!(pin.backend.as_deref(), Some("sol"));
    assert_eq!(pin.model, None, "/backends clears the stale model pin");
    assert_eq!(session.base_provider.as_deref(), Some("sol"));
    assert_eq!(session.base_model, None);

    // Dials merge in without disturbing the backend axis.
    mark_cognition_choice(CognitionOverride::Off);
    mark_tenacity_choice(Some(Tenacity::Relentless));
    session.drain(Some(&store), &id);
    let pin = store.preference_pin(&id).unwrap().unwrap();
    assert_eq!(pin.backend.as_deref(), Some("sol"), "untouched axis kept");
    assert_eq!(pin.cognition.as_deref(), Some("off"));
    assert_eq!(pin.tenacity, Some(Tenacity::Relentless));
}

/// #1668 / review finding 2 — the residue vector. Conversation A is pinned
/// and resumed; B is untouched. Visiting B must reset the session to the
/// invocation baseline and leave B's row EMPTY across turns.
#[test]
fn an_applied_pin_never_leaks_into_the_next_conversation() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = store_in(root.path(), ws.path());
    let cfg = cfg_with(&["sol", "other"]);
    let a = durable_conversation(&store, "pinned A");
    let b = durable_conversation(&store, "untouched B");
    store
        .update_preference_pin(
            &a,
            &OperatorPreferencePin {
                backend: Some("other".into()),
                cognition: Some("off".into()),
                tenacity: Some(Tenacity::Relentless),
                ..Default::default()
            },
        )
        .unwrap();

    let baseline = PreferenceBaseline::snapshot(None, None);
    let mut session = Session::new(&cfg);

    // Resume A: the pin applies to the LIVE session…
    session.switch_to(Some(&store), &a, &baseline, &cfg);
    assert_eq!(std::env::var("NEWT_PROVIDER").as_deref(), Ok("other"));
    assert_eq!(cli_cognition(), CognitionOverride::Off);
    assert_eq!(cli_tenacity(), Some(Tenacity::Relentless));
    // …but is NEVER adopted as the operator baseline (the residue vector).
    assert_eq!(session.base_provider, None, "pin must not become baseline");
    assert_eq!(session.base_model, None);

    // Switch to B: reset to the invocation baseline first.
    session.switch_to(Some(&store), &b, &baseline, &cfg);
    assert!(
        std::env::var("NEWT_PROVIDER").is_err(),
        "A's route must not survive the switch"
    );
    assert_eq!(cli_cognition(), CognitionOverride::Unset, "dial reset");
    assert_eq!(cli_tenacity(), None, "dial reset");

    // …and turns in B pin nothing at all.
    for _ in 0..3 {
        session.drain(Some(&store), &b);
    }
    assert_eq!(
        serde_json::to_string(&store.preference_pin(&b).unwrap().unwrap()).unwrap(),
        "{}",
        "B must still round-trip empty"
    );
    // A's own row is untouched by the visit to B.
    assert_eq!(
        store
            .preference_pin(&a)
            .unwrap()
            .unwrap()
            .backend
            .as_deref(),
        Some("other")
    );
}

/// #1668 / review finding 2, the `/new` half: a conversation started after
/// a pinned resume runs on the invocation baseline and pins nothing.
#[test]
fn a_new_conversation_after_a_pinned_resume_starts_from_the_baseline() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = store_in(root.path(), ws.path());
    let cfg = cfg_with(&["sol", "other"]);
    let a = durable_conversation(&store, "pinned A");
    store
        .update_preference_pin(
            &a,
            &OperatorPreferencePin {
                backend: Some("other".into()),
                tenacity: Some(Tenacity::Relentless),
                ..Default::default()
            },
        )
        .unwrap();
    let baseline = PreferenceBaseline::snapshot(None, None);
    let mut session = Session::new(&cfg);
    session.switch_to(Some(&store), &a, &baseline, &cfg);

    // `/new`: a brand-new id with no row yet.
    let fresh = store.create("fresh", None).unwrap();
    session.switch_to(Some(&store), &fresh, &baseline, &cfg);
    assert!(std::env::var("NEWT_PROVIDER").is_err());
    assert_eq!(cli_tenacity(), None);
    session.drain(Some(&store), &fresh);
    assert_eq!(
        serde_json::to_string(&store.preference_pin(&fresh).unwrap().unwrap()).unwrap(),
        "{}"
    );
}

/// #1668: an empty pin still re-seats the session on the invocation
/// baseline, and a pin's dials apply through the live setters.
#[test]
fn a_dials_pin_applies_and_an_empty_one_restores_the_baseline() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = store_in(root.path(), ws.path());
    let cfg = cfg_with(&["sol"]);
    let dialed = durable_conversation(&store, "dialed");
    let plain = durable_conversation(&store, "plain");
    store
        .update_preference_pin(
            &dialed,
            &OperatorPreferencePin {
                cognition: Some("off".into()),
                tenacity: Some(Tenacity::Relentless),
                ..Default::default()
            },
        )
        .unwrap();
    // The invocation launched with its own dial (a --tenacity flag).
    set_cli_tenacity(Tenacity::Insistent);
    let baseline = PreferenceBaseline::snapshot(None, None);
    let mut session = Session::new(&cfg);

    session.switch_to(Some(&store), &dialed, &baseline, &cfg);
    assert_eq!(cli_cognition(), CognitionOverride::Off, "'off' round-trips");
    assert_eq!(cli_tenacity(), Some(Tenacity::Relentless));
    assert!(std::env::var("NEWT_PROVIDER").is_err(), "axis untouched");

    session.switch_to(Some(&store), &plain, &baseline, &cfg);
    assert_eq!(cli_cognition(), CognitionOverride::Unset);
    assert_eq!(
        cli_tenacity(),
        Some(Tenacity::Insistent),
        "an unpinned axis returns to the INVOCATION baseline"
    );
}

/// #1668 / review finding 3: an unresolvable pinned axis applies nothing,
/// prints its notice — and, because capture is action-only, the STORED row
/// survives verbatim through subsequent turns.
#[test]
fn a_failed_open_pin_survives_verbatim_in_the_row() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = store_in(root.path(), ws.path());
    let cfg = cfg_with(&["sol"]);
    let id = durable_conversation(&store, "stale pin");
    let stale = OperatorPreferencePin {
        backend: Some("retired-dgx".into()),
        model: Some("nemotron-340b".into()),
        cognition: Some("transcending".into()),
        tenacity: Some(Tenacity::Relentless),
    };
    store.update_preference_pin(&id, &stale).unwrap();

    let baseline = PreferenceBaseline::snapshot(None, None);
    let mut session = Session::new(&cfg);
    session.switch_to(Some(&store), &id, &baseline, &cfg);
    assert!(
        std::env::var("NEWT_PROVIDER").is_err(),
        "an unknown pinned backend must not reroute"
    );
    assert!(
        std::env::var("NEWT_DGX_MODEL").is_err(),
        "its model must not smear onto the current backend"
    );
    assert_eq!(
        cli_cognition(),
        CognitionOverride::Unset,
        "bad dial skipped"
    );
    assert_eq!(
        cli_tenacity(),
        Some(Tenacity::Relentless),
        "good dial applies"
    );

    // Turns pass; nothing was acted on, so nothing overwrites the row.
    for _ in 0..3 {
        session.drain(Some(&store), &id);
    }
    assert_eq!(
        store.preference_pin(&id).unwrap(),
        Some(stale),
        "the fail-open pin must survive the session verbatim"
    );
}

/// #1668 / review findings 4 + 9: an axis this invocation's explicit flags
/// own is not overwritten by a pin — for the whole invocation — and the
/// stored row is untouched, so the pin still governs the next run.
#[test]
fn this_runs_explicit_flags_beat_the_pin_for_the_whole_invocation() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = store_in(root.path(), ws.path());
    let cfg = cfg_with(&["sol", "other"]);
    let id = durable_conversation(&store, "pinned");
    let pin = OperatorPreferencePin {
        backend: Some("other".into()),
        cognition: Some("off".into()),
        tenacity: Some(Tenacity::Relaxed),
        ..Default::default()
    };
    store.update_preference_pin(&id, &pin).unwrap();

    // `newt --cognition contemplating` this run: the flag installs the dial
    // and records that it owns the axis.
    set_cli_cognition(CognitionOverride::Set(Cognition::Contemplating));
    record_cli_preference_axes(PreferenceAxes {
        cognition: true,
        ..Default::default()
    });
    assert!(cli_preference_axes().cognition);
    let baseline = PreferenceBaseline::snapshot(None, None);
    let mut session = Session::new(&cfg);

    session.switch_to(Some(&store), &id, &baseline, &cfg);
    assert_eq!(
        cli_cognition(),
        CognitionOverride::Set(Cognition::Contemplating),
        "the just-typed flag must beat the stored pin"
    );
    assert_eq!(cli_tenacity(), Some(Tenacity::Relaxed), "unowned applies");
    assert_eq!(
        std::env::var("NEWT_PROVIDER").as_deref(),
        Ok("other"),
        "the unowned backend axis still applies"
    );
    // Re-switching (a second /resume) keeps the flag winning all run.
    session.switch_to(Some(&store), &id, &baseline, &cfg);
    assert_eq!(
        cli_cognition(),
        CognitionOverride::Set(Cognition::Contemplating)
    );
    // The row is untouched — the flag wins this run, not forever.
    assert_eq!(store.preference_pin(&id).unwrap(), Some(pin));
}

/// #1668 / review finding 6: a claim-refused startup resume neither applies
/// nor captures the held conversation's pin — the session runs the fresh
/// replacement conversation on the invocation baseline.
///
/// Review-2 finding 6: this drives the REAL gate
/// ([`apply_startup_preference_pin`]) that `run_chat` calls, and points it
/// at the HELD conversation on purpose. Hand-modelling the ordering — by
/// passing the replacement id the test computed itself — proved only that
/// a conversation with no pin applies no pin, which is vacuous. Aiming the
/// refused outcome squarely at the pinned row is what makes re-introducing
/// the bug fail: restore the old `resumed_at_start`-only condition and this
/// applies `other` + `Relentless` and the assertions below fail.
#[test]
fn a_claim_refused_resume_neither_applies_nor_captures_the_pin() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = store_in(root.path(), ws.path());
    let cfg = cfg_with(&["sol", "other"]);
    let held = durable_conversation(&store, "held elsewhere");
    store
        .update_preference_pin(
            &held,
            &OperatorPreferencePin {
                backend: Some("other".into()),
                tenacity: Some(Tenacity::Relentless),
                ..Default::default()
            },
        )
        .unwrap();

    let baseline = PreferenceBaseline::snapshot(None, None);
    let mut session = Session::new(&cfg);
    // Aimed at the HELD id, with the outcome that says we do not hold it:
    // the gate must refuse on the outcome alone.
    let applied = session.switch_at_startup(
        StartupConversation::ResumedRefused,
        Some(&store),
        &held,
        &baseline,
        &cfg,
    );
    assert!(!applied, "a refused claim applies nothing");
    assert!(
        std::env::var("NEWT_PROVIDER").is_err(),
        "the held conversation's pin must not be applied"
    );
    assert_eq!(cli_tenacity(), None);

    // And the positive control: the SAME pin, the same seam, with the claim
    // granted — so the test above cannot pass merely because the plumbing
    // is inert.
    let mut holder = Session::new(&cfg);
    holder.switch_at_startup(
        StartupConversation::ResumedHeld,
        Some(&store),
        &held,
        &baseline,
        &cfg,
    );
    assert_eq!(
        std::env::var("NEWT_PROVIDER").ok().as_deref(),
        Some("other"),
        "held: the pin DOES apply, so the refusal above is a real refusal"
    );
    assert_eq!(cli_tenacity(), Some(Tenacity::Relentless));
    reset_globals();

    let replacement = store.create("replacement", None).unwrap();
    session.drain(Some(&store), &replacement);
    assert_eq!(
        store
            .preference_pin(&held)
            .unwrap()
            .unwrap()
            .backend
            .as_deref(),
        Some("other"),
        "and the held conversation's row is untouched"
    );
}

/// A persona declaring `backend:` + `model:`, for the persona-branch tests.
fn persona_routing_to(backend: &str, model: Option<&str>) -> Persona {
    let mut front = format!("+++\nbackend = \"{backend}\"\n");
    if let Some(m) = model {
        front.push_str(&format!("model = \"{m}\"\n"));
    }
    front.push_str("+++\nbe helpful\n");
    Persona {
        name: "router".to_string(),
        prompt: "be helpful".to_string(),
        path: std::path::PathBuf::from("/dev/null"),
        profile: newt_core::RoleProfile::parse(&front).expect("front-matter parses"),
    }
}

/// #1668 review-2 finding 7: the persona branch of the switch. Every
/// earlier test in this module passed `persona: None`, so the documented
/// precedence — a conversation's PIN outranks the active persona's declared
/// `backend:` — was asserted nowhere and could have regressed silently.
///
/// Both directions, because only the pair is meaningful: with no pin the
/// persona's route is what survives the switch, and with a pin the pin wins.
#[test]
fn a_pin_outranks_the_active_personas_declared_backend() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = store_in(root.path(), ws.path());
    let cfg = cfg_with(&["sol", "other"]);
    // The persona's model MUST match what the config already resolves to
    // ("m0"): the harness keeps this tier network-free by never letting a
    // route change the resolved url/model, which is what fires
    // `refresh_backend`'s served-adoption probe.
    let persona = persona_routing_to("sol", Some("m0"));
    let baseline = PreferenceBaseline::snapshot(None, None);

    // 1. No pin on the incoming conversation: the persona's route stands.
    let unpinned = durable_conversation(&store, "unpinned");
    let mut session = Session::new(&cfg);
    session.switch_with_persona(Some(&store), &unpinned, &baseline, &cfg, Some(&persona));
    assert_eq!(
        std::env::var("NEWT_PROVIDER").ok().as_deref(),
        Some("sol"),
        "with nothing pinned, the persona's declared backend routes"
    );
    assert_eq!(
        std::env::var("NEWT_DGX_MODEL").ok().as_deref(),
        Some("m0"),
        "and the persona's declared model with it"
    );

    // 2. A pin naming a DIFFERENT backend outranks it.
    let pinned = durable_conversation(&store, "pinned");
    store
        .update_preference_pin(
            &pinned,
            &OperatorPreferencePin {
                backend: Some("other".into()),
                ..Default::default()
            },
        )
        .unwrap();
    session.switch_with_persona(Some(&store), &pinned, &baseline, &cfg, Some(&persona));
    assert_eq!(
        std::env::var("NEWT_PROVIDER").ok().as_deref(),
        Some("other"),
        "the conversation's pin outranks the persona's declared backend"
    );
    // And the pin named no model, so the override clears to the backend's
    // own default rather than inheriting the persona's model — the
    // `/backends <name>` rule, which a pin must not quietly opt out of.
    assert!(
        std::env::var("NEWT_DGX_MODEL").is_err(),
        "a pinned backend with no model clears the override, persona or not"
    );
}

/// #1668: an ephemeral session (no store) never writes a pin — the #545
/// "leave no trace" guarantee — while the operator baseline still tracks
/// the live action.
#[test]
fn an_ephemeral_session_marks_actions_but_persists_nothing() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let cfg = cfg_with(&["sol"]);
    let mut session = Session::new(&cfg);
    mark_backend_pick("sol");
    session.drain(None, "no-store-conversation");
    assert_eq!(session.base_provider.as_deref(), Some("sol"));
    assert!(session.pending.is_empty(), "nothing left held");
}

/// #1668: a conversation with no durable row yet HOLDS its actions until
/// the row exists (a fresh session's first `/backends` must still pin),
/// and a conversation switch drops what was never written.
#[test]
fn actions_wait_for_a_durable_row_and_are_dropped_on_a_switch() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = store_in(root.path(), ws.path());
    let cfg = cfg_with(&["sol"]);
    let id = store.create("no turns yet", None).unwrap();
    assert!(store.preference_pin(&id).unwrap().is_some());

    // A conversation id the store has never seen has no row at all.
    let unborn = "conv-not-yet-saved";
    let mut session = Session::new(&cfg);
    mark_backend_pick("sol");
    session.drain(Some(&store), unborn);
    assert!(!session.pending.is_empty(), "held for the row to appear");
    assert_eq!(
        session.base_provider.as_deref(),
        Some("sol"),
        "the live baseline still follows the action"
    );

    // The row appears (the first saved turn creates it) → the held action
    // lands on the next drain.
    store.create_with_id(unborn, "now durable", None).unwrap();
    session.drain(Some(&store), unborn);
    assert_eq!(
        store
            .preference_pin(unborn)
            .unwrap()
            .unwrap()
            .backend
            .as_deref(),
        Some("sol")
    );

    // Held actions belong to the conversation being LEFT.
    mark_model_pick("m7");
    session.drain(Some(&store), "another-unborn");
    assert!(!session.pending.is_empty());
    let baseline = PreferenceBaseline::snapshot(None, None);
    session.switch_to(Some(&store), &id, &baseline, &cfg);
    assert!(
        session.pending.is_empty(),
        "a switch drops what the outgoing conversation never persisted"
    );
    session.drain(Some(&store), &id);
    assert_eq!(
        serde_json::to_string(&store.preference_pin(&id).unwrap().unwrap()).unwrap(),
        "{}",
        "the incoming conversation must not inherit them"
    );
}

/// #1668: a corrupt row fails open — one notice, the invocation baseline,
/// and no crash. The authority-boundary counterpart of the newt-core
/// decode tests: a tampered column can never route or dial the session.
#[test]
fn a_corrupt_pin_falls_open_to_the_invocation_baseline() {
    let _g = GlobalSettingsGuard::acquire();
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = store_in(root.path(), ws.path());
    let cfg = cfg_with(&["sol", "other"]);
    let id = durable_conversation(&store, "tampered");
    store
        .set_raw_preference_pin_for_test(
            &id,
            r#"{"backend":"other","api_key":"sk-evil","sandbox":"off","caveats":{"fs":"all"}}"#,
        )
        .unwrap();
    // SAFETY: guarded single-threaded test.
    unsafe { std::env::set_var("NEWT_PROVIDER", "sol") };
    let baseline = PreferenceBaseline::snapshot(Some("sol".into()), None);
    let mut session = Session::new(&cfg);
    session.switch_to(Some(&store), &id, &baseline, &cfg);
    assert_eq!(
        std::env::var("NEWT_PROVIDER").as_deref(),
        Ok("sol"),
        "a tampered row must not reroute the session"
    );
    assert_eq!(session.base_provider.as_deref(), Some("sol"));
    assert_eq!(cli_cognition(), CognitionOverride::Unset);
    assert_eq!(cli_tenacity(), None);
}
