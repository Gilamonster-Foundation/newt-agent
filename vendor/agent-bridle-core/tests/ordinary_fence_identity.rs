//! Freeze the default fence identity before introducing an optional exec domain.
use agent_bridle_core::{
    empty_closure, AdmittedFence, AxisEnforcement, BackendProjection, Caveats, ChildNetworkPolicy,
    ConfinementMechanism, EnforcementFloor, ResolvedAuthority, RuntimeClosure, SandboxKind, Scope,
};

#[test]
fn ordinary_fence_body_and_cid_keep_the_existing_encoding() {
    let caveats = Caveats {
        exec: Scope::only(["/bin/sh".to_owned()]),
        ..Caveats::top()
    };
    let mechanism = ConfinementMechanism::new(SandboxKind::None, ChildNetworkPolicy::LandlockOnly);
    let admitted = AdmittedFence::admit(
        &caveats,
        RuntimeClosure::empty(),
        mechanism,
        EnforcementFloor::from_scalar(AxisEnforcement::Advisory),
        |effective| BackendProjection {
            resolved: ResolvedAuthority::from_delegated(effective),
            runtime_closure: empty_closure(),
        },
    )
    .unwrap();
    let body = serde_json::json!({"mechanism_caveats": caveats, "mechanism": mechanism});
    assert_eq!(
        body.to_string(),
        r#"{"mechanism":{"child_network":"landlock-only","kind":"none"},"mechanism_caveats":{"exec":{"only":["/bin/sh"]},"fs_read":"all","fs_write":"all","max_calls":"unlimited","net":"all","valid_for_generation":"all"}}"#
    );
    assert_eq!(
        serde_json::to_value(admitted.fence_id()).unwrap(),
        "bafyr4ieamrtjdm7e5blaezbyyz6lmbwtkw6ifxmppvwqqwubnvmlxqcvci"
    );
}

#[test]
fn ordinary_serialized_envelope_round_trips_with_omitted_false_flags() {
    let envelope = agent_bridle_core::ToolEnvelope::new(SandboxKind::None)
        .with_exit_code(0)
        .with_stdout("actual ordinary result")
        .with_disclosure(agent_bridle_core::Disclosure {
            engine: Some("brush".to_owned()),
            ..Default::default()
        });
    let wire = envelope.into_json();
    eprintln!("actual ordinary serialized envelope: {wire}");
    assert!(wire.get("denied").is_none());
    assert!(wire.get("stdout_truncated").is_none());
    assert!(wire.get("stderr_truncated").is_none());
    assert!(wire["disclosure"].get("unbridled").is_none());
    assert!(wire["disclosure"].get("net_over_delivery").is_none());
    let decoded: agent_bridle_core::ToolEnvelope = serde_json::from_value(wire).unwrap();
    assert!(!decoded.denied && !decoded.stdout_truncated && !decoded.stderr_truncated);
    assert!(!decoded.disclosure.unbridled && !decoded.disclosure.net_over_delivery);
}

#[test]
fn omitted_flag_defaults_do_not_coerce_malformed_or_true_values() {
    use agent_bridle_core::{Disclosure, ToolEnvelope};
    let mut envelope = ToolEnvelope::new(SandboxKind::None).with_disclosure(Disclosure {
        unbridled: true,
        net_over_delivery: true,
        ..Default::default()
    });
    envelope.denied = true;
    envelope.stdout_truncated = true;
    envelope.stderr_truncated = true;
    let wire = envelope.into_json();
    let parsed: ToolEnvelope = serde_json::from_value(wire.clone()).unwrap();
    assert!(parsed.denied && parsed.stdout_truncated && parsed.stderr_truncated);
    assert!(parsed.disclosure.unbridled && parsed.disclosure.net_over_delivery);
    for field in [
        "denied",
        "stdout_truncated",
        "stderr_truncated",
        "unbridled",
        "net_over_delivery",
    ] {
        for invalid in [
            serde_json::Value::Null,
            serde_json::json!("false"),
            serde_json::json!(0),
        ] {
            let mut bad = wire.clone();
            if matches!(field, "unbridled" | "net_over_delivery") {
                bad["disclosure"][field] = invalid;
            } else {
                bad[field] = invalid;
            }
            assert!(
                serde_json::from_value::<ToolEnvelope>(bad).is_err(),
                "{field}"
            );
        }
    }
    let mut missing_backend = wire;
    missing_backend
        .as_object_mut()
        .unwrap()
        .remove("sandbox_kind");
    assert!(serde_json::from_value::<ToolEnvelope>(missing_backend).is_err());
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
