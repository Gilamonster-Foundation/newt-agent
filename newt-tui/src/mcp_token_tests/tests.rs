use super::*;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_oauth_policy() -> OAuthHopPolicy {
    OAuthHopPolicy::new(&newt_core::Scope::All)
}

#[test]
fn unix_now_is_plausible() {
    assert!(unix_now() > 1_735_689_600.0);
}

#[test]
fn pkce_verifier_and_challenge_are_distinct() {
    let p = gen_pkce().unwrap();
    assert!(!p.verifier.is_empty());
    assert!(!p.challenge.is_empty());
    assert_ne!(p.verifier, p.challenge);
    // Verifier must be base64url-safe (no + / =).
    assert!(!p.verifier.contains('+'));
    assert!(!p.verifier.contains('/'));
    assert!(!p.verifier.contains('='));
}

#[test]
fn pkce_challenge_matches_sha256_of_verifier() {
    let p = gen_pkce().unwrap();
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let expected = engine.encode(Sha256::digest(p.verifier.as_bytes()));
    assert_eq!(p.challenge, expected);
}

#[test]
fn oauth_hops_consume_the_complete_network_scope() {
    let denied = OAuthHopPolicy::new(&newt_core::Scope::none());
    let error = validate_discovery_hop_with_policy(
        "https://auth.example.test/token",
        "token endpoint",
        false,
        &denied,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("outside the session network capability"),
        "{error}"
    );

    let allowed = OAuthHopPolicy::new(&newt_core::Scope::only(["auth.example.test".to_string()]));
    assert!(validate_discovery_hop_with_policy(
        "https://auth.example.test/token",
        "token endpoint",
        false,
        &allowed,
    )
    .is_ok());
    assert!(validate_discovery_hop_with_policy(
        "https://registration.example.test/client",
        "registration endpoint",
        false,
        &allowed,
    )
    .is_err());
}

#[test]
fn saved_dcr_requires_grants_response_type_and_requested_scope() {
    let base = ClientFile {
        client_id: "client".into(),
        redirect_uris: vec!["http://127.0.0.1:0/callback".into()],
        issuer: Some("https://auth.example.test".into()),
        extra: BTreeMap::from([
            ("registration_method".into(), serde_json::json!("dcr")),
            (
                "token_endpoint_auth_method".into(),
                serde_json::json!("none"),
            ),
            (
                "grant_types".into(),
                serde_json::json!(["authorization_code", "refresh_token"]),
            ),
            ("scope".into(), serde_json::json!("files:read profile")),
        ]),
    };
    assert!(dcr_registration_is_eligible(&base, Some("files:read")).is_err());
    let mut eligible = base;
    eligible
        .extra
        .insert("response_types".into(), serde_json::json!(["code"]));
    assert!(dcr_registration_is_eligible(&eligible, Some("files:read")).is_ok());
    assert!(dcr_registration_is_eligible(&eligible, Some("files:write")).is_err());
}

#[test]
fn parse_callback_extracts_code_and_state() {
    let callback =
        parse_callback("/callback?code=AUTH_CODE_HERE&state=abc123&iss=https%3A%2F%2Fauth.example");
    assert_eq!(callback.code.as_deref(), Some("AUTH_CODE_HERE"));
    assert_eq!(callback.state.as_deref(), Some("abc123"));
    assert_eq!(callback.issuer.as_deref(), Some("https://auth.example"));
}

#[test]
fn parse_callback_returns_none_for_missing_params() {
    let callback = parse_callback("/callback?error=access_denied");
    assert!(callback.code.is_none());
    assert!(callback.state.is_none());
    assert_eq!(callback.error.as_deref(), Some("access_denied"));
}

#[test]
fn urlencoding_encode_encodes_special_chars() {
    let encoded = urlencoding_encode("http://127.0.0.1:8080/callback");
    assert!(encoded.starts_with("http%3A%2F%2F127.0.0.1%3A8080%2Fcallback"));
}

#[test]
fn urlencoding_encode_leaves_unreserved_chars() {
    let s = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
    assert_eq!(urlencoding_encode(s), s);
}

#[test]
fn urlencoding_decode_round_trips() {
    let original = "hello world/test+foo";
    let encoded = urlencoding_encode(original);
    let decoded = urlencoding_decode(&encoded);
    assert_eq!(decoded, original);
}

#[test]
fn protected_resource_well_known_inserts_before_a_path() {
    assert_eq!(
        protected_resource_metadata_url("https://mcp.example.test/teams/red/mcp?region=us")
            .unwrap(),
        "https://mcp.example.test/.well-known/oauth-protected-resource/teams/red/mcp?region=us"
    );
}

#[test]
fn canonical_resource_preserves_origin_path_identity() {
    let bare = validate_https_resource("https://MCP.EXAMPLE.test:443").unwrap();
    assert_eq!(
        canonical_resource_identifier("https://MCP.EXAMPLE.test:443", &bare),
        "https://mcp.example.test"
    );

    let bare_query = validate_https_resource("https://mcp.example.test?tenant=red").unwrap();
    assert_eq!(
        canonical_resource_identifier("https://mcp.example.test?tenant=red", &bare_query),
        "https://mcp.example.test?tenant=red"
    );

    let slash = validate_https_resource("https://mcp.example.test/").unwrap();
    assert_eq!(
        canonical_resource_identifier("https://mcp.example.test/", &slash),
        "https://mcp.example.test/"
    );
}

#[test]
fn production_discovery_rejects_plain_http_even_on_loopback() {
    assert!(protected_resource_metadata_url("http://127.0.0.1:9000/mcp").is_err());
    assert!(authorization_server_metadata_url("http://127.0.0.1:9001").is_err());
    assert!(resource_origin("http://127.0.0.1:9000/mcp").is_err());
}

#[test]
fn parses_resource_metadata_from_bearer_challenge() {
    let value = r#"Bearer realm="mcp", resource_metadata="https://mcp.example.test/oauth-meta""#;
    let parsed = parse_bearer_challenge(value).unwrap().unwrap();
    assert_eq!(
        parsed.resource_metadata.as_deref(),
        Some("https://mcp.example.test/oauth-meta")
    );
}

#[tokio::test]
async fn discovers_pathful_resource_then_separate_authorization_server() {
    let resource_server = MockServer::start().await;
    let authorization_server = MockServer::start().await;
    let resource = format!("{}/teams/red/mcp", resource_server.uri());
    let issuer = format!("{}/tenant/example", authorization_server.uri());

    Mock::given(method("POST"))
        .and(path("/teams/red/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/teams/red/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [issuer],
        })))
        .expect(1)
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/.well-known/oauth-authorization-server/tenant/example",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{}/authorize", authorization_server.uri()),
            "token_endpoint": format!("{}/token", authorization_server.uri()),
            "code_challenge_methods_supported": ["S256"],
        })))
        .expect(1)
        .mount(&authorization_server)
        .await;

    let discovered = discover_oauth_meta_with_policy(&resource, true)
        .await
        .unwrap();
    assert_eq!(discovered.resource, resource);
    assert_eq!(discovered.issuer, issuer);
    resource_server.verify().await;
    authorization_server.verify().await;
}

#[tokio::test]
async fn prefers_www_authenticate_resource_metadata() {
    let resource_server = MockServer::start().await;
    let authorization_server = MockServer::start().await;
    let resource = format!("{}/mcp/project-a", resource_server.uri());
    let metadata_url = format!("{}/oauth/resource/project-a", resource_server.uri());
    let issuer = authorization_server.uri();
    let challenge = format!("Bearer resource_metadata=\"{metadata_url}\"");

    Mock::given(method("POST"))
        .and(path("/mcp/project-a"))
        .respond_with(
            ResponseTemplate::new(401).insert_header("WWW-Authenticate", challenge.as_str()),
        )
        .expect(1)
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/oauth/resource/project-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [issuer],
        })))
        .expect(1)
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{}/authorize", authorization_server.uri()),
            "token_endpoint": format!("{}/token", authorization_server.uri()),
            "code_challenge_methods_supported": ["S256"],
        })))
        .expect(1)
        .mount(&authorization_server)
        .await;

    let discovered = discover_oauth_meta_with_policy(&resource, true)
        .await
        .unwrap();
    assert_eq!(discovered.resource, resource);
    assert_eq!(discovered.issuer, issuer);
    resource_server.verify().await;
    authorization_server.verify().await;
}

#[tokio::test]
async fn missing_protected_resource_metadata_does_not_use_legacy_origin_fallback() {
    let server = MockServer::start().await;
    let resource = format!("{}/nested/mcp", server.uri());

    Mock::given(method("POST"))
        .and(path("/nested/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/nested/mcp"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let error = discover_oauth_meta_with_policy(&resource, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("required RFC 9728 metadata"), "{error}");
    server.verify().await;
}

#[tokio::test]
async fn malformed_protected_resource_metadata_does_not_downgrade_to_legacy() {
    let server = MockServer::start().await;
    let resource = format!("{}/nested/mcp", server.uri());

    Mock::given(method("POST"))
        .and(path("/nested/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/nested/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": "https://attacker.invalid/mcp",
            "authorization_servers": [server.uri()],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let err = discover_oauth_meta_with_policy(&resource, true)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("resource mismatch"), "{err:#}");
    server.verify().await;
}

#[test]
fn stored_bearer_resource_binding_is_exact_but_origin_case_is_canonical() {
    let bound = "https://MCP.EXAMPLE.test/team-a/mcp?tenant=red";
    assert!(resource_matches(
        bound,
        "https://mcp.example.test/team-a/mcp?tenant=red"
    ));
    assert!(!resource_matches(
        bound,
        "https://mcp.example.test/team-b/mcp?tenant=red"
    ));
    assert!(!resource_matches(
        bound,
        "https://mcp.example.test/team-a/mcp?tenant=blue"
    ));
    assert!(!resource_matches(
        bound,
        "https://other.example.test/team-a/mcp?tenant=red"
    ));
    assert!(!resource_matches(
        "https://mcp.example.test",
        "https://mcp.example.test/"
    ));
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
fn challenge_parser_uses_exact_names_and_quoted_escapes() {
    let misleading = r#"Bearer realm="resource_metadata=\"https://evil.test\"", foo_resource_metadata="https://evil.test""#;
    let parsed = parse_bearer_challenge(misleading).unwrap().unwrap();
    assert!(parsed.resource_metadata.is_none());

    let valid = r#"Bearer scope="files:read files:write", resource_metadata="https://mcp.example/meta\"v2""#;
    let parsed = parse_bearer_challenge(valid).unwrap().unwrap();
    assert_eq!(parsed.scope.as_deref(), Some("files:read files:write"));
    assert_eq!(
        parsed.resource_metadata.as_deref(),
        Some("https://mcp.example/meta\"v2")
    );

    let combined = r#"Bearer resource_metadata="https://mcp.example/meta", Basic realm="legacy""#;
    let parsed = parse_bearer_challenge(combined).unwrap().unwrap();
    assert_eq!(
        parsed.resource_metadata.as_deref(),
        Some("https://mcp.example/meta")
    );
}

#[test]
fn redirect_policy_rejects_remote_plaintext_and_selects_loopback() {
    let unsafe_client = ClientFile {
        client_id: "client".into(),
        redirect_uris: vec!["http://evil.example/callback".into()],
        issuer: Some("https://auth.example".into()),
        extra: BTreeMap::new(),
    };
    assert!(callback_target(&unsafe_client).is_err());
    let missing_redirects = ClientFile {
        redirect_uris: Vec::new(),
        ..unsafe_client.clone()
    };
    assert!(callback_target(&missing_redirects).is_err());
    let omitted_port = ClientFile {
        redirect_uris: vec!["http://127.0.0.1/callback".into()],
        ..unsafe_client.clone()
    };
    assert!(callback_target(&omitted_port).is_err());

    let client = ClientFile {
        client_id: "client".into(),
        redirect_uris: vec![
            "https://app.example/callback".into(),
            "http://127.0.0.1:0/oauth/callback".into(),
        ],
        issuer: Some("https://auth.example".into()),
        extra: BTreeMap::new(),
    };
    let target = callback_target(&client).unwrap();
    assert!(target.bind_addr.ip().is_loopback());
    assert_eq!(target.path, "/oauth/callback");
}

#[test]
fn client_registration_never_crosses_issuers() {
    let client = ClientFile {
        client_id: "client".into(),
        redirect_uris: vec![],
        issuer: Some("https://old.example".into()),
        extra: BTreeMap::new(),
    };
    assert!(bind_client_registration(client, "https://new.example", false,).is_err());
}

#[test]
fn portable_client_metadata_id_can_rebind_without_losing_registration_fields() {
    let mut extra = BTreeMap::new();
    extra.insert(
        "token_endpoint_auth_method".into(),
        serde_json::Value::String("none".into()),
    );
    extra.insert(
        "registration_method".into(),
        serde_json::Value::String("cimd".into()),
    );
    let client = ClientFile {
        client_id: "https://client.example/oauth/client.json".into(),
        redirect_uris: vec!["http://127.0.0.1:0/callback".into()],
        issuer: Some("https://old.example".into()),
        extra,
    };
    let (rebound, changed) = bind_client_registration(client, "https://new.example", true).unwrap();
    assert!(changed);
    assert_eq!(rebound.issuer.as_deref(), Some("https://new.example"));
    assert_eq!(
        rebound.extra.get("token_endpoint_auth_method"),
        Some(&serde_json::Value::String("none".into()))
    );
}

#[test]
fn issuerless_as_local_client_never_auto_binds() {
    let client = ClientFile {
        client_id: "legacy-client".into(),
        redirect_uris: vec![],
        issuer: None,
        extra: BTreeMap::new(),
    };
    assert!(bind_client_registration(client, "https://auth.example", true,).is_err());
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
fn authorization_url_preserves_existing_query_and_adds_scope() {
    let url = build_authorization_url(
        "https://auth.example/authorize?audience=internal",
        "client id",
        "http://127.0.0.1:3456/callback",
        "challenge",
        "https://mcp.example/mcp",
        "state",
        Some("files:read files:write"),
    )
    .unwrap();
    let parsed = reqwest::Url::parse(&url).unwrap();
    let query: BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
    assert_eq!(query.get("audience").map(String::as_str), Some("internal"));
    assert_eq!(
        query.get("scope").map(String::as_str),
        Some("files:read files:write")
    );
    assert_eq!(
        query.get("resource").map(String::as_str),
        Some("https://mcp.example/mcp")
    );
}

#[test]
fn authorization_response_enforces_issuer_before_error() {
    let mismatch = CallbackParams {
        state: Some("state".into()),
        issuer: Some("https://attacker.example".into()),
        error: Some("attacker-controlled-description".into()),
        ..CallbackParams::default()
    };
    let error = validate_authorization_response(&mismatch, "state", "https://auth.example", true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("issuer mismatch"));
    assert!(!error.contains("attacker-controlled-description"));

    let absent = CallbackParams {
        state: Some("state".into()),
        code: Some("code".into()),
        ..CallbackParams::default()
    };
    assert!(
        validate_authorization_response(&absent, "state", "https://auth.example", true).is_err()
    );
}

#[test]
fn callback_request_parser_requires_get_and_an_origin_form_target() {
    assert_eq!(
        callback_request_target("GET /callback?code=x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap(),
        "/callback?code=x"
    );
    assert!(callback_request_target("POST /callback HTTP/1.1\r\n\r\n").is_err());
    assert!(
        callback_request_target("GET https://attacker.example/callback HTTP/1.1\r\n\r\n").is_err()
    );
}

#[test]
fn saved_as_local_issuer_is_the_only_authorization_server_candidate() {
    let client = ClientFile {
        client_id: "registered-client".into(),
        redirect_uris: vec![],
        issuer: Some("https://second.example".into()),
        extra: BTreeMap::new(),
    };
    assert_eq!(
        order_authorization_servers(
            vec![
                "https://first.example".into(),
                "https://second.example".into(),
            ],
            Some(&client),
            false,
        )
        .unwrap(),
        vec!["https://second.example"]
    );
    assert!(order_authorization_servers(
        vec!["https://first.example".into()],
        Some(&client),
        false
    )
    .is_err());
    assert_eq!(
        order_authorization_servers(vec!["https://first.example".into()], Some(&client), true)
            .unwrap(),
        vec!["https://first.example"]
    );
}

#[test]
fn scope_step_up_preserves_prior_grants_without_duplicates() {
    assert_eq!(
        merge_scopes(Some("files:write profile"), Some("files:read profile")),
        Some("files:write profile files:read".into())
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

#[test]
fn production_discovery_rejects_private_literal_hops() {
    assert!(validate_discovery_hop("https://127.0.0.1/oauth", "issuer", false).is_err());
    assert!(validate_discovery_hop("https://10.0.0.1/oauth", "issuer", false).is_err());
    assert!(validate_discovery_hop("https://auth.example/oauth", "issuer", false).is_ok());
}

#[tokio::test]
async fn falls_back_from_path_to_root_protected_resource_metadata() {
    let server = MockServer::start().await;
    let resource = format!("{}/nested/mcp", server.uri());
    let issuer = server.uri();
    Mock::given(method("POST"))
        .and(path("/nested/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/nested/mcp"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [issuer],
            "scopes_supported": ["files:read"]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .mount(&server)
        .await;
    let discovered = discover_oauth_meta_with_policy(&resource, true)
        .await
        .unwrap();
    assert_eq!(discovered.scope.as_deref(), Some("files:read"));
    server.verify().await;
}

#[tokio::test]
async fn supports_external_challenge_metadata_and_challenge_scope_priority() {
    let resource_server = MockServer::start().await;
    let metadata_server = MockServer::start().await;
    let authorization_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    let metadata_url = format!("{}/resource-meta", metadata_server.uri());
    let issuer = authorization_server.uri();
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(
            ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!("Bearer resource_metadata=\"{metadata_url}\", scope=\"challenge:scope\"")
                    .as_str(),
            ),
        )
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/resource-meta"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [issuer],
            "scopes_supported": ["metadata:scope"]
        })))
        .mount(&metadata_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{}/authorize", authorization_server.uri()),
            "token_endpoint": format!("{}/token", authorization_server.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .mount(&authorization_server)
        .await;
    let discovered = discover_oauth_meta_with_policy(&resource, true)
        .await
        .unwrap();
    assert_eq!(discovered.scope.as_deref(), Some("challenge:scope"));
}

#[tokio::test]
async fn falls_back_to_oidc_discovery_for_pathful_issuer() {
    let resource_server = MockServer::start().await;
    let authorization_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    let issuer = format!("{}/tenant/a", authorization_server.uri());
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [issuer]
        })))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server/tenant/a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": format!("{}/wrong-tenant", authorization_server.uri()),
            "authorization_endpoint": format!("{}/authorize", authorization_server.uri()),
            "token_endpoint": format!("{}/token", authorization_server.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .expect(1)
        .mount(&authorization_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration/tenant/a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{}/authorize", authorization_server.uri()),
            "token_endpoint": format!("{}/token", authorization_server.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .expect(1)
        .mount(&authorization_server)
        .await;
    let discovered = discover_oauth_meta_with_policy(&resource, true)
        .await
        .unwrap();
    assert_eq!(discovered.issuer, issuer);
    authorization_server.verify().await;
}

#[tokio::test]
async fn falls_back_to_path_appended_oidc_discovery() {
    let resource_server = MockServer::start().await;
    let authorization_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    let issuer = format!("{}/tenant/a", authorization_server.uri());
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [issuer]
        })))
        .mount(&resource_server)
        .await;
    for missing in [
        "/.well-known/oauth-authorization-server/tenant/a",
        "/.well-known/openid-configuration/tenant/a",
    ] {
        Mock::given(method("GET"))
            .and(path(missing))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&authorization_server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/tenant/a/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{}/authorize", authorization_server.uri()),
            "token_endpoint": format!("{}/token", authorization_server.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .expect(1)
        .mount(&authorization_server)
        .await;
    let discovered = discover_oauth_meta_with_policy(&resource, true)
        .await
        .unwrap();
    assert_eq!(discovered.issuer, issuer);
    authorization_server.verify().await;
}

#[tokio::test]
async fn selects_the_first_usable_advertised_authorization_server() {
    let resource_server = MockServer::start().await;
    let unusable_server = MockServer::start().await;
    let usable_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    let unusable_issuer = unusable_server.uri();
    let usable_issuer = usable_server.uri();
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [unusable_issuer, usable_issuer]
        })))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": unusable_issuer,
            "authorization_endpoint": format!("{}/authorize", unusable_server.uri()),
            "token_endpoint": format!("{}/token", unusable_server.uri()),
            "code_challenge_methods_supported": ["plain"]
        })))
        .expect(1)
        .mount(&unusable_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": usable_issuer,
            "authorization_endpoint": format!("{}/authorize", usable_server.uri()),
            "token_endpoint": format!("{}/token", usable_server.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .expect(1)
        .mount(&usable_server)
        .await;
    let discovered = discover_oauth_meta_with_policy(&resource, true)
        .await
        .unwrap();
    assert_eq!(discovered.issuer, usable_issuer);
    unusable_server.verify().await;
    usable_server.verify().await;
}

#[tokio::test]
async fn excessive_advertised_authorization_servers_are_rejected() {
    let resource_server = MockServer::start().await;
    let issuer_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    let issuers: Vec<String> = (0..=MAX_ADVERTISED_AUTHORIZATION_SERVERS)
        .map(|index| format!("{}/tenant/{index}", issuer_server.uri()))
        .collect();
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": issuers
        })))
        .mount(&resource_server)
        .await;

    let error = discover_oauth_meta_with_policy(&resource, true)
        .await
        .expect_err("an over-long authorization_servers list must be refused");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("above the limit"),
        "unexpected error: {rendered}"
    );
    // Rejected, not truncated: not one candidate is contacted.
    assert!(
        issuer_server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "an over-long list must not produce any issuer discovery traffic"
    );
}

#[tokio::test]
async fn discovery_walks_every_failing_candidate_up_to_the_limit() {
    let resource_server = MockServer::start().await;
    let failing_server = MockServer::start().await;
    let usable_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    let usable_issuer = usable_server.uri();
    let mut issuers: Vec<String> = (0..MAX_ADVERTISED_AUTHORIZATION_SERVERS - 1)
        .map(|index| format!("{}/tenant/{index}", failing_server.uri()))
        .collect();
    issuers.push(usable_issuer.clone());
    assert_eq!(issuers.len(), MAX_ADVERTISED_AUTHORIZATION_SERVERS);

    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": issuers
        })))
        .mount(&resource_server)
        .await;
    // `failing_server` has no matching mocks, so every metadata URL 404s.
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": usable_issuer,
            "authorization_endpoint": format!("{}/authorize", usable_server.uri()),
            "token_endpoint": format!("{}/token", usable_server.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .expect(1)
        .mount(&usable_server)
        .await;

    // A full-width advertised set is still honoured end to end: the bound
    // rejects excess, it does not narrow legitimate failover.
    let discovered = discover_oauth_meta_with_policy(&resource, true)
        .await
        .expect("the last candidate must still be reached");
    assert_eq!(discovered.issuer, usable_issuer);
    usable_server.verify().await;
}

#[tokio::test]
async fn stalling_issuer_candidates_cannot_outlive_the_discovery_budget() {
    let resource_server = MockServer::start().await;
    let first_server = MockServer::start().await;
    let second_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [first_server.uri(), second_server.uri()]
        })))
        .mount(&resource_server)
        .await;
    for server in [&first_server, &second_server] {
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(60))
                    .set_body_json(serde_json::json!({})),
            )
            .mount(server)
            .await;
    }

    let budget = DiscoveryBudget::new(std::time::Duration::from_millis(300));
    let started = std::time::Instant::now();
    let error = discover_oauth_meta_within_budget(&resource, true, &budget)
        .await
        .expect_err("stalling candidates must not stall discovery forever");
    let elapsed = started.elapsed();
    let rendered = format!("{error:#}");
    assert!(rendered.contains("budget"), "unexpected error: {rendered}");
    // Aggregate, not per-request: the second candidate never gets its own
    // fresh timeout once the shared budget is gone.
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "discovery ran {elapsed:?}, far past its 300ms budget"
    );
    assert!(
        second_server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "an exhausted budget must not fund another candidate"
    );
}

#[tokio::test]
async fn exhausted_discovery_budget_fails_closed_before_any_hop() {
    let resource_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    let budget = DiscoveryBudget::new(std::time::Duration::ZERO);
    assert!(budget.is_exhausted());

    let error = discover_oauth_meta_within_budget(&resource, true, &budget)
        .await
        .expect_err("an exhausted budget must fail closed");
    assert!(
        format!("{error:#}").contains("budget"),
        "unexpected error: {error:#}"
    );
    assert!(
        resource_server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "no external work may start once the budget is gone"
    );
}

#[tokio::test]
async fn unregistered_client_skips_pkce_server_without_dcr() {
    let resource_server = MockServer::start().await;
    let first_server = MockServer::start().await;
    let dcr_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    let first_issuer = first_server.uri();
    let dcr_issuer = dcr_server.uri();
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [first_issuer, dcr_issuer]
        })))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": first_issuer,
            "authorization_endpoint": format!("{}/authorize", first_server.uri()),
            "token_endpoint": format!("{}/token", first_server.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .expect(1)
        .mount(&first_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": dcr_issuer,
            "authorization_endpoint": format!("{}/authorize", dcr_server.uri()),
            "token_endpoint": format!("{}/token", dcr_server.uri()),
            "registration_endpoint": format!("{}/register", dcr_server.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .expect(1)
        .mount(&dcr_server)
        .await;
    let discovered = discover_oauth_meta_for_client_with_policy(
        &resource,
        true,
        None,
        true,
        &test_oauth_policy(),
    )
    .await
    .unwrap();
    assert_eq!(discovered.issuer, dcr_issuer);
    first_server.verify().await;
    dcr_server.verify().await;
}

#[tokio::test]
async fn reusable_saved_client_does_not_require_dynamic_registration() {
    let resource_server = MockServer::start().await;
    let authorization_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    let issuer = authorization_server.uri();
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [issuer]
        })))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{}/authorize", authorization_server.uri()),
            "token_endpoint": format!("{}/token", authorization_server.uri()),
            "code_challenge_methods_supported": ["S256"],
            "authorization_response_iss_parameter_supported": true
        })))
        .expect(1)
        .mount(&authorization_server)
        .await;
    let client = ClientFile {
        client_id: "existing-public-client".into(),
        redirect_uris: vec!["http://127.0.0.1:0/callback".into()],
        issuer: Some(issuer.clone()),
        extra: BTreeMap::new(),
    };

    let discovered = discover_oauth_meta_for_client_with_policy(
        &resource,
        true,
        Some(&client),
        true,
        &test_oauth_policy(),
    )
    .await
    .unwrap();
    assert_eq!(discovered.issuer, issuer);
    assert!(discovered.registration_endpoint.is_none());
    authorization_server.verify().await;
}

#[tokio::test]
async fn unusable_bound_issuer_falls_back_to_an_advertised_dcr_server() {
    let resource_server = MockServer::start().await;
    let old_server = MockServer::start().await;
    let replacement_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    let old_issuer = old_server.uri();
    let replacement_issuer = replacement_server.uri();
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [old_issuer, replacement_issuer]
        })))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": old_issuer,
            "authorization_endpoint": format!("{}/authorize", old_server.uri()),
            "token_endpoint": format!("{}/token", old_server.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .expect(1)
        .mount(&old_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": replacement_issuer,
            "authorization_endpoint": format!("{}/authorize", replacement_server.uri()),
            "token_endpoint": format!("{}/token", replacement_server.uri()),
            "registration_endpoint": format!("{}/register", replacement_server.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .expect(1)
        .mount(&replacement_server)
        .await;
    let client = ClientFile {
        client_id: "old-client".into(),
        redirect_uris: vec!["http://127.0.0.1:0/callback".into()],
        issuer: Some(old_issuer.clone()),
        extra: BTreeMap::new(),
    };

    let discovered = discover_oauth_meta_for_client_with_policy(
        &resource,
        true,
        Some(&client),
        true,
        &test_oauth_policy(),
    )
    .await
    .unwrap();
    assert_eq!(discovered.issuer, replacement_issuer);
    assert!(discovered.registration_endpoint.is_some());
    old_server.verify().await;
    replacement_server.verify().await;
}

#[tokio::test]
async fn unsupported_cimd_client_selects_dcr_replacement() {
    let resource_server = MockServer::start().await;
    let authorization_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    let issuer = authorization_server.uri();
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [issuer]
        })))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{}/authorize", authorization_server.uri()),
            "token_endpoint": format!("{}/token", authorization_server.uri()),
            "registration_endpoint": format!("{}/register", authorization_server.uri()),
            "code_challenge_methods_supported": ["S256"],
            "authorization_response_iss_parameter_supported": true
        })))
        .expect(1)
        .mount(&authorization_server)
        .await;
    let client = ClientFile {
        client_id: "https://client.example.test/oauth/client.json".into(),
        redirect_uris: vec!["http://127.0.0.1:0/callback".into()],
        issuer: Some(issuer.clone()),
        extra: BTreeMap::from([
            ("registration_method".into(), serde_json::json!("cimd")),
            (
                "token_endpoint_auth_method".into(),
                serde_json::json!("none"),
            ),
        ]),
    };

    let discovered = discover_oauth_meta_for_client_with_policy(
        &resource,
        true,
        Some(&client),
        true,
        &test_oauth_policy(),
    )
    .await
    .unwrap();
    assert!(discovered.registration_endpoint.is_some());
    assert!(
        resolve_client_registration(Some(client), &discovered, None, true, &test_oauth_policy(),)
            .await
            .is_err(),
        "the unmounted DCR endpoint proves unsupported CIMD was not reused"
    );
    authorization_server.verify().await;
}

#[tokio::test]
async fn invalid_cimd_id_skips_no_dcr_server_for_later_dcr_candidate() {
    let resource_server = MockServer::start().await;
    let first_server = MockServer::start().await;
    let dcr_server = MockServer::start().await;
    let resource = format!("{}/mcp", resource_server.uri());
    let first_issuer = first_server.uri();
    let dcr_issuer = dcr_server.uri();
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [first_issuer, dcr_issuer]
        })))
        .mount(&resource_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": first_issuer,
            "authorization_endpoint": format!("{}/authorize", first_server.uri()),
            "token_endpoint": format!("{}/token", first_server.uri()),
            "code_challenge_methods_supported": ["S256"],
            "authorization_response_iss_parameter_supported": true,
            "client_id_metadata_document_supported": true
        })))
        .expect(1)
        .mount(&first_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": dcr_issuer,
            "authorization_endpoint": format!("{}/authorize", dcr_server.uri()),
            "token_endpoint": format!("{}/token", dcr_server.uri()),
            "registration_endpoint": format!("{}/register", dcr_server.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .expect(1)
        .mount(&dcr_server)
        .await;
    let client = ClientFile {
        client_id: "not-an-https-metadata-document".into(),
        redirect_uris: vec!["http://127.0.0.1:0/callback".into()],
        issuer: Some(first_issuer),
        extra: BTreeMap::from([
            ("registration_method".into(), serde_json::json!("cimd")),
            (
                "token_endpoint_auth_method".into(),
                serde_json::json!("none"),
            ),
        ]),
    };

    let discovered = discover_oauth_meta_for_client_with_policy(
        &resource,
        true,
        Some(&client),
        true,
        &test_oauth_policy(),
    )
    .await
    .unwrap();
    assert_eq!(discovered.issuer, dcr_issuer);
    first_server.verify().await;
    dcr_server.verify().await;
}

#[tokio::test]
async fn rejects_authorization_server_without_s256() {
    let server = MockServer::start().await;
    let resource = format!("{}/mcp", server.uri());
    let issuer = server.uri();
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource,
            "authorization_servers": [issuer]
        })))
        .mount(&server)
        .await;
    for path_value in [
        "/.well-known/oauth-authorization-server",
        "/.well-known/openid-configuration",
    ] {
        Mock::given(method("GET"))
            .and(path(path_value))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{}/authorize", server.uri()),
                "token_endpoint": format!("{}/token", server.uri())
            })))
            .mount(&server)
            .await;
    }
    let error = discover_oauth_meta_with_policy(&resource, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("S256"), "{error}");
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
fn stale_scopes_are_not_carried_across_resource_or_issuer_bindings() {
    let mut extra = BTreeMap::new();
    extra.insert("scope".into(), serde_json::json!("old:admin"));
    let token = TokenFile {
        access_token: "access".into(),
        refresh_token: None,
        expires_at: None,
        resource: Some("https://mcp.example/old".into()),
        issuer: Some("https://auth.example/old".into()),
        extra,
    };
    let metadata = MetaFile {
        resource: "https://mcp.example/old".into(),
        issuer: "https://auth.example/old".into(),
        authorization_endpoint: None,
        token_endpoint: "https://auth.example/old/token".into(),
        code_challenge_methods_supported: vec!["S256".into()],
        authorization_response_iss_parameter_supported: false,
        extra: BTreeMap::new(),
    };
    assert_eq!(
        prior_scope_for_binding(
            Some(&token),
            Some(&metadata),
            "https://mcp.example/old",
            "https://auth.example/old"
        )
        .as_deref(),
        Some("old:admin")
    );
    assert!(prior_scope_for_binding(
        Some(&token),
        Some(&metadata),
        "https://mcp.example/new",
        "https://auth.example/old"
    )
    .is_none());
    assert!(prior_scope_for_binding(
        Some(&token),
        Some(&metadata),
        "https://mcp.example/old",
        "https://auth.example/new"
    )
    .is_none());
}

#[test]
fn unbound_or_partially_bound_scopes_cannot_widen_explicit_oauth() {
    let resource = "https://mcp.example/team/mcp";
    let issuer = "https://auth.example";
    let metadata = MetaFile {
        resource: resource.into(),
        issuer: issuer.into(),
        authorization_endpoint: None,
        token_endpoint: format!("{issuer}/token"),
        code_challenge_methods_supported: vec!["S256".into()],
        authorization_response_iss_parameter_supported: false,
        extra: BTreeMap::new(),
    };
    let scoped_token = |resource_binding: Option<&str>, issuer_binding: Option<&str>| TokenFile {
        access_token: "access".into(),
        refresh_token: None,
        expires_at: None,
        resource: resource_binding.map(str::to_owned),
        issuer: issuer_binding.map(str::to_owned),
        extra: BTreeMap::from([("scope".into(), serde_json::json!("admin"))]),
    };

    for token in [
        scoped_token(None, None),
        scoped_token(Some(resource), None),
        scoped_token(None, Some(issuer)),
    ] {
        assert!(
            prior_scope_for_binding(Some(&token), Some(&metadata), resource, issuer).is_none(),
            "an unbound or partially bound token must not widen explicit OAuth scopes"
        );
    }
}

#[test]
fn duplicate_issuer_and_hostile_callback_error_are_rejected_safely() {
    let duplicate = parse_callback(
        "/callback?state=s&code=c&iss=https%3A%2F%2Fauth.example&iss=https%3A%2F%2Fauth.example",
    );
    assert!(
        validate_authorization_response(&duplicate, "s", "https://auth.example", true).is_err()
    );

    let hostile = CallbackParams {
        state: Some("s".into()),
        issuer: Some("https://auth.example".into()),
        error: Some("bad\r\nsecret=value".into()),
        ..CallbackParams::default()
    };
    let message = validate_authorization_response(&hostile, "s", "https://auth.example", true)
        .unwrap_err()
        .to_string();
    assert_eq!(message, "OAuth authorization was rejected");
}

#[test]
fn confidential_cimd_registration_is_never_portable() {
    let mut extra = BTreeMap::new();
    extra.insert("registration_method".into(), serde_json::json!("cimd"));
    extra.insert(
        "token_endpoint_auth_method".into(),
        serde_json::json!("client_secret_basic"),
    );
    extra.insert("client_secret".into(), serde_json::json!("old-secret"));
    let client = ClientFile {
        client_id: "https://client.example/client.json".into(),
        redirect_uris: issuer_redirect_uris("https://old-auth.example"),
        issuer: Some("https://old-auth.example".into()),
        extra,
    };
    assert!(!client_is_portable_cimd(&client));
    assert!(bind_client_registration(client, "https://new-auth.example", true).is_err());
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

#[tokio::test]
async fn mixup_defense_uses_distinct_redirect_or_requires_issuer_response() {
    let issuer = "https://auth.example";
    let discovered = DiscoveredOAuthMeta {
        resource: "https://mcp.example/mcp".into(),
        issuer: issuer.into(),
        authorization_endpoint: format!("{issuer}/authorize"),
        token_endpoint: format!("{issuer}/token"),
        registration_endpoint: None,
        scope: None,
        code_challenge_methods_supported: vec!["S256".into()],
        authorization_response_iss_parameter_supported: false,
        extra: BTreeMap::new(),
    };
    let as_local = ClientFile {
        client_id: "local-client".into(),
        redirect_uris: issuer_redirect_uris(issuer),
        issuer: Some(issuer.into()),
        extra: BTreeMap::new(),
    };
    assert!(resolve_client_registration(
        Some(as_local),
        &discovered,
        None,
        false,
        &test_oauth_policy(),
    )
    .await
    .is_ok());

    let mut extra = BTreeMap::new();
    extra.insert("registration_method".into(), serde_json::json!("cimd"));
    extra.insert(
        "token_endpoint_auth_method".into(),
        serde_json::json!("none"),
    );
    let portable = ClientFile {
        client_id: "https://client.example/client.json".into(),
        redirect_uris: vec!["http://127.0.0.1:0/callback".into()],
        issuer: Some(issuer.into()),
        extra,
    };
    let error = resolve_client_registration(
        Some(portable),
        &discovered,
        None,
        false,
        &test_oauth_policy(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("RFC 9207"), "{error}");
}

#[tokio::test]
async fn legacy_client_is_safely_re_registered_with_refresh_and_scope_contract() {
    let server = MockServer::start().await;
    let issuer = server.uri();
    let redirects = issuer_redirect_uris(&issuer);
    Mock::given(method("POST"))
        .and(path("/register"))
        .and(body_json(serde_json::json!({
            "redirect_uris": redirects,
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "client_name": "newt-agent",
            "scope": "files:read profile"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "client_id": "new-public-client",
            "redirect_uris": issuer_redirect_uris(&issuer),
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "scope": "profile files:read"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let legacy: ClientFile = serde_json::from_value(serde_json::json!({
        "client_id": "legacy-client",
        "redirect_uris": ["http://127.0.0.1:0/callback"]
    }))
    .unwrap();
    let discovered = DiscoveredOAuthMeta {
        resource: "https://mcp.example/mcp".into(),
        issuer: issuer.clone(),
        authorization_endpoint: format!("{issuer}/authorize"),
        token_endpoint: format!("{issuer}/token"),
        registration_endpoint: Some(format!("{issuer}/register")),
        scope: Some("files:read profile".into()),
        code_challenge_methods_supported: vec!["S256".into()],
        authorization_response_iss_parameter_supported: false,
        extra: BTreeMap::new(),
    };
    let (registered, migrated) = resolve_client_registration(
        Some(legacy),
        &discovered,
        discovered.scope.as_deref(),
        true,
        &test_oauth_policy(),
    )
    .await
    .unwrap();
    assert!(migrated);
    assert_eq!(registered.client_id, "new-public-client");
    assert!(client_has_issuer_distinct_redirect(&registered, &issuer));
    server.verify().await;
}

#[tokio::test]
async fn issuer_bound_localhost_redirect_is_re_registered_before_use() {
    let server = MockServer::start().await;
    let issuer = server.uri();
    let redirects = issuer_redirect_uris(&issuer);
    Mock::given(method("POST"))
        .and(path("/register"))
        .and(body_json(serde_json::json!({
            "redirect_uris": redirects,
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "client_name": "newt-agent"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "client_id": "replacement-public-client",
            "redirect_uris": issuer_redirect_uris(&issuer),
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let stored = ClientFile {
        client_id: "bound-but-unusable".into(),
        redirect_uris: vec!["http://localhost:0/callback".into()],
        issuer: Some(issuer.clone()),
        extra: BTreeMap::new(),
    };
    let discovered = DiscoveredOAuthMeta {
        resource: "https://mcp.example/mcp".into(),
        issuer: issuer.clone(),
        authorization_endpoint: format!("{issuer}/authorize"),
        token_endpoint: format!("{issuer}/token"),
        registration_endpoint: Some(format!("{issuer}/register")),
        scope: None,
        code_challenge_methods_supported: vec!["S256".into()],
        authorization_response_iss_parameter_supported: true,
        extra: BTreeMap::new(),
    };

    let (registered, migrated) =
        resolve_client_registration(Some(stored), &discovered, None, true, &test_oauth_policy())
            .await
            .unwrap();

    assert!(migrated);
    assert_eq!(registered.client_id, "replacement-public-client");
    assert!(callback_target(&registered).is_ok());
    server.verify().await;
}

#[tokio::test]
async fn issuer_mismatched_saved_client_is_re_registered_when_dcr_is_available() {
    let server = MockServer::start().await;
    let issuer = server.uri();
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "client_id": "issuer-safe-replacement",
            "redirect_uris": issuer_redirect_uris(&issuer),
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let stored = ClientFile {
        client_id: "old-issuer-client".into(),
        redirect_uris: vec!["http://127.0.0.1:0/callback".into()],
        issuer: Some("https://old-auth.example.test".into()),
        extra: BTreeMap::new(),
    };
    let discovered = DiscoveredOAuthMeta {
        resource: "https://mcp.example.test/mcp".into(),
        issuer: issuer.clone(),
        authorization_endpoint: format!("{issuer}/authorize"),
        token_endpoint: format!("{issuer}/token"),
        registration_endpoint: Some(format!("{issuer}/register")),
        scope: None,
        code_challenge_methods_supported: vec!["S256".into()],
        authorization_response_iss_parameter_supported: true,
        extra: BTreeMap::new(),
    };

    let (registered, migrated) =
        resolve_client_registration(Some(stored), &discovered, None, true, &test_oauth_policy())
            .await
            .unwrap();

    assert!(migrated);
    assert_eq!(registered.client_id, "issuer-safe-replacement");
    assert_eq!(registered.issuer.as_deref(), Some(issuer.as_str()));
    server.verify().await;
}

#[tokio::test]
async fn dcr_rejects_rfc7591_confidential_default() {
    let server = MockServer::start().await;
    let issuer = server.uri();
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "client_id": "ambiguous-client",
            "redirect_uris": issuer_redirect_uris(&issuer),
            "grant_types": ["authorization_code", "refresh_token"]
        })))
        .mount(&server)
        .await;
    let error = register_public_client(
        &format!("{issuer}/register"),
        issuer_redirect_uris(&issuer),
        &issuer,
        None,
        true,
        &test_oauth_policy(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("client_secret_basic"), "{error}");
}

#[tokio::test]
async fn dcr_rejects_a_response_that_omits_redirect_uris() {
    let server = MockServer::start().await;
    let issuer = server.uri();
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "client_id": "missing-redirect-contract",
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"]
        })))
        .mount(&server)
        .await;
    let error = register_public_client(
        &format!("{issuer}/register"),
        issuer_redirect_uris(&issuer),
        &issuer,
        None,
        true,
        &test_oauth_policy(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("omitted redirect_uris"), "{error}");
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
