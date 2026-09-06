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
    assert_eq!(bearer.as_deref(), Some("current"));
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
