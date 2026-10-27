use super::*;

#[test]
fn unix_now_is_plausible() {
    assert!(unix_now() > 1_735_689_600.0);
}

#[test]
fn hermes_names_are_portable_and_legacy_hashes_are_path_safe() {
    let traversal = token_cache_component("../../other/server");
    let ordinary = token_cache_component("other-server");
    assert!(!traversal.contains('/'));
    assert!(!traversal.contains(".."));
    assert_ne!(traversal, ordinary);
    assert!(hermes_raw_component("../../other/server").is_none());
    assert!(hermes_raw_component("ordinary-server").is_some());
    assert_eq!(hermes_raw_component("Review.Source"), Some("Review.Source"));
    assert!(hermes_raw_component("CON").is_none());
    assert!(hermes_raw_component("con.device").is_none());
    assert!(hermes_raw_component("server name").is_none());
    assert!(hermes_raw_component("server.meta").is_none());
    assert!(hermes_raw_component("server.client").is_none());
    assert!(hermes_raw_component("server.").is_none());
    assert!(hermes_raw_component(".newt-oauth-deadbeef.manifest").is_none());
    assert!(hermes_raw_component("server-0123456789abcdef01234567").is_none());
    assert!(hermes_raw_component(
        "server-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    )
    .is_none());
}

/// Real-filesystem grounding for mocked companion-name collision checks.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn companion_suffix_names_cannot_collide_with_another_servers_raw_trio() {
    let temp = tempfile::tempdir().unwrap();
    let ordinary = portable_credential_paths(temp.path(), "server").unwrap();
    let meta_named = portable_credential_paths(temp.path(), "server.meta").unwrap();
    let client_named = portable_credential_paths(temp.path(), "server.client").unwrap();

    assert_ne!(meta_named.token, ordinary.meta);
    assert_ne!(client_named.token, ordinary.client);
    assert!(meta_named.token.starts_with(temp.path()));
    assert!(client_named.token.starts_with(temp.path()));
}

/// Real-filesystem grounding for mocked hashed-namespace path isolation.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn new_hashed_namespace_cannot_collide_with_raw_or_legacy_hash_names() {
    let temp = tempfile::tempdir().unwrap();
    let unsafe_name = "team/server";
    let canonical = portable_credential_paths(temp.path(), unsafe_name).unwrap();
    let legacy = full_hashed_credential_paths(temp.path(), unsafe_name);
    let legacy_component = legacy
        .token
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap();
    let literal = portable_credential_paths(temp.path(), legacy_component).unwrap();

    assert_ne!(canonical.token, legacy.token);
    assert_ne!(literal.token, legacy.token);
    assert!(canonical
        .token
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with(".newt-oauth-name-"));
}

#[test]
fn auth_status_requires_resource_and_issuer_binding() {
    let token = TokenFile {
        access_token: "secret".into(),
        refresh_token: None,
        expires_at: Some(2_000.0),
        resource: None,
        issuer: None,
        extra: BTreeMap::new(),
    };
    let meta = MetaFile {
        resource: "https://mcp.example/team-a".into(),
        issuer: "https://auth.example".into(),
        authorization_endpoint: Some("https://auth.example/authorize".into()),
        token_endpoint: "https://auth.example/token".into(),
        code_challenge_methods_supported: vec!["S256".into()],
        authorization_response_iss_parameter_supported: false,
        extra: BTreeMap::new(),
    };
    let unbound_client = ClientFile {
        client_id: "client".into(),
        redirect_uris: vec![],
        issuer: None,
        extra: BTreeMap::new(),
    };
    assert_eq!(
        classify_auth_state(
            "https://mcp.example/team-a",
            None,
            None,
            Some(&unbound_client),
            1_000.0,
        ),
        AuthState::NeedsMigration
    );

    assert_eq!(
        classify_auth_state(
            "https://mcp.example/team-a",
            Some(&token),
            Some(&meta),
            Some(&unbound_client),
            1_000.0,
        ),
        AuthState::NeedsMigration
    );
    let bound_client = ClientFile {
        issuer: Some("https://auth.example".into()),
        ..unbound_client
    };
    let unknown_expiry = TokenFile {
        expires_at: None,
        resource: Some(meta.resource.clone()),
        issuer: Some(meta.issuer.clone()),
        ..token.clone()
    };
    assert_eq!(
        classify_auth_state(
            "https://mcp.example/team-a",
            Some(&unknown_expiry),
            Some(&meta),
            Some(&bound_client),
            1_000.0,
        ),
        AuthState::Valid
    );
    assert_eq!(
        classify_auth_state(
            "https://mcp.example/team-a",
            Some(&TokenFile {
                resource: Some(meta.resource.clone()),
                issuer: Some(meta.issuer.clone()),
                ..token.clone()
            }),
            Some(&meta),
            Some(&bound_client),
            1_000.0,
        ),
        AuthState::Valid
    );
    assert_eq!(
        classify_auth_state(
            "https://mcp.example/team-b",
            Some(&token),
            Some(&meta),
            Some(&bound_client),
            1_000.0,
        ),
        AuthState::NeedsMigration
    );
}

/// #3038 review: the load-time auth pass treats a saved login the connect
/// path can use without a browser as an auth path. A bound, expired token with
/// a refresh token classifies `Expired` and is usable (connect refreshes it);
/// one without a refresh token, or a rejected login (`NeedsFlow`, #3032), is
/// not, and the server is warned.
#[test]
fn a_refreshable_expired_login_is_an_auth_path_and_a_rejected_one_is_not_3038() {
    let resource = "https://mcp.example/team-a";
    let meta = MetaFile {
        resource: resource.into(),
        issuer: "https://auth.example".into(),
        authorization_endpoint: Some("https://auth.example/authorize".into()),
        token_endpoint: "https://auth.example/token".into(),
        code_challenge_methods_supported: vec!["S256".into()],
        authorization_response_iss_parameter_supported: false,
        extra: BTreeMap::new(),
    };
    let client = ClientFile {
        client_id: "client".into(),
        redirect_uris: vec![],
        issuer: Some(meta.issuer.clone()),
        extra: BTreeMap::new(),
    };
    let expired = |refresh_token: Option<&str>| TokenFile {
        access_token: "secret".into(),
        refresh_token: refresh_token.map(str::to_string),
        expires_at: Some(1_000.0),
        resource: Some(meta.resource.clone()),
        issuer: Some(meta.issuer.clone()),
        extra: BTreeMap::new(),
    };
    let state = |token: &TokenFile| {
        classify_auth_state(resource, Some(token), Some(&meta), Some(&client), 2_000.0)
    };
    let refreshable = state(&expired(Some("refresh")));
    let spent = state(&expired(None));
    assert_eq!(refreshable, AuthState::Expired);
    assert_eq!(spent, AuthState::NeedsFlow);

    let entry = |name: &str| {
        let mut entry: newt_core::mcp::McpServerEntry = toml::from_str(&format!(
            "name = '{name}'\ntype = 'http'\nurl = '{resource}'\n"
        ))
        .unwrap();
        entry.trust = newt_core::mcp::McpTrust::Trusted;
        entry
    };
    let states = [
        ("valid", AuthState::Valid),
        ("refreshable", refreshable),
        ("spent", spent),
        ("rejected", AuthState::NeedsFlow),
        ("migrate", AuthState::NeedsMigration),
        ("none", AuthState::Unregistered),
    ];
    let entries: Vec<_> = states.iter().map(|(name, _)| entry(name)).collect();
    let warnings = newt_core::mcp::missing_auth_warnings(&entries, &|entry| {
        states
            .iter()
            .any(|(name, state)| *name == entry.name && state.usable_without_login())
    });
    let warned: Vec<&str> = entries
        .iter()
        .map(|entry| entry.name.as_str())
        .filter(|name| warnings.iter().any(|w| w.contains(&format!("`{name}`"))))
        .collect();
    assert_eq!(warned, ["spent", "rejected", "migrate", "none"]);
}

#[test]
fn token_response_requires_nonempty_bearer_but_allows_unknown_expiry() {
    let token =
        parse_token_response(br#"{"access_token":"secret","token_type":"Bearer"}"#).unwrap();
    assert_eq!(token.expires_in, None);
    assert!(parse_token_response(br#"{"access_token":"","token_type":"Bearer"}"#).is_err());
    assert!(parse_token_response(br#"{"access_token":"secret","token_type":"mac"}"#).is_err());
}

#[test]
fn confidential_client_authentication_uses_basic_without_body_secret() {
    let mut extra = BTreeMap::new();
    extra.insert(
        "token_endpoint_auth_method".into(),
        serde_json::Value::String("client_secret_basic".into()),
    );
    extra.insert(
        "client_secret".into(),
        serde_json::Value::String("secret".into()),
    );
    let client = ClientFile {
        client_id: "client".into(),
        redirect_uris: vec![],
        issuer: Some("https://auth.example".into()),
        extra,
    };
    let mut form = Vec::new();
    let request = apply_client_authentication(
        reqwest::Client::new().post("https://auth.example/token"),
        &client,
        &mut form,
    )
    .unwrap()
    .build()
    .unwrap();
    assert!(request
        .headers()
        .contains_key(reqwest::header::AUTHORIZATION));
    assert!(form.is_empty());
}

/// Real-filesystem grounding for mocked exact-name and case-fold path mapping.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn exact_server_names_do_not_alias_on_case_or_dots() {
    let dir = Path::new("/tokens");
    assert_ne!(
        token_path(dir, "Foo", ".json").unwrap(),
        token_path(dir, "foo", ".json").unwrap()
    );
    assert_eq!(
        token_path(dir, "foo", ".json").unwrap(),
        dir.join("foo.json")
    );
    let dotted = token_path(dir, "Review.Source", ".json").unwrap();
    assert_eq!(dotted, dir.join("Review.Source.json"));
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(
        credential_lock_path(temp.path(), "Review.Source").unwrap(),
        credential_lock_path(temp.path(), "review.source").unwrap()
    );
}

/// Real-filesystem grounding for mocked raw-Hermes alias rejection.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn raw_hermes_case_alias_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    write_token_file(
        &temp.path().join("review.source.json"),
        &serde_json::json!({"access_token":"legacy"}),
    )
    .unwrap();
    let error = portable_credential_paths(temp.path(), "Review.Source")
        .unwrap_err()
        .to_string();
    assert!(error.contains("case-fold alias"), "{error}");
}

#[test]
fn windows_home_fallback_is_injectable_without_global_env_mutation() {
    let user = std::ffi::OsStr::new(r"C:\Users\ExampleUser");
    assert_eq!(
        platform_home_from(None, Some(user)),
        Some(PathBuf::from(user))
    );
    assert_eq!(
        platform_home_from(Some(std::ffi::OsStr::new("/home/example")), Some(user)),
        Some(PathBuf::from("/home/example"))
    );
}

#[test]
fn unknown_expiry_attempts_refresh_once_with_safe_fallback() {
    let token = TokenFile {
        access_token: "access".into(),
        refresh_token: Some("refresh".into()),
        expires_at: None,
        resource: None,
        issuer: None,
        extra: BTreeMap::new(),
    };
    assert_eq!(
        token_load_action(&token, 1_000.0),
        TokenLoadAction::RefreshWithFallback
    );
}

#[test]
fn saved_client_auth_contract_must_be_usable_before_browser_side_effects() {
    let base = ClientFile {
        client_id: "public-client".into(),
        redirect_uris: vec!["http://127.0.0.1:0/callback".into()],
        issuer: Some("https://auth.example.test".into()),
        extra: BTreeMap::new(),
    };
    assert!(client_auth_is_usable(&base));
    assert!(!client_auth_is_usable(&ClientFile {
        client_id: "".into(),
        ..base.clone()
    }));
    assert!(!client_auth_is_usable(&ClientFile {
        extra: BTreeMap::from([(
            "token_endpoint_auth_method".into(),
            serde_json::json!("private_key_jwt"),
        )]),
        ..base.clone()
    }));
    assert!(!client_auth_is_usable(&ClientFile {
        extra: BTreeMap::from([(
            "token_endpoint_auth_method".into(),
            serde_json::json!("client_secret_basic"),
        )]),
        ..base.clone()
    }));
    assert!(client_auth_is_usable(&ClientFile {
        extra: BTreeMap::from([
            (
                "token_endpoint_auth_method".into(),
                serde_json::json!("client_secret_post"),
            ),
            ("client_secret".into(), serde_json::json!("test-secret")),
        ]),
        ..base
    }));
}

fn credential_test_bundle(access_token: &str, legacy_unbound: bool) -> CredentialBundle {
    let resource = "https://mcp.example.test/team/mcp";
    let issuer = "https://auth.example.test";
    CredentialBundle {
        token: TokenFile {
            access_token: access_token.into(),
            refresh_token: Some(format!("refresh-{access_token}")),
            expires_at: None,
            resource: (!legacy_unbound).then(|| resource.into()),
            issuer: (!legacy_unbound).then(|| issuer.into()),
            extra: BTreeMap::from([("token_type".into(), serde_json::json!("Bearer"))]),
        },
        meta: MetaFile {
            resource: resource.into(),
            issuer: issuer.into(),
            authorization_endpoint: Some(format!("{issuer}/authorize")),
            token_endpoint: format!("{issuer}/token"),
            code_challenge_methods_supported: vec!["S256".into()],
            authorization_response_iss_parameter_supported: true,
            extra: BTreeMap::new(),
        },
        client: ClientFile {
            client_id: format!("client-{access_token}"),
            redirect_uris: vec!["http://127.0.0.1:0/callback".into()],
            issuer: Some(issuer.into()),
            extra: BTreeMap::new(),
        },
    }
}

/// Real-filesystem grounding for mocked atomic manifest commit recovery.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn manifest_commit_point_rejects_every_partial_generation() {
    let interrupted = [
        PublishPhase::GenerationClient,
        PublishPhase::GenerationMeta,
        PublishPhase::GenerationToken,
    ];
    for phase in interrupted {
        let temp = tempfile::tempdir().unwrap();
        let name = "Review.Source";
        let old = credential_test_bundle("old", false);
        let new = credential_test_bundle("new", false);
        let transaction = acquire_credential_lock(temp.path(), name).unwrap();
        publish_credential_generation(
            temp.path(),
            name,
            &old,
            HermesCursor::default(),
            &transaction,
        )
        .unwrap();
        let error = publish_credential_generation_with_hook(
            temp.path(),
            name,
            &new,
            HermesCursor::default(),
            &transaction.manifest_destination,
            |published| {
                if published == phase {
                    anyhow::bail!("simulated crash after {published:?}");
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("simulated crash"));
        migrate_legacy_credentials(temp.path(), name, &transaction).unwrap();
        let active = read_credential_bundle(temp.path(), name).unwrap().unwrap();
        let expected = "old";
        assert_eq!(
            active.token.access_token, expected,
            "production recovery after {phase:?} must select one coherent generation"
        );
        assert_eq!(active.client.client_id, format!("client-{expected}"));
        assert!(!portable_credential_paths(temp.path(), name)
            .unwrap()
            .any_present());
    }

    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    publish_credential_generation(
        temp.path(),
        name,
        &credential_test_bundle("old", false),
        HermesCursor::default(),
        &transaction,
    )
    .unwrap();
    let _ = publish_credential_generation_with_hook(
        temp.path(),
        name,
        &credential_test_bundle("new", false),
        HermesCursor::default(),
        &transaction.manifest_destination,
        |phase| {
            if phase == PublishPhase::Manifest {
                anyhow::bail!("simulated crash after manifest");
            }
            Ok(())
        },
    );
    migrate_legacy_credentials(temp.path(), name, &transaction).unwrap();
    let active = read_credential_bundle(temp.path(), name).unwrap().unwrap();
    assert_eq!(active.token.access_token, "new");
    assert_eq!(active.client.client_id, "client-new");
}

/// Real-filesystem grounding for mocked portable exact-name migration.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn mixed_case_dotted_hermes_trio_migrates_without_renaming() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let bundle = credential_test_bundle("legacy", false);
    let raw = exact_raw_paths(temp.path(), name).unwrap();
    write_token_file(&raw.token, &bundle.token).unwrap();
    write_token_file(&raw.meta, &bundle.meta).unwrap();
    write_token_file(&raw.client, &bundle.client).unwrap();

    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    assert!(migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());
    assert!(credential_manifest_path(temp.path(), name).is_file());
    assert!(
        raw.complete(),
        "Hermes exact-name mirror must remain readable"
    );
    let active = read_credential_bundle(temp.path(), name).unwrap().unwrap();
    assert_eq!(active.token.access_token, "legacy");
}

/// Real-filesystem grounding for mocked Hermes rotation adoption.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn hermes_token_rotation_is_adopted_instead_of_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let old = credential_test_bundle("old", false);
    let raw = exact_raw_paths(temp.path(), name).unwrap();
    write_token_file(&raw.token, &old.token).unwrap();
    write_token_file(&raw.meta, &old.meta).unwrap();
    write_token_file(&raw.client, &old.client).unwrap();
    let meta_before = std::fs::read(&raw.meta).unwrap();
    let client_before = std::fs::read(&raw.client).unwrap();
    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    assert!(migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());

    let rotated = updated_token_file(
        "hermes-new",
        Some("hermes-refresh-new"),
        Some(3600.0),
        &old.token.extra,
        &old.meta.resource,
        &old.meta.issuer,
    );
    write_token_file(&raw.token, &rotated).unwrap();

    assert!(migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());
    let active = read_credential_bundle(temp.path(), name).unwrap().unwrap();
    assert_eq!(active.token.access_token, "hermes-new");
    assert_eq!(
        active.token.refresh_token.as_deref(),
        Some("hermes-refresh-new")
    );
    let mirror: TokenFile = read_json(&raw.token).unwrap().unwrap();
    assert_eq!(mirror.access_token, "hermes-new");
    assert_eq!(std::fs::read(&raw.meta).unwrap(), meta_before);
    assert_eq!(std::fs::read(&raw.client).unwrap(), client_before);
}

/// Real-filesystem grounding for mocked unbound-rotation refusal.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn stable_unbound_hermes_token_rotation_is_not_adopted() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let old = credential_test_bundle("old", false);
    let raw = exact_raw_paths(temp.path(), name).unwrap();
    write_token_file(&raw.token, &old.token).unwrap();
    write_token_file(&raw.meta, &old.meta).unwrap();
    write_token_file(&raw.client, &old.client).unwrap();
    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    assert!(migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());
    let manifest_before = read_credential_manifest(temp.path(), name)
        .unwrap()
        .unwrap();

    // Hermes replaces each flat file independently. This stable token-only
    // state can therefore be the middle of a full reauthorization for a
    // different resource, not a refresh of the old trio.
    let mut unbound = old.token.clone();
    unbound.access_token = "unbound-rotation".into();
    unbound.refresh_token = Some("unbound-refresh".into());
    unbound.resource = None;
    unbound.issuer = None;
    write_token_file(&raw.token, &unbound).unwrap();

    assert!(!migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());
    let active = read_credential_bundle(temp.path(), name).unwrap().unwrap();
    assert_eq!(active.token.access_token, "old");
    assert_eq!(
        read_json::<TokenFile>(&raw.token)
            .unwrap()
            .unwrap()
            .access_token,
        "unbound-rotation"
    );
    let manifest_after = read_credential_manifest(temp.path(), name)
        .unwrap()
        .unwrap();
    assert_eq!(
        manifest_after.hermes_token_sha256, manifest_before.hermes_token_sha256,
        "refusing an unbound rotation must not advance the adoption cursor"
    );
}

/// Real-filesystem grounding for mocked refresh-generation precedence.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn newt_refresh_generation_cannot_roll_back_to_an_unchanged_flat_token() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let flat = credential_test_bundle("flat-old", false);
    let raw = exact_raw_paths(temp.path(), name).unwrap();
    write_token_file(&raw.token, &flat.token).unwrap();
    write_token_file(&raw.meta, &flat.meta).unwrap();
    write_token_file(&raw.client, &flat.client).unwrap();
    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    assert!(migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());
    let cursor = manifest_hermes_cursor(
        &read_credential_manifest(temp.path(), name)
            .unwrap()
            .unwrap(),
    );
    let mut refreshed = flat.clone();
    refreshed.token.access_token = "newt-refreshed".into();
    refreshed.token.refresh_token = Some("newt-refresh-rotated".into());
    publish_credential_generation(temp.path(), name, &refreshed, cursor, &transaction).unwrap();

    assert!(!migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());
    let active = read_credential_bundle(temp.path(), name).unwrap().unwrap();
    assert_eq!(active.token.access_token, "newt-refreshed");
    let untouched: TokenFile = read_json(&raw.token).unwrap().unwrap();
    assert_eq!(untouched.access_token, "flat-old");
}

/// Real-filesystem grounding for mocked coherent-trio cursor checks.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn hermes_rotation_is_refused_after_a_byte_only_metadata_change() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let old = credential_test_bundle("old", false);
    let raw = exact_raw_paths(temp.path(), name).unwrap();
    write_token_file(&raw.token, &old.token).unwrap();
    write_token_file(&raw.meta, &old.meta).unwrap();
    write_token_file(&raw.client, &old.client).unwrap();
    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    assert!(migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());

    let compact_meta = serde_json::to_vec(&old.meta).unwrap();
    newt_core::atomic_fs::ResolvedPath::resolve(&raw.meta)
        .unwrap()
        .atomic_write_private(&compact_meta)
        .unwrap();
    let mut rotated = old.token.clone();
    rotated.access_token = "should-not-import".into();
    write_token_file(&raw.token, &rotated).unwrap();

    assert!(!migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());
    let active = read_credential_bundle(temp.path(), name).unwrap().unwrap();
    assert_eq!(active.token.access_token, "old");
}

/// Real-filesystem grounding for mocked version-one manifest adoption.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn version_one_manifest_upgrades_and_adopts_one_coherent_rotation() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let old = credential_test_bundle("old", false);
    let raw = exact_raw_paths(temp.path(), name).unwrap();
    write_token_file(&raw.token, &old.token).unwrap();
    write_token_file(&raw.meta, &old.meta).unwrap();
    write_token_file(&raw.client, &old.client).unwrap();
    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    assert!(migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());

    let mut legacy_manifest = read_credential_manifest(temp.path(), name)
        .unwrap()
        .unwrap();
    legacy_manifest.version = 1;
    legacy_manifest.hermes_token_sha256 = None;
    legacy_manifest.hermes_meta_sha256 = None;
    legacy_manifest.hermes_client_sha256 = None;
    transaction
        .manifest_destination
        .atomic_write_private(&serde_json::to_vec_pretty(&legacy_manifest).unwrap())
        .unwrap();
    let mut rotated = old.token.clone();
    rotated.access_token = "rotated-before-upgrade".into();
    write_token_file(&raw.token, &rotated).unwrap();

    assert!(migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());
    let active = read_credential_bundle(temp.path(), name).unwrap().unwrap();
    assert_eq!(active.token.access_token, "rotated-before-upgrade");
    let upgraded = read_credential_manifest(temp.path(), name)
        .unwrap()
        .unwrap();
    assert_eq!(upgraded.version, CREDENTIAL_MANIFEST_VERSION);
    assert!(upgraded.hermes_token_sha256.is_some());
}

/// Real-filesystem grounding for mocked version-one binding enforcement.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn version_one_manifest_upgrade_refuses_unbound_rotation() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let old = credential_test_bundle("old", false);
    let raw = exact_raw_paths(temp.path(), name).unwrap();
    write_token_file(&raw.token, &old.token).unwrap();
    write_token_file(&raw.meta, &old.meta).unwrap();
    write_token_file(&raw.client, &old.client).unwrap();
    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    assert!(migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());

    let mut legacy_manifest = read_credential_manifest(temp.path(), name)
        .unwrap()
        .unwrap();
    legacy_manifest.version = 1;
    legacy_manifest.hermes_token_sha256 = None;
    legacy_manifest.hermes_meta_sha256 = None;
    legacy_manifest.hermes_client_sha256 = None;
    transaction
        .manifest_destination
        .atomic_write_private(&serde_json::to_vec_pretty(&legacy_manifest).unwrap())
        .unwrap();

    let mut unbound = old.token.clone();
    unbound.access_token = "unbound-before-upgrade".into();
    unbound.resource = None;
    unbound.issuer = None;
    write_token_file(&raw.token, &unbound).unwrap();

    assert!(migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());
    let active = read_credential_bundle(temp.path(), name).unwrap().unwrap();
    assert_eq!(
        active.token.access_token, "old",
        "v1 migration must not bind an external token by association"
    );
    let upgraded = read_credential_manifest(temp.path(), name)
        .unwrap()
        .unwrap();
    assert_eq!(upgraded.version, CREDENTIAL_MANIFEST_VERSION);
    assert!(upgraded.hermes_token_sha256.is_some());
}

/// Real-filesystem grounding for mocked legacy reauthentication classification.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn unbound_hermes_trio_is_withheld_and_left_for_explicit_reauth() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let raw = exact_raw_paths(temp.path(), name).unwrap();
    write_token_file(
        &raw.token,
        &serde_json::json!({
            "access_token": "legacy-access",
            "refresh_token": "legacy-refresh",
            "token_type": "Bearer"
        }),
    )
    .unwrap();
    write_token_file(
        &raw.meta,
        &serde_json::json!({
            "issuer": "https://auth.example.test",
            "authorization_endpoint": "https://auth.example.test/authorize",
            "token_endpoint": "https://auth.example.test/token",
            "code_challenge_methods_supported": ["S256"]
        }),
    )
    .unwrap();
    write_token_file(
        &raw.client,
        &serde_json::json!({
            "client_id": "legacy-client",
            "redirect_uris": ["http://127.0.0.1:0/callback"],
            "token_endpoint_auth_method": "none"
        }),
    )
    .unwrap();

    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    assert!(!migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());
    assert!(!credential_manifest_path(temp.path(), name).exists());
    let active = read_credential_bundle(temp.path(), name).unwrap().unwrap();
    assert!(active.meta.resource.is_empty());
    assert!(active.client.issuer.is_none());
    assert_eq!(
        classify_auth_state(
            "https://mcp.example.test/team/mcp",
            Some(&active.token),
            Some(&active.meta),
            Some(&active.client),
            unix_now(),
        ),
        AuthState::NeedsMigration
    );
}

/// Real-filesystem grounding for mocked manifested-generation precedence.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn manifested_newt_generation_survives_malformed_read_only_hermes_trio() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let repaired = credential_test_bundle("repaired", false);
    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    publish_credential_generation(
        temp.path(),
        name,
        &repaired,
        HermesCursor::default(),
        &transaction,
    )
    .unwrap();

    let raw = exact_raw_paths(temp.path(), name).unwrap();
    let malformed = b"{not-json";
    newt_core::atomic_fs::ResolvedPath::resolve(&raw.token)
        .unwrap()
        .atomic_write_private(malformed)
        .unwrap();
    write_token_file(&raw.meta, &repaired.meta).unwrap();
    write_token_file(&raw.client, &repaired.client).unwrap();

    assert!(!migrate_legacy_credentials(temp.path(), name, &transaction).unwrap());
    let active = read_credential_bundle(temp.path(), name).unwrap().unwrap();
    assert_eq!(active.token.access_token, "repaired");
    assert_eq!(std::fs::read(&raw.token).unwrap(), malformed.to_vec());
}

/// Real-filesystem grounding for mocked credential snapshot interleaving.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn credential_snapshot_detects_deterministic_interleaving() {
    let temp = tempfile::tempdir().unwrap();
    let name = "server";
    let initial = credential_snapshot(temp.path(), name).unwrap();
    write_token_file(
        &token_path(temp.path(), name, ".meta.json").unwrap(),
        &serde_json::json!({"issuer":"B"}),
    )
    .unwrap();
    assert!(ensure_credential_snapshot(temp.path(), name, &initial).is_err());
}

/// Real-filesystem grounding for mocked manifest digest validation.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn version_two_manifest_rejects_malformed_cursor_digests() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    publish_credential_generation(
        temp.path(),
        name,
        &credential_test_bundle("old", false),
        HermesCursor::default(),
        &transaction,
    )
    .unwrap();
    let mut manifest = read_credential_manifest(temp.path(), name)
        .unwrap()
        .unwrap();
    manifest.hermes_token_sha256 = Some("NOT-A-DIGEST".into());
    transaction
        .manifest_destination
        .atomic_write_private(&serde_json::to_vec_pretty(&manifest).unwrap())
        .unwrap();
    assert!(read_credential_manifest(temp.path(), name).is_err());
}

/// Real-filesystem grounding for mocked refresh compare-and-swap checks.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn refresh_cas_rejects_any_flat_state_change() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let old = credential_test_bundle("old", false);
    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    publish_credential_generation(
        temp.path(),
        name,
        &old,
        HermesCursor::default(),
        &transaction,
    )
    .unwrap();
    let snapshot = refresh_snapshot(temp.path(), name, &old).unwrap();
    drop(transaction);

    let raw = exact_raw_paths(temp.path(), name).unwrap();
    let mut partial_meta = old.meta.clone();
    partial_meta
        .extra
        .insert("external-write".into(), serde_json::json!(true));
    write_token_file(&raw.meta, &partial_meta).unwrap();
    let refreshed = credential_test_bundle("refreshed", false);
    let error =
        persist_refreshed_bundle(temp.path(), name, &snapshot, &refreshed, &old.meta.resource)
            .unwrap_err()
            .to_string();
    assert!(
        error.contains("concurrent MCP credential change"),
        "{error}"
    );
    let active = read_credential_bundle(temp.path(), name).unwrap().unwrap();
    assert_eq!(active.token.access_token, "old");
}

/// Real-filesystem grounding for mocked refresh-race recovery.
#[tokio::test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
async fn forced_refresh_returns_an_already_rotated_bearer_without_network() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let current = credential_test_bundle("current", false);
    let transaction = acquire_credential_lock(temp.path(), name).unwrap();
    publish_credential_generation(
        temp.path(),
        name,
        &current,
        HermesCursor::default(),
        &transaction,
    )
    .unwrap();
    drop(transaction);

    let bearer = refresh_bearer_token_from_dir(
        name,
        &current.meta.resource,
        "rejected",
        temp.path(),
        &test_oauth_policy(),
    )
    .await;
    assert_eq!(bearer, Ok(Some("current".to_string())));
}

/// Real-filesystem grounding for mocked case-folded credential locking.
#[test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
fn credential_lock_serializes_case_aliases_and_releases_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    let upper = credential_lock_path(temp.path(), "Review.Source").unwrap();
    let lower = credential_lock_path(temp.path(), "review.source").unwrap();
    assert_eq!(upper, lower);
    let first = acquire_credential_lock(temp.path(), "Review.Source").unwrap();
    assert!(upper.is_file());

    let directory = temp.path().to_path_buf();
    let (sent, received) = std::sync::mpsc::channel();
    let contender = std::thread::spawn(move || {
        let _second = acquire_credential_lock(&directory, "review.source").unwrap();
        sent.send(()).unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(75));
    assert!(
        received.try_recv().is_err(),
        "case aliases must not overlap"
    );
    drop(first);
    received
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    contender.join().unwrap();
    assert!(!upper.exists());
}

#[test]
fn refresh_exchange_repeats_the_bound_resource_indicator() {
    let form = refresh_form("refresh-secret", "https://mcp.example.test/team-a/mcp");
    assert_eq!(
        form,
        vec![
            ("grant_type".into(), "refresh_token".into()),
            ("refresh_token".into(), "refresh-secret".into()),
            (
                "resource".into(),
                "https://mcp.example.test/team-a/mcp".into()
            ),
        ]
    );
}

// --- #3028 follow-up: durable rejection record + typed refresh outcome -------

/// A bundle whose token endpoint is `token_endpoint` (a loopback mock).
fn bundle_with_token_endpoint(access_token: &str, token_endpoint: &str) -> CredentialBundle {
    let mut bundle = credential_test_bundle(access_token, false);
    bundle.meta.token_endpoint = token_endpoint.into();
    bundle
}

async fn invalid_grant_token_endpoint() -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/token"))
        .respond_with(
            wiremock::ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({"error": "invalid_grant"})),
        )
        .mount(&server)
        .await;
    server
}

fn publish(dir: &Path, name: &str, bundle: &CredentialBundle) {
    let transaction = acquire_credential_lock(dir, name).unwrap();
    publish_credential_generation(dir, name, bundle, HermesCursor::default(), &transaction)
        .unwrap();
}

fn state_of(dir: &Path, name: &str, resource: &str) -> AuthState {
    auth_status_from_dir(&[(name.into(), resource.into())], dir)
        .pop()
        .unwrap()
        .state
}

/// The record binds the resource and the token's RawContentId, never the
/// token bytes, and matches only the exact token it was minted for.
#[test]
fn rejection_record_binds_resource_and_token_identity_not_bytes_3028() {
    let bundle = credential_test_bundle("bearer-secret-7f3a9c", false);
    let record = RejectionRecord::for_token(&bundle.meta.resource, &bundle.token);
    assert!(record.matches(&bundle.meta.resource, &bundle.token));
    let replaced = credential_test_bundle("bearer-replacement-11b2", false);
    assert!(!record.matches(&bundle.meta.resource, &replaced.token));
    assert!(!record.matches("https://mcp.example.test/other/mcp", &bundle.token));
    let stored = serde_json::to_string(&record).unwrap();
    assert!(!stored.contains("bearer-secret-7f3a9c"), "{stored}");
}

/// A bundle with no refresh grant: the forced refresh after a 401 fails
/// with no network, the failure the durable record must capture.
fn bundle_without_refresh(access_token: &str) -> CredentialBundle {
    let mut bundle = credential_test_bundle(access_token, false);
    bundle.token.refresh_token = None;
    bundle
}

/// A resource 401 followed by a failed forced refresh is recorded durably: a
/// FRESH reader of the same token directory — what `newt auth --all` is —
/// sees NeedsFlow and selects the server, although the token has no expiry
/// and looked valid. Grounds the mocked `rejection_record_*` unit test.
#[tokio::test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
async fn rejected_bearer_is_selected_by_a_fresh_cli_reader_3028() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let bundle = bundle_without_refresh("current");
    let resource = bundle.meta.resource.clone();
    publish(temp.path(), name, &bundle);
    assert_eq!(state_of(temp.path(), name, &resource), AuthState::Valid);

    let outcome = refresh_bearer_token_from_dir(
        name,
        &resource,
        "current",
        temp.path(),
        &test_oauth_policy(),
    )
    .await;
    assert_eq!(outcome, Ok(None));

    // Fresh process: no Mcp state, only the token directory.
    assert_eq!(state_of(temp.path(), name, &resource), AuthState::NeedsFlow);
    let mut entry: newt_core::mcp::McpServerEntry = toml::from_str(&format!(
        "name = '{name}'\ntype = 'http'\nurl = '{resource}'\n"
    ))
    .unwrap();
    entry.trust = newt_core::mcp::McpTrust::Trusted;
    let targets = crate::auth_command::login_all_targets(&[entry], &|_| None, &|entry| {
        state_of(temp.path(), &entry.name, entry.url.as_deref().unwrap()) == AuthState::Valid
    });
    assert_eq!(
        targets.len(),
        1,
        "the rejected server must be signed in again"
    );
}

/// A successful login replaces the rejected token: the new token is not
/// poisoned by the old record, and the stale record is cleared.
#[tokio::test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
async fn a_replaced_token_is_not_poisoned_by_an_old_rejection_3028() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let rejected = bundle_without_refresh("current");
    let resource = rejected.meta.resource.clone();
    publish(temp.path(), name, &rejected);
    let _ = refresh_bearer_token_from_dir(
        name,
        &resource,
        "current",
        temp.path(),
        &test_oauth_policy(),
    )
    .await;
    assert_eq!(state_of(temp.path(), name, &resource), AuthState::NeedsFlow);

    publish(
        temp.path(),
        name,
        &credential_test_bundle("fresh-login", false),
    );
    assert_eq!(state_of(temp.path(), name, &resource), AuthState::Valid);
    assert!(
        !rejection_path(temp.path(), name).exists(),
        "stale record cleared"
    );
}

/// The token endpoint's `invalid_grant` on an expired token is a typed
/// outcome of the real load path, and it reaches the connection status the
/// startup table, Reconnect and the panel render.
#[tokio::test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
async fn token_endpoint_invalid_grant_reaches_the_status_message_3028() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let endpoint = invalid_grant_token_endpoint().await;
    let mut bundle = bundle_with_token_endpoint("expired", &format!("{}/token", endpoint.uri()));
    bundle.token.expires_at = Some(1.0);
    let resource = bundle.meta.resource.clone();
    publish(temp.path(), name, &bundle);

    let loaded =
        load_bearer_token_from_dir(name, &resource, temp.path(), &test_oauth_policy()).await;
    assert_eq!(
        loaded,
        LoadedBearer {
            token: None,
            grant_rejected: Some(OAuthGrantRejected),
        }
    );

    // Reconnect path: a resource 401, then the forced refresh through the
    // real refresh function, refused with `invalid_grant`.
    let current = bundle_with_token_endpoint("current", &format!("{}/token", endpoint.uri()));
    publish(temp.path(), name, &current);
    let dir = temp.path().to_path_buf();
    let failed = crate::mcp::retry_http_unauthorized_once(
        Err::<(), _>(anyhow::Error::new(newt_mcp_client::HttpStatusError::new(
            401,
            "Unauthorized",
            "",
        ))),
        Some("current".to_string()),
        || async {
            refresh_bearer_token_from_dir(name, &resource, "current", &dir, &test_oauth_policy())
                .await
        },
        |_| {},
        |_| async { anyhow::Ok(()) },
    )
    .await
    .err()
    .expect("a refused refresh cannot reconnect");
    let status = crate::mcp::McpStatus::from_error(&failed);
    let crate::mcp::McpStatus::Rejected(reason) = status else {
        panic!("a refused grant is a rejected credential: {status:?}");
    };
    assert!(reason.contains("invalid_grant"), "{reason}");
}

/// Review of #3032: a refresh that SUCCEEDS but whose replacement bearer the
/// resource also refuses (401 → refresh → 401) is recorded against the
/// replacement, so a fresh reader — `newt auth --all` — selects the server.
#[tokio::test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
async fn a_refreshed_bearer_refused_again_is_selected_by_a_fresh_reader_3028() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let original = credential_test_bundle("current", false);
    let resource = original.meta.resource.clone();
    publish(temp.path(), name, &original);
    let dir = temp.path().to_path_buf();

    let result = crate::mcp::retry_http_unauthorized_once(
        Err::<(), _>(anyhow::Error::new(newt_mcp_client::HttpStatusError::new(
            401,
            "Unauthorized",
            "",
        ))),
        Some("current".to_string()),
        // A successful refresh publishes the replacement, as persist does.
        || async {
            publish(&dir, name, &credential_test_bundle("refreshed", false));
            Ok(Some("refreshed".to_string()))
        },
        |refused| record_bearer_rejected_in(&dir, name, &resource, refused),
        |_| async {
            Err(anyhow::Error::new(newt_mcp_client::HttpStatusError::new(
                401,
                "Unauthorized",
                "",
            )))
        },
    )
    .await;
    assert!(result.is_err(), "no second retry");

    assert_eq!(state_of(temp.path(), name, &resource), AuthState::NeedsFlow);
    let mut entry: newt_core::mcp::McpServerEntry = toml::from_str(&format!(
        "name = '{name}'\ntype = 'http'\nurl = '{resource}'\n"
    ))
    .unwrap();
    entry.trust = newt_core::mcp::McpTrust::Trusted;
    let targets = crate::auth_command::login_all_targets(&[entry], &|_| None, &|entry| {
        state_of(temp.path(), &entry.name, entry.url.as_deref().unwrap()) == AuthState::Valid
    });
    assert_eq!(targets.len(), 1);

    // A concurrent replacement after the refusal is not poisoned.
    publish(
        temp.path(),
        name,
        &credential_test_bundle("other-login", false),
    );
    record_bearer_rejected_in(temp.path(), name, &resource, "refreshed");
    assert_eq!(state_of(temp.path(), name, &resource), AuthState::Valid);
}

/// Review of #3032: an unknown-lifetime token whose proactive refresh is
/// refused (`invalid_grant`) still falls back to the current bearer, and
/// the refusal travels with it: a following NON-401 connection failure is
/// reported Rejected, with the login remediation.
#[tokio::test]
#[ignore = "real filesystem; weekly/release mcp-import-real lane"]
async fn a_fallback_bearer_carries_the_refused_grant_to_a_non_401_failure_3028() {
    let temp = tempfile::tempdir().unwrap();
    let name = "Review.Source";
    let endpoint = invalid_grant_token_endpoint().await;
    let bundle = bundle_with_token_endpoint("current", &format!("{}/token", endpoint.uri()));
    assert!(bundle.token.expires_at.is_none() && bundle.token.refresh_token.is_some());
    let resource = bundle.meta.resource.clone();
    publish(temp.path(), name, &bundle);

    let loaded =
        load_bearer_token_from_dir(name, &resource, temp.path(), &test_oauth_policy()).await;
    assert_eq!(
        loaded,
        LoadedBearer {
            token: Some("current".to_string()),
            grant_rejected: Some(OAuthGrantRejected),
        }
    );

    let failure = crate::mcp::attach_grant_rejection(
        anyhow::anyhow!("MCP initialize failed: connection reset"),
        loaded.grant_rejected,
    );
    let mut mcp = crate::mcp::Mcp::empty();
    mcp.statuses = vec![(name.into(), crate::mcp::McpStatus::from_error(&failure))];
    let line = &mcp.status_lines()[1];
    assert!(line.contains("invalid_grant"), "{line}");
    assert!(line.contains(crate::mcp::LOGIN_REMEDIATION), "{line}");
}
