use super::*;

#[test]
fn persona_provider_env_maps_backend_and_optional_model() {
    // A persona naming a backend + model → both routing values.
    let p = newt_core::RoleProfile::parse(
        "+++\nrole = \"researcher\"\nbackend = \"sol\"\nmodel = \"gpt-5.6-sol\"\n+++\n\n# Bob\n",
    )
    .unwrap();
    assert_eq!(
        persona_provider_env(Some(&p)),
        Some(("sol".to_string(), Some("gpt-5.6-sol".to_string())))
    );
    // A persona naming only a backend → clear the model override (None), so
    // the backend's own default model applies (mirrors `/backends`).
    let p2 = newt_core::RoleProfile::parse("+++\nbackend = \"sol\"\n+++\n\n# B\n").unwrap();
    assert_eq!(
        persona_provider_env(Some(&p2)),
        Some(("sol".to_string(), None))
    );
    // No backend declared → no routing (leave the session backend untouched).
    let p3 = newt_core::RoleProfile::parse("+++\ncognition = \"pondering\"\n+++\n\n# T\n").unwrap();
    assert_eq!(persona_provider_env(Some(&p3)), None);
    assert_eq!(persona_provider_env(None), None);
}

#[test]
fn persona_backend_route_validates_known_reverts_none_and_refuses_unknown() {
    let configured = ["sol", "openai"];
    // A valid backend + model → route to it.
    let p = newt_core::RoleProfile::parse(
        "+++\nbackend = \"sol\"\nmodel = \"gpt-5.6-sol\"\n+++\n\n# B\n",
    )
    .unwrap();
    assert_eq!(
        persona_backend_route(Some(&p), &configured),
        Ok(Some(("sol".to_string(), Some("gpt-5.6-sol".to_string()))))
    );
    // Valid backend, no model → route with the override cleared.
    let p2 = newt_core::RoleProfile::parse("+++\nbackend = \"openai\"\n+++\n\n# B\n").unwrap();
    assert_eq!(
        persona_backend_route(Some(&p2), &configured),
        Ok(Some(("openai".to_string(), None)))
    );
    // An UNKNOWN backend name is REFUSED (no silent fallback reroute) — the
    // caller warns and leaves the env untouched.
    let p3 = newt_core::RoleProfile::parse("+++\nbackend = \"ghost\"\n+++\n\n# B\n").unwrap();
    assert_eq!(
        persona_backend_route(Some(&p3), &configured),
        Err("ghost".to_string())
    );
    // No backend (or a cleared persona) → Ok(None) = revert to the baseline.
    let p4 = newt_core::RoleProfile::parse("+++\ncognition = \"pondering\"\n+++\n\n# T\n").unwrap();
    assert_eq!(persona_backend_route(Some(&p4), &configured), Ok(None));
    assert_eq!(persona_backend_route(None, &configured), Ok(None));
}
