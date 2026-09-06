//! The interactive MCP OAuth 2.1 authorization-code + PKCE flow.
//!
//! Metadata discovery, PKCE generation, browser open, local callback server,
//! and token exchange — the half of `mcp_token` that runs only when an
//! operator explicitly authenticates a server. `run_oauth_flow` is the entry
//! point; everything else here supports it.

use super::*;

pub(super) struct PkceChallenge {
    pub(super) verifier: String,
    pub(super) challenge: String,
}

pub(super) fn gen_pkce() -> anyhow::Result<PkceChallenge> {
    // 32 random bytes → 43-char base64url string (within the 43-128 range).
    let mut buf = [0u8; 32];
    fill_random(&mut buf)?;

    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let verifier = engine.encode(buf);

    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = engine.encode(digest);

    Ok(PkceChallenge {
        verifier,
        challenge,
    })
}

fn fill_random(buf: &mut [u8]) -> anyhow::Result<()> {
    getrandom::getrandom(buf).map_err(|err| anyhow::anyhow!("failed to read OS randomness: {err}"))
}

fn is_internal_literal_host(host: &str) -> bool {
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    match ip {
        std::net::IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_link_local()
                || ip.is_loopback()
                || ip.is_unspecified()
                || ip.octets()[0] == 0
                || ip.octets()[0] >= 224
                || matches!(ip.octets(), [100, second, ..] if (64..=127).contains(&second))
        }
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80
                || (ip.segments()[0] & 0xff00) == 0xff00
        }
    }
}

fn exact_host_is_approved(url: &reqwest::Url, policy: &OAuthHopPolicy) -> bool {
    url.host_str()
        .is_some_and(|host| policy.explicitly_grants_host(host))
}

pub(super) fn fenced_client_for_url_with_policy(
    url: &reqwest::Url,
    allow_test_loopback_http: bool,
    policy: &OAuthHopPolicy,
) -> anyhow::Result<newt_mcp_client::FencedHttpClient> {
    newt_mcp_client::FencedHttpClient::for_url(
        url,
        std::time::Duration::from_secs(30),
        (cfg!(test) && allow_test_loopback_http) || exact_host_is_approved(url, policy),
    )
}

#[cfg(test)]
pub(super) fn validate_discovery_hop(
    endpoint: &str,
    kind: &str,
    allow_test_loopback_http: bool,
) -> anyhow::Result<reqwest::Url> {
    validate_discovery_hop_with_policy(
        endpoint,
        kind,
        allow_test_loopback_http,
        &OAuthHopPolicy::new(&newt_core::Scope::All),
    )
}

pub(super) fn validate_discovery_hop_with_policy(
    endpoint: &str,
    kind: &str,
    allow_test_loopback_http: bool,
    policy: &OAuthHopPolicy,
) -> anyhow::Result<reqwest::Url> {
    let url = validate_endpoint_url(endpoint, kind, allow_test_loopback_http)?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("OAuth {kind} URL has no host: {endpoint}"))?;
    if !(policy.permits_host(host) || cfg!(test) && allow_test_loopback_http) {
        anyhow::bail!("OAuth {kind} host `{host}` is outside the session network capability");
    }
    if !allow_test_loopback_http
        && !exact_host_is_approved(&url, policy)
        && (newt_mcp_client::host_is_loopback(host) || is_internal_literal_host(host))
    {
        anyhow::bail!(
            "OAuth {kind} resolves to a forbidden loopback/private literal host: {endpoint}"
        );
    }
    Ok(url)
}

pub(super) fn validate_https_resource(resource_url: &str) -> anyhow::Result<reqwest::Url> {
    validate_resource_url(resource_url, false)
}

fn validate_resource_url(
    resource_url: &str,
    allow_test_loopback_http: bool,
) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(resource_url)
        .map_err(|e| anyhow::anyhow!("invalid MCP resource URL `{resource_url}`: {e}"))?;
    let loopback_http = cfg!(test)
        && allow_test_loopback_http
        && url.scheme() == "http"
        && url
            .host_str()
            .is_some_and(newt_mcp_client::host_is_loopback);
    if url.scheme() != "https" && !loopback_http {
        anyhow::bail!("MCP OAuth resource URL must use https: {resource_url}");
    }
    if url.host_str().is_none() || url.username() != "" || url.password().is_some() {
        anyhow::bail!("invalid MCP OAuth resource URL: {resource_url}");
    }
    if url.fragment().is_some() {
        anyhow::bail!("MCP OAuth resource URL must not contain a fragment: {resource_url}");
    }
    Ok(url)
}

/// Return the URL parser's normalized resource identifier while preserving the
/// significant distinction between an origin with no path and an explicit `/`.
/// `url::Url` synthesizes `/` for an origin-only URL, but RFC 9728 resource
/// metadata uses simple string comparison and MCP recommends the bare origin as
/// the canonical identifier when that is what the client was configured with.
pub(super) fn canonical_resource_identifier(input: &str, parsed: &reqwest::Url) -> String {
    let explicit_path = input
        .split_once("://")
        .map(|(_, authority_and_rest)| {
            authority_and_rest
                .find(['/', '?', '#'])
                .is_some_and(|index| authority_and_rest.as_bytes()[index] == b'/')
        })
        .unwrap_or(true);
    let mut canonical = parsed.as_str().to_string();
    if parsed.path() == "/" && !explicit_path {
        let authority_start = canonical.find("://").map_or(0, |index| index + 3);
        if let Some(relative_slash) = canonical[authority_start..].find('/') {
            let slash = authority_start + relative_slash;
            if canonical[slash + 1..].is_empty()
                || canonical.as_bytes().get(slash + 1) == Some(&b'?')
            {
                canonical.remove(slash);
            }
        }
    }
    canonical
}

pub(super) fn validate_https_endpoint(endpoint: &str, kind: &str) -> anyhow::Result<reqwest::Url> {
    validate_endpoint_url(endpoint, kind, false)
}

fn validate_endpoint_url(
    endpoint: &str,
    kind: &str,
    allow_test_loopback_http: bool,
) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|e| anyhow::anyhow!("invalid OAuth {kind} `{endpoint}`: {e}"))?;
    let loopback_http = cfg!(test)
        && allow_test_loopback_http
        && url.scheme() == "http"
        && url
            .host_str()
            .is_some_and(newt_mcp_client::host_is_loopback);
    if (url.scheme() != "https" && !loopback_http)
        || url.host_str().is_none()
        || url.username() != ""
        || url.password().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!("OAuth {kind} must be an absolute https URL without credentials or fragment: {endpoint}");
    }
    Ok(url)
}

pub(super) fn resource_matches(bound: &str, selected: &str) -> bool {
    let Ok(bound_url) = reqwest::Url::parse(bound) else {
        return false;
    };
    let Ok(selected_url) = reqwest::Url::parse(selected) else {
        return false;
    };
    bound_url.fragment().is_none()
        && selected_url.fragment().is_none()
        && canonical_resource_identifier(bound, &bound_url)
            == canonical_resource_identifier(selected, &selected_url)
}

/// RFC 9728 well-known URL: insert the suffix between the authority and the
/// path/query rather than appending it to the MCP endpoint.
#[cfg(test)]
pub(super) fn protected_resource_metadata_url(resource_url: &str) -> anyhow::Result<String> {
    protected_resource_metadata_url_with_policy(resource_url, false)
}

fn protected_resource_metadata_url_with_policy(
    resource_url: &str,
    allow_test_loopback_http: bool,
) -> anyhow::Result<String> {
    let resource = validate_resource_url(resource_url, allow_test_loopback_http)?;
    let mut metadata = resource.clone();
    let resource_path = resource.path();
    let path_suffix = if resource_path == "/" {
        ""
    } else {
        resource_path
    };
    metadata.set_path(&format!(
        "/.well-known/oauth-protected-resource{path_suffix}"
    ));
    Ok(metadata.into())
}

fn root_protected_resource_metadata_url_with_policy(
    resource_url: &str,
    allow_test_loopback_http: bool,
) -> anyhow::Result<String> {
    let mut metadata = validate_resource_url(resource_url, allow_test_loopback_http)?;
    metadata.set_path("/.well-known/oauth-protected-resource");
    metadata.set_query(None);
    Ok(metadata.into())
}

/// RFC 8414 well-known URL: insert the suffix before any issuer path.
#[cfg(test)]
pub(super) fn authorization_server_metadata_url(issuer: &str) -> anyhow::Result<String> {
    authorization_server_metadata_url_with_policy(issuer, false)
}

fn authorization_server_metadata_url_with_policy(
    issuer: &str,
    allow_test_loopback_http: bool,
) -> anyhow::Result<String> {
    let issuer = validate_endpoint_url(issuer, "issuer", allow_test_loopback_http)?;
    if issuer.query().is_some() {
        anyhow::bail!("OAuth issuer must not contain a query: {issuer}");
    }
    let mut metadata = issuer.clone();
    let issuer_path = issuer.path();
    let path_suffix = if issuer_path == "/" { "" } else { issuer_path };
    metadata.set_path(&format!(
        "/.well-known/oauth-authorization-server{path_suffix}"
    ));
    Ok(metadata.into())
}

fn openid_configuration_urls_with_policy(
    issuer: &str,
    allow_test_loopback_http: bool,
) -> anyhow::Result<Vec<String>> {
    let issuer = validate_endpoint_url(issuer, "issuer", allow_test_loopback_http)?;
    if issuer.query().is_some() {
        anyhow::bail!("OAuth issuer must not contain a query: {issuer}");
    }
    let path_suffix = if issuer.path() == "/" {
        String::new()
    } else {
        issuer.path().to_string()
    };
    let mut inserted = issuer.clone();
    inserted.set_path(&format!("/.well-known/openid-configuration{path_suffix}"));
    let mut urls = vec![inserted.into()];
    if !path_suffix.is_empty() {
        let mut appended = issuer;
        appended.set_path(&format!(
            "{}/.well-known/openid-configuration",
            path_suffix.trim_end_matches('/')
        ));
        urls.push(appended.into());
    }
    Ok(urls)
}

#[cfg(test)]
pub(super) fn resource_origin(resource_url: &str) -> anyhow::Result<String> {
    resource_origin_with_policy(resource_url, false)
}

#[cfg(test)]
fn resource_origin_with_policy(
    resource_url: &str,
    allow_test_loopback_http: bool,
) -> anyhow::Result<String> {
    let resource = validate_resource_url(resource_url, allow_test_loopback_http)?;
    let host = resource
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("MCP resource URL has no host"))?;
    let port = resource.port().map(|p| format!(":{p}")).unwrap_or_default();
    Ok(format!("{}://{host}{port}", resource.scheme()))
}

fn is_auth_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Parse the two MCP-relevant auth parameters without substring matching.
/// Quoted strings honor backslash escaping; malformed target parameters are
/// rejected, while unrelated schemes/parameters are ignored.
pub(super) fn parse_bearer_challenge(header: &str) -> anyhow::Result<Option<BearerChallenge>> {
    let bytes = header.as_bytes();
    let mut index = 0;
    let mut in_bearer = false;
    let mut found_bearer = false;
    let mut parsed = BearerChallenge::default();
    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b',') {
            index += 1;
        }
        let start = index;
        while index < bytes.len() && is_auth_token_char(bytes[index]) {
            index += 1;
        }
        if start == index {
            index += 1;
            continue;
        }
        let name = &header[start..index];
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            in_bearer = name.eq_ignore_ascii_case("Bearer");
            found_bearer |= in_bearer;
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let mut value = String::new();
        if index < bytes.len() && bytes[index] == b'"' {
            index += 1;
            let mut terminated = false;
            while index < bytes.len() {
                match bytes[index] {
                    b'"' => {
                        index += 1;
                        terminated = true;
                        break;
                    }
                    b'\\' if index + 1 < bytes.len() => {
                        index += 1;
                        value.push(bytes[index] as char);
                        index += 1;
                    }
                    byte => {
                        value.push(byte as char);
                        index += 1;
                    }
                }
            }
            if !terminated
                && in_bearer
                && matches!(
                    name.to_ascii_lowercase().as_str(),
                    "resource_metadata" | "scope"
                )
            {
                anyhow::bail!("unterminated `{name}` value in WWW-Authenticate");
            }
        } else {
            let start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b','
            {
                index += 1;
            }
            value.push_str(&header[start..index]);
        }
        if !in_bearer {
            continue;
        }
        if name.eq_ignore_ascii_case("resource_metadata") {
            if value.is_empty() {
                anyhow::bail!("empty resource_metadata value");
            }
            parsed.resource_metadata = Some(value);
        } else if name.eq_ignore_ascii_case("scope") {
            if value.is_empty() {
                anyhow::bail!("empty scope value");
            }
            parsed.scope = Some(value);
        }
    }
    Ok(found_bearer.then_some(parsed))
}

fn valid_scope_value(scope: &str) -> bool {
    !scope.is_empty()
        && scope.bytes().all(|byte| {
            byte == b' '
                || byte == b'!'
                || (b'#'..=b'[').contains(&byte)
                || (b']'..=b'~').contains(&byte)
        })
        && scope.split(' ').all(|part| !part.is_empty())
}

pub(super) fn merge_scopes(
    current: Option<&str>,
    previously_granted: Option<&str>,
) -> Option<String> {
    let mut scopes = Vec::new();
    for scope in current.into_iter().chain(previously_granted) {
        for item in scope.split(' ') {
            if !item.is_empty() && !scopes.contains(&item) {
                scopes.push(item);
            }
        }
    }
    (!scopes.is_empty()).then(|| scopes.join(" "))
}

pub(super) fn prior_scope_for_binding(
    token: Option<&TokenFile>,
    metadata: Option<&MetaFile>,
    resource: &str,
    issuer: &str,
) -> Option<String> {
    let token = token?;
    let metadata = metadata?;
    if metadata.resource != resource
        || metadata.issuer != issuer
        || !token_has_exact_binding(token, metadata)
    {
        return None;
    }
    token
        .extra
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

async fn fetch_protected_resource_meta(
    resource_url: &str,
    allow_test_loopback_http: bool,
    policy: &OAuthHopPolicy,
) -> anyhow::Result<(Option<ProtectedResourceMeta>, Option<String>)> {
    let resource = validate_resource_url(resource_url, allow_test_loopback_http)?;
    let canonical_resource = canonical_resource_identifier(resource_url, &resource);
    if !allow_test_loopback_http {
        validate_discovery_hop_with_policy(resource.as_str(), "protected resource", false, policy)?;
    }
    let challenge_client =
        fenced_client_for_url_with_policy(&resource, allow_test_loopback_http, policy)?;
    let challenge = challenge_client
        .post(resource.clone())?
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "newt-oauth-discovery",
            "method": "initialize",
            "params": {
                "protocolVersion": newt_mcp_client::PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "newt-agent", "version": env!("CARGO_PKG_VERSION")}
            }
        }))
        .send()
        .await?;
    let mut challenge_metadata = None;
    let mut challenge_scope = None;
    let mut malformed = None;
    for value in challenge
        .headers()
        .get_all(reqwest::header::WWW_AUTHENTICATE)
    {
        let Ok(value) = value.to_str() else {
            continue;
        };
        match parse_bearer_challenge(value) {
            Ok(Some(parsed)) => {
                challenge_metadata = challenge_metadata.or(parsed.resource_metadata);
                challenge_scope = challenge_scope.or(parsed.scope);
            }
            Ok(None) => {}
            Err(error) => malformed = Some(error),
        }
    }
    if challenge_metadata.is_none() && challenge_scope.is_none() {
        if let Some(error) = malformed {
            return Err(error);
        }
    }
    if let Some(scope) = challenge_scope.as_deref() {
        if !valid_scope_value(scope) {
            anyhow::bail!("invalid scope value in WWW-Authenticate");
        }
    }
    // The challenge body is not semantically used, but still drain it through
    // the OAuth cap so a 401/200 probe cannot stream unbounded data.
    let _ = bounded_response_body(challenge).await?;

    let from_challenge = challenge_metadata.is_some();
    let mut metadata_urls = if let Some(url) = challenge_metadata {
        vec![url]
    } else {
        let path_url =
            protected_resource_metadata_url_with_policy(resource_url, allow_test_loopback_http)?;
        let root_url = root_protected_resource_metadata_url_with_policy(
            resource_url,
            allow_test_loopback_http,
        )?;
        if path_url == root_url {
            vec![path_url]
        } else {
            vec![path_url, root_url]
        }
    };

    for metadata_url in metadata_urls.drain(..) {
        let parsed = validate_discovery_hop_with_policy(
            &metadata_url,
            "protected resource metadata URL",
            allow_test_loopback_http,
            policy,
        )?;
        let metadata_client =
            fenced_client_for_url_with_policy(&parsed, allow_test_loopback_http, policy)?;
        let response = metadata_client.get(parsed)?.send().await?;
        if !from_challenge
            && matches!(
                response.status(),
                reqwest::StatusCode::NOT_FOUND
                    | reqwest::StatusCode::METHOD_NOT_ALLOWED
                    | reqwest::StatusCode::GONE
            )
        {
            continue;
        }
        if !response.status().is_success() {
            anyhow::bail!(
                "protected resource metadata request failed: HTTP {}",
                response.status()
            );
        }
        let body = bounded_response_body(response).await?;
        let metadata: ProtectedResourceMeta =
            serde_json::from_slice(&body).context("parsing protected resource metadata")?;
        // RFC 9728 uses simple string identity. The selected resource has already
        // been canonicalized once by URL parsing; do not apply further URL
        // equivalence (default-port elision, trailing-slash changes, etc.).
        if metadata.resource != canonical_resource {
            anyhow::bail!(
                "protected resource metadata resource mismatch: expected `{}`, got `{}`",
                canonical_resource,
                metadata.resource
            );
        }
        if metadata.authorization_servers.is_empty() {
            anyhow::bail!("protected resource metadata has no authorization_servers");
        }
        if metadata.authorization_servers.len() > MAX_ADVERTISED_AUTHORIZATION_SERVERS {
            anyhow::bail!(
                "protected resource metadata advertises {} authorization servers, above the limit of {MAX_ADVERTISED_AUTHORIZATION_SERVERS}",
                metadata.authorization_servers.len()
            );
        }
        if metadata
            .scopes_supported
            .iter()
            .any(|scope| !valid_scope_value(scope) || scope.contains(' '))
        {
            anyhow::bail!("protected resource metadata has an invalid scopes_supported value");
        }
        return Ok((Some(metadata), challenge_scope));
    }
    Ok((None, challenge_scope))
}

async fn fetch_authorization_server_meta(
    issuer: &str,
    allow_test_loopback_http: bool,
    policy: &OAuthHopPolicy,
) -> anyhow::Result<OAuthMeta> {
    validate_discovery_hop_with_policy(issuer, "issuer", allow_test_loopback_http, policy)?;
    let mut urls = vec![authorization_server_metadata_url_with_policy(
        issuer,
        allow_test_loopback_http,
    )?];
    urls.extend(openid_configuration_urls_with_policy(
        issuer,
        allow_test_loopback_http,
    )?);
    let mut failures = Vec::new();
    for url in urls {
        let attempt = async {
            let parsed = validate_discovery_hop_with_policy(
                &url,
                "authorization server metadata URL",
                allow_test_loopback_http,
                policy,
            )?;
            let metadata_client =
                fenced_client_for_url_with_policy(&parsed, allow_test_loopback_http, policy)?;
            let response = metadata_client.get(parsed)?.send().await?;
            if !response.status().is_success() {
                anyhow::bail!("HTTP {}", response.status());
            }
            let body = bounded_response_body(response).await?;
            let metadata: OAuthMeta =
                serde_json::from_slice(&body).context("parsing authorization server metadata")?;
            if metadata.issuer != issuer {
                anyhow::bail!(
                    "issuer mismatch: expected `{issuer}`, got `{}`",
                    metadata.issuer
                );
            }
            let authorization_endpoint = validate_discovery_hop_with_policy(
                &metadata.authorization_endpoint,
                "authorization endpoint",
                allow_test_loopback_http,
                policy,
            )?;
            // The authorization endpoint is opened by the user's browser, not
            // by this client. Resolve and screen it anyway so attacker-owned
            // metadata cannot steer the browser at an internal address.
            let _authorization_fence = fenced_client_for_url_with_policy(
                &authorization_endpoint,
                allow_test_loopback_http,
                policy,
            )?;
            let token_endpoint = validate_discovery_hop_with_policy(
                &metadata.token_endpoint,
                "token endpoint",
                allow_test_loopback_http,
                policy,
            )?;
            let _token_fence = fenced_client_for_url_with_policy(
                &token_endpoint,
                allow_test_loopback_http,
                policy,
            )?;
            if let Some(registration_endpoint) = metadata.registration_endpoint.as_deref() {
                let registration_endpoint = validate_discovery_hop_with_policy(
                    registration_endpoint,
                    "dynamic client registration endpoint",
                    allow_test_loopback_http,
                    policy,
                )?;
                let _registration_fence = fenced_client_for_url_with_policy(
                    &registration_endpoint,
                    allow_test_loopback_http,
                    policy,
                )?;
            }
            if !metadata
                .code_challenge_methods_supported
                .iter()
                .any(|method| method == "S256")
            {
                anyhow::bail!(
                    "authorization server `{issuer}` does not advertise required PKCE method S256"
                );
            }
            Ok::<OAuthMeta, anyhow::Error>(metadata)
        }
        .await;
        match attempt {
            Ok(metadata) => return Ok(metadata),
            Err(error) => failures.push(format!("{url}: {error:#}")),
        }
    }
    anyhow::bail!(
        "authorization server metadata discovery failed for `{issuer}`: {}",
        failures.join("; ")
    )
}

/// Discover RFC 9728 protected-resource metadata, then independently discover
/// the selected authorization server using RFC 8414. Missing or invalid
/// protected-resource metadata fails closed instead of treating the MCP
/// resource origin as an authorization issuer.
#[cfg(test)]
pub(super) async fn discover_oauth_meta_with_policy(
    server_url: &str,
    allow_test_loopback_http: bool,
) -> anyhow::Result<DiscoveredOAuthMeta> {
    discover_oauth_meta_for_client_with_policy(
        server_url,
        allow_test_loopback_http,
        None,
        false,
        &OAuthHopPolicy::new(&newt_core::Scope::All),
    )
    .await
}

/// As [`discover_oauth_meta_with_policy`], with the aggregate discovery budget
/// supplied by the caller so exhaustion can be exercised deterministically.
#[cfg(test)]
pub(super) async fn discover_oauth_meta_within_budget(
    server_url: &str,
    allow_test_loopback_http: bool,
    budget: &DiscoveryBudget,
) -> anyhow::Result<DiscoveredOAuthMeta> {
    discover_oauth_meta_within_budget_for_client(
        server_url,
        allow_test_loopback_http,
        None,
        false,
        &OAuthHopPolicy::new(&newt_core::Scope::All),
        budget,
    )
    .await
}

pub(super) async fn discover_oauth_meta_for_client_with_policy(
    server_url: &str,
    allow_test_loopback_http: bool,
    client: Option<&ClientFile>,
    allow_re_registration: bool,
    policy: &OAuthHopPolicy,
) -> anyhow::Result<DiscoveredOAuthMeta> {
    discover_oauth_meta_within_budget_for_client(
        server_url,
        allow_test_loopback_http,
        client,
        allow_re_registration,
        policy,
        &DiscoveryBudget::new(OAUTH_DISCOVERY_BUDGET),
    )
    .await
}

async fn discover_oauth_meta_within_budget_for_client(
    server_url: &str,
    allow_test_loopback_http: bool,
    client: Option<&ClientFile>,
    allow_re_registration: bool,
    policy: &OAuthHopPolicy,
    budget: &DiscoveryBudget,
) -> anyhow::Result<DiscoveredOAuthMeta> {
    let resource = validate_resource_url(server_url, allow_test_loopback_http)?;
    let canonical_resource = canonical_resource_identifier(server_url, &resource);
    let (protected, challenge_scope) = budget
        .bound(
            "protected resource metadata discovery",
            fetch_protected_resource_meta(server_url, allow_test_loopback_http, policy),
        )
        .await?;
    let protected = protected.ok_or_else(|| {
        anyhow::anyhow!("MCP protected resource did not publish required RFC 9728 metadata")
    })?;
    let issuers = order_authorization_servers(
        protected.authorization_servers.clone(),
        client,
        allow_re_registration,
    )?;
    let requested_scope = challenge_scope.clone().or_else(|| {
        (!protected.scopes_supported.is_empty()).then(|| protected.scopes_supported.join(" "))
    });
    let mut errors = Vec::new();
    let mut selected = None;
    for issuer in issuers {
        if budget.is_exhausted() {
            errors.push(format!(
                "{issuer}: OAuth discovery budget of {}s exhausted before this candidate",
                OAUTH_DISCOVERY_BUDGET.as_secs()
            ));
            break;
        }
        let attempt = budget
            .bound(
                &format!("authorization server discovery for `{issuer}`"),
                fetch_authorization_server_meta(&issuer, allow_test_loopback_http, policy),
            )
            .await;
        match attempt {
            Ok(metadata)
                if client.is_some_and(client_is_portable_cimd)
                    && !metadata_supports_cimd(&metadata.extra)
                    && !(allow_re_registration && metadata.registration_endpoint.is_some()) =>
            {
                errors.push(format!(
                    "{issuer}: authorization server does not advertise client_id_metadata_document_supported"
                ));
            }
            Ok(metadata) => {
                let stored_reusable = client.is_some_and(|client| {
                    let portable = client_is_portable_cimd(client);
                    let issuer_compatible = client.issuer.as_deref() == Some(issuer.as_str())
                        || (portable && metadata_supports_cimd(&metadata.extra));
                    let registration_supported = !portable
                        || (metadata_supports_cimd(&metadata.extra)
                            && valid_client_metadata_document_id(&client.client_id));
                    let mixup_safe = metadata.authorization_response_iss_parameter_supported
                        || client_has_issuer_distinct_redirect(client, &issuer);
                    issuer_compatible
                        && registration_supported
                        && client_auth_is_usable(client)
                        && dcr_registration_is_eligible(client, requested_scope.as_deref()).is_ok()
                        && mixup_safe
                        && callback_target(client).is_ok()
                });
                if allow_re_registration
                    && !stored_reusable
                    && metadata.registration_endpoint.is_none()
                {
                    errors.push(format!(
                        "{issuer}: saved client is not reusable and the authorization server has no dynamic client registration endpoint"
                    ));
                    continue;
                }
                selected = Some((issuer, metadata));
                break;
            }
            Err(error) => errors.push(format!("{issuer}: {error:#}")),
        }
    }
    let (issuer, metadata) = selected.ok_or_else(|| {
        anyhow::anyhow!(
            "no advertised authorization server passed discovery: {}",
            errors.join("; ")
        )
    })?;
    Ok(DiscoveredOAuthMeta {
        resource: canonical_resource,
        issuer,
        authorization_endpoint: metadata.authorization_endpoint,
        token_endpoint: metadata.token_endpoint,
        registration_endpoint: metadata.registration_endpoint,
        scope: requested_scope,
        code_challenge_methods_supported: metadata.code_challenge_methods_supported,
        authorization_response_iss_parameter_supported: metadata
            .authorization_response_iss_parameter_supported,
        extra: metadata.extra,
    })
}

/// Parse the OAuth authorization response as application/x-www-form-urlencoded.
pub(super) fn parse_callback(path: &str) -> CallbackParams {
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut out = CallbackParams::default();
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            let value = urlencoding_decode(value);
            match urlencoding_decode(key).as_str() {
                "code" => out.code = Some(value),
                "state" => out.state = Some(value),
                "iss" => {
                    if out.issuer.replace(value).is_some() {
                        out.duplicate_issuer = true;
                    }
                }
                "error" => out.error = Some(value),
                _ => {}
            }
        }
    }
    out
}

pub(super) fn validate_authorization_response(
    callback: &CallbackParams,
    expected_state: &str,
    expected_issuer: &str,
    issuer_parameter_required: bool,
) -> anyhow::Result<String> {
    if callback.state.as_deref() != Some(expected_state) {
        anyhow::bail!("OAuth state mismatch — possible CSRF; aborting");
    }
    if callback.duplicate_issuer {
        anyhow::bail!("OAuth authorization response contained duplicate issuer parameters");
    }
    if let Some(returned_issuer) = callback.issuer.as_deref() {
        if returned_issuer != expected_issuer {
            // Do not display any attacker-controlled OAuth error fields on an
            // issuer mismatch (RFC 9207 mix-up defense).
            anyhow::bail!("OAuth authorization response issuer mismatch; aborting");
        }
    } else if issuer_parameter_required {
        anyhow::bail!("OAuth authorization response omitted the required issuer; aborting");
    }
    if let Some(error) = callback.error.as_deref() {
        if let Some(error) = safe_oauth_error_code(error) {
            anyhow::bail!("OAuth authorization was rejected: {error}");
        }
        anyhow::bail!("OAuth authorization was rejected");
    }
    callback
        .code
        .clone()
        .ok_or_else(|| anyhow::anyhow!("No authorization code in callback"))
}

pub(super) fn callback_request_target(request: &str) -> anyhow::Result<&str> {
    let mut parts = request
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty callback request"))?
        .split_whitespace();
    if parts.next() != Some("GET") {
        anyhow::bail!("OAuth callback must use GET");
    }
    let target = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("OAuth callback request has no target"))?;
    let version = parts.next();
    if !target.starts_with('/')
        || !matches!(version, Some("HTTP/1.0" | "HTTP/1.1"))
        || parts.next().is_some()
    {
        anyhow::bail!("malformed OAuth callback request line");
    }
    Ok(target)
}

fn write_callback_response(stream: &mut std::net::TcpStream, status: &str, body: &[u8]) {
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(body);
}

pub(super) fn urlencoding_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.bytes().peekable();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next().unwrap_or(b'0');
            let h2 = chars.next().unwrap_or(b'0');
            if let Ok(decoded) =
                u8::from_str_radix(std::str::from_utf8(&[h1, h2]).unwrap_or("00"), 16)
            {
                out.push(decoded as char);
                continue;
            }
        }
        out.push(if b == b'+' { ' ' } else { b as char });
    }
    out
}

fn random_state() -> anyhow::Result<String> {
    let mut buf = [0u8; 16];
    fill_random(&mut buf)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
}

fn open_browser(url: &str) -> std::io::Result<std::process::Child> {
    #[cfg(target_os = "macos")]
    let (program, prefix_args): (&str, &[&str]) = ("open", &[]);
    #[cfg(target_os = "linux")]
    let (program, prefix_args): (&str, &[&str]) = ("xdg-open", &[]);
    #[cfg(windows)]
    let (program, prefix_args): (&str, &[&str]) = ("rundll32", &["url.dll,FileProtocolHandler"]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let (program, prefix_args): (&str, &[&str]) = ("xdg-open", &[]);

    std::process::Command::new(program)
        .args(prefix_args)
        .arg(url)
        .spawn()
}

pub(super) struct CallbackTarget {
    pub(super) redirect_uri: String,
    pub(super) bind_addr: std::net::SocketAddr,
    pub(super) path: String,
}

pub(super) fn callback_target(client: &ClientFile) -> anyhow::Result<CallbackTarget> {
    if client.redirect_uris.is_empty() {
        anyhow::bail!(
            "no usable local callback redirect URI; the saved client has no registered redirect_uris"
        );
    }
    let candidates = client.redirect_uris.clone();
    let mut failures = Vec::new();
    for candidate in candidates {
        let Ok(url) = reqwest::Url::parse(&candidate) else {
            failures.push(format!("invalid URI `{candidate}`"));
            continue;
        };
        let raw_host = url.host_str().unwrap_or_default();
        let host = raw_host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(raw_host);
        let secure_remote = url.scheme() == "https";
        let local_http = url.scheme() == "http" && newt_mcp_client::host_is_loopback(host);
        if (!secure_remote && !local_http)
            || url.username() != ""
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            failures.push(format!("unsafe redirect URI `{candidate}`"));
            continue;
        }
        if secure_remote {
            // HTTPS is valid MCP redirect policy, but this CLI owns a loopback
            // callback listener and cannot impersonate a remote HTTPS origin.
            failures.push(format!(
                "HTTPS redirect `{candidate}` cannot be served by the local CLI callback"
            ));
            continue;
        }
        if host.eq_ignore_ascii_case("localhost") {
            failures.push(format!(
                "redirect `{candidate}` is ambiguous between IPv4 and IPv6; register 127.0.0.1 or ::1 explicitly"
            ));
            continue;
        }
        let Some(port) = url.port() else {
            failures.push(format!(
                "redirect `{candidate}` omits an explicit loopback port"
            ));
            continue;
        };
        let ip = host
            .parse::<std::net::IpAddr>()
            .ok()
            .filter(std::net::IpAddr::is_loopback)
            .ok_or_else(|| anyhow::anyhow!("redirect host is not loopback"))?;
        return Ok(CallbackTarget {
            redirect_uri: candidate,
            bind_addr: std::net::SocketAddr::new(ip, port),
            path: url.path().to_string(),
        });
    }
    anyhow::bail!(
        "no usable local callback redirect URI; MCP requires localhost or HTTPS, and this CLI requires a localhost callback: {}",
        failures.join("; ")
    )
}

pub(super) fn bind_client_registration(
    mut client: ClientFile,
    issuer: &str,
    client_metadata_documents_supported: bool,
) -> anyhow::Result<(ClientFile, bool)> {
    let valid_metadata_document_id = valid_client_metadata_document_id(&client.client_id);
    let portable = client_is_portable_cimd(&client)
        && valid_metadata_document_id
        && client_metadata_documents_supported;
    if client_is_portable_cimd(&client) && !portable {
        anyhow::bail!(
            "saved client metadata document is invalid or unsupported by this authorization server"
        );
    }
    match client.issuer.as_deref() {
        Some(bound) if bound == issuer => Ok((client, false)),
        Some(_) if portable => {
            // Client ID Metadata Document identifiers are intentionally
            // portable across authorization servers. Record the new issuer for
            // status/token binding, but do not force a re-registration.
            client.issuer = Some(issuer.to_string());
            Ok((client, true))
        }
        Some(_) => anyhow::bail!(
            "MCP OAuth client registration is bound to a different authorization-server issuer; re-register before continuing"
        ),
        None if portable => {
            client.issuer = Some(issuer.to_string());
            Ok((client, true))
        }
        None => anyhow::bail!(
            "MCP OAuth client registration has no verified issuer binding; re-register it before continuing"
        ),
    }
}

fn valid_client_metadata_document_id(client_id: &str) -> bool {
    reqwest::Url::parse(client_id).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.path() != "/"
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}

pub(super) fn client_is_portable_cimd(client: &ClientFile) -> bool {
    let registered_as_cimd = client
        .extra
        .get("registration_method")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|method| method.eq_ignore_ascii_case("cimd"));
    let public_client = client
        .extra
        .get("token_endpoint_auth_method")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|method| method.eq_ignore_ascii_case("none"))
        && !client.extra.contains_key("client_secret");
    registered_as_cimd && public_client
}

fn metadata_supports_cimd(extra: &BTreeMap<String, serde_json::Value>) -> bool {
    extra
        .get("client_id_metadata_document_supported")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

fn registration_string_list_contains(client: &ClientFile, field: &str, required: &str) -> bool {
    client
        .extra
        .get(field)
        .and_then(serde_json::Value::as_array)
        .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(required)))
}

/// One eligibility predicate is used both for fresh DCR responses and saved
/// DCR reuse, so a registration rejected at creation cannot become acceptable
/// merely by surviving to a later process.
pub(super) fn dcr_registration_is_eligible(
    client: &ClientFile,
    requested_scope: Option<&str>,
) -> anyhow::Result<()> {
    let is_dcr = client
        .extra
        .get("registration_method")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|method| method.eq_ignore_ascii_case("dcr"));
    if !is_dcr {
        return Ok(());
    }
    if client_auth_method(client) != "none" || client.extra.contains_key("client_secret") {
        anyhow::bail!("dynamic registration is not a public PKCE client");
    }
    if !registration_string_list_contains(client, "grant_types", "authorization_code")
        || !registration_string_list_contains(client, "grant_types", "refresh_token")
    {
        anyhow::bail!(
            "dynamic registration does not permit authorization_code and refresh_token grants"
        );
    }
    if !registration_string_list_contains(client, "response_types", "code") {
        anyhow::bail!("dynamic registration does not permit the code response type");
    }
    if let Some(requested) = requested_scope {
        let registered = client
            .extra
            .get("scope")
            .and_then(serde_json::Value::as_str)
            .filter(|scope| valid_scope_value(scope))
            .ok_or_else(|| {
                anyhow::anyhow!("dynamic registration omitted the requested scope grant")
            })?;
        if !requested
            .split(' ')
            .all(|scope| registered.split(' ').any(|allowed| allowed == scope))
        {
            anyhow::bail!("dynamic registration did not grant every requested scope");
        }
    }
    Ok(())
}

pub(super) fn order_authorization_servers(
    mut issuers: Vec<String>,
    client: Option<&ClientFile>,
    allow_re_registration: bool,
) -> anyhow::Result<Vec<String>> {
    if let Some(bound_issuer) = client.and_then(|client| client.issuer.as_deref()) {
        let position = issuers.iter().position(|issuer| issuer == bound_issuer);
        if !client.is_some_and(client_is_portable_cimd) {
            let Some(position) = position else {
                if allow_re_registration {
                    return Ok(issuers);
                }
                anyhow::bail!(
                    "saved MCP OAuth client issuer `{bound_issuer}` is not advertised by the protected resource"
                );
            };
            let preferred = issuers.remove(position);
            if allow_re_registration {
                issuers.insert(0, preferred);
                return Ok(issuers);
            }
            return Ok(vec![preferred]);
        }
        if let Some(position) = position {
            let preferred = issuers.remove(position);
            issuers.insert(0, preferred);
        }
    }
    Ok(issuers)
}

#[derive(Serialize)]
struct DynamicClientRegistrationRequest<'a> {
    redirect_uris: &'a [String],
    token_endpoint_auth_method: &'static str,
    grant_types: [&'static str; 2],
    response_types: [&'static str; 1],
    client_name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
}

pub(super) async fn register_public_client(
    registration_endpoint: &str,
    redirect_uris: Vec<String>,
    issuer: &str,
    requested_scope: Option<&str>,
    allow_test_loopback_http: bool,
    policy: &OAuthHopPolicy,
) -> anyhow::Result<ClientFile> {
    if redirect_uris.is_empty() {
        anyhow::bail!("dynamic client registration requires at least one redirect URI");
    }
    let endpoint = validate_discovery_hop_with_policy(
        registration_endpoint,
        "dynamic client registration endpoint",
        allow_test_loopback_http,
        policy,
    )?;
    let client = fenced_client_for_url_with_policy(&endpoint, allow_test_loopback_http, policy)?;
    let request = DynamicClientRegistrationRequest {
        redirect_uris: &redirect_uris,
        token_endpoint_auth_method: "none",
        grant_types: ["authorization_code", "refresh_token"],
        response_types: ["code"],
        client_name: "newt-agent",
        scope: requested_scope,
    };
    let response = client.post(endpoint)?.json(&request).send().await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = bounded_response_body(response).await.unwrap_or_default();
        let detail = safe_oauth_error(&body)
            .map(|error| format!(" — {error}"))
            .unwrap_or_default();
        anyhow::bail!("dynamic client registration failed: HTTP {status}{detail}");
    }
    let body = bounded_response_body(response).await?;
    let value: serde_json::Value =
        serde_json::from_slice(&body).context("parsing dynamic client registration response")?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("dynamic client registration response was not an object"))?;
    let client_id = object
        .get("client_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("dynamic client registration omitted client_id"))?
        .to_string();
    let returned_method = object
        .get("token_endpoint_auth_method")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!(
            "dynamic client registration omitted token_endpoint_auth_method; RFC 7591 would default it to client_secret_basic"
        ))?;
    if returned_method != "none" || object.contains_key("client_secret") {
        anyhow::bail!(
            "dynamic client registration returned a confidential client; newt requested a public PKCE client"
        );
    }
    let returned_redirects = serde_json::from_value::<Vec<String>>(
        object
            .get("redirect_uris")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("dynamic client registration omitted redirect_uris"))?,
    )
    .context("parsing dynamic client redirect_uris")?;
    if returned_redirects != redirect_uris {
        anyhow::bail!("dynamic client registration changed the requested redirect_uris");
    }
    let mut extra: BTreeMap<String, serde_json::Value> = object
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "client_id" | "redirect_uris"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    extra.insert(
        "token_endpoint_auth_method".into(),
        serde_json::Value::String("none".into()),
    );
    extra.insert(
        "registration_method".into(),
        serde_json::Value::String("dcr".into()),
    );
    let registration = ClientFile {
        client_id,
        redirect_uris,
        issuer: Some(issuer.to_string()),
        extra,
    };
    dcr_registration_is_eligible(&registration, requested_scope)?;
    Ok(registration)
}

fn issuer_callback_path(issuer: &str) -> String {
    let digest = Sha256::digest(issuer.as_bytes());
    let hash: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("/callback/newt-{hash}")
}

pub(super) fn issuer_redirect_uris(issuer: &str) -> Vec<String> {
    vec![format!(
        "http://127.0.0.1:0{}",
        issuer_callback_path(issuer)
    )]
}

pub(super) fn client_has_issuer_distinct_redirect(client: &ClientFile, issuer: &str) -> bool {
    let expected_path = issuer_callback_path(issuer);
    client.redirect_uris.iter().any(|redirect| {
        reqwest::Url::parse(redirect).is_ok_and(|url| {
            url.scheme() == "http"
                && url
                    .host_str()
                    .is_some_and(newt_mcp_client::host_is_loopback)
                && url.path() == expected_path
                && url.query().is_none()
                && url.fragment().is_none()
        })
    })
}

pub(super) async fn resolve_client_registration(
    stored: Option<ClientFile>,
    discovered: &DiscoveredOAuthMeta,
    requested_scope: Option<&str>,
    allow_test_loopback_http: bool,
    policy: &OAuthHopPolicy,
) -> anyhow::Result<(ClientFile, bool)> {
    match stored {
        Some(client) if client_is_portable_cimd(&client) => {
            if discovered.authorization_response_iss_parameter_supported
                && metadata_supports_cimd(&discovered.extra)
                && valid_client_metadata_document_id(&client.client_id)
                && client_auth_is_usable(&client)
                && dcr_registration_is_eligible(&client, requested_scope).is_ok()
                && callback_target(&client).is_ok()
            {
                if let Ok(bound) = bind_client_registration(
                    client,
                    &discovered.issuer,
                    metadata_supports_cimd(&discovered.extra),
                ) {
                    return Ok(bound);
                }
            }
            let registration_endpoint = discovered.registration_endpoint.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "portable MCP OAuth client lacks a usable local callback or RFC 9207 issuer response, and the authorization server has no dynamic registration endpoint"
                )
            })?;
            let replacement = register_public_client(
                registration_endpoint,
                issuer_redirect_uris(&discovered.issuer),
                &discovered.issuer,
                requested_scope,
                allow_test_loopback_http,
                policy,
            )
            .await?;
            Ok((replacement, true))
        }
        Some(client) if client.issuer.is_some() => {
            let bound = bind_client_registration(
                client,
                &discovered.issuer,
                metadata_supports_cimd(&discovered.extra),
            );
            if let Ok((client, migrated)) = bound {
                let mixup_safe = discovered.authorization_response_iss_parameter_supported
                    || client_has_issuer_distinct_redirect(&client, &discovered.issuer);
                if mixup_safe
                    && client_auth_is_usable(&client)
                    && dcr_registration_is_eligible(&client, requested_scope).is_ok()
                    && callback_target(&client).is_ok()
                {
                    return Ok((client, migrated));
                }
            }
            let registration_endpoint = discovered.registration_endpoint.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "saved MCP OAuth client has a stale issuer, unusable local callback, or non-mix-up-safe redirect, and the authorization server has no dynamic registration endpoint"
                )
            })?;
            let replacement = register_public_client(
                registration_endpoint,
                issuer_redirect_uris(&discovered.issuer),
                &discovered.issuer,
                requested_scope,
                allow_test_loopback_http,
                policy,
            )
            .await?;
            Ok((replacement, true))
        }
        _legacy => {
            let registration_endpoint = discovered.registration_endpoint.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "MCP OAuth client registration is missing a verified issuer and the selected authorization server has no dynamic registration endpoint; re-register this client with Hermes"
                )
            })?;
            let client = register_public_client(
                registration_endpoint,
                issuer_redirect_uris(&discovered.issuer),
                &discovered.issuer,
                requested_scope,
                allow_test_loopback_http,
                policy,
            )
            .await?;
            Ok((client, true))
        }
    }
}

pub(super) fn build_authorization_url(
    endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    resource: &str,
    state: &str,
    scope: Option<&str>,
) -> anyhow::Result<String> {
    let mut url = validate_https_endpoint(endpoint, "authorization endpoint")?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", resource)
            .append_pair("state", state);
        if let Some(scope) = scope {
            pairs.append_pair("scope", scope);
        }
    }
    Ok(url.into())
}

/// Run the full MCP OAuth 2.1 authorization-code + PKCE flow for `server_name`.
///
/// `server_url` is the MCP endpoint URL — used for OAuth metadata discovery when
/// no `<name>.meta.json` is present.
///
/// On success writes a Newt-owned hidden credential generation. Hermes' flat
/// records remain read-only interoperability input and are never overwritten.
pub async fn run_oauth_flow(
    server_name: &str,
    server_url: &str,
    policy: &OAuthHopPolicy,
) -> anyhow::Result<()> {
    let dir = {
        let home = platform_home_dir()
            .ok_or_else(|| anyhow::anyhow!("neither HOME nor USERPROFILE is set"))?;
        let d = home.join(".hermes").join("mcp-tokens");
        std::fs::create_dir_all(&d)?;
        d
    };

    validate_https_resource(server_url)?;
    // Validate the Hermes compatibility paths and case-fold identity before any
    // discovery or browser side effect.
    let _ = portable_credential_paths(&dir, server_name)?;
    let _ = credential_lock_path(&dir, server_name)?;

    let (initial_snapshot, initial_hermes_cursor, stored_client, prior_token, prior_meta): (
        CredentialSnapshot,
        HermesCursor,
        Option<ClientFile>,
        Option<TokenFile>,
        Option<MetaFile>,
    ) = {
        let transaction = acquire_credential_lock(&dir, server_name)?;
        let had_manifest = credential_manifest_path(&dir, server_name).exists();
        if let Err(error) = migrate_legacy_credentials(&dir, server_name, &transaction) {
            if had_manifest {
                return Err(error).context("validating Newt MCP credential generation");
            }
            tracing::warn!(
                "ignoring unusable legacy MCP credentials for `{server_name}` during explicit re-authentication: {error:#}"
            );
        }
        let before = credential_snapshot(&dir, server_name)?;
        let records = match read_credential_records(&dir, server_name) {
            Ok(records) => records,
            Err(error) if !had_manifest => {
                tracing::warn!(
                    "legacy MCP credential hints for `{server_name}` are unusable; registering fresh: {error:#}"
                );
                CredentialRecords::default()
            }
            Err(error) => return Err(error),
        };
        let snapshot = credential_snapshot(&dir, server_name)?;
        if before != snapshot {
            anyhow::bail!(
                "MCP credentials for `{server_name}` changed while authorization state was being prepared; retry `newt auth`"
            );
        }
        let cursor = match read_credential_manifest(&dir, server_name)? {
            Some(manifest) => manifest_hermes_cursor(&manifest),
            None => hermes_cursor_from_snapshot(
                &snapshot,
                &portable_credential_paths(&dir, server_name)?,
            ),
        };
        (
            snapshot,
            cursor,
            records.client,
            records.token,
            records.meta,
        )
    };
    let discovery_client = stored_client
        .as_ref()
        .filter(|client| client.issuer.is_some() || client_is_portable_cimd(client));

    // ── 1. OAuth server metadata ──────────────────────────────────────────
    // Explicit authentication always rediscovers: RFC 9728 allows the resource
    // to change AS metadata, and stale cached issuer state must not silently win.
    let discovered = discover_oauth_meta_for_client_with_policy(
        server_url,
        false,
        discovery_client,
        true,
        policy,
    )
    .await?;
    let prior_scope = prior_scope_for_binding(
        prior_token.as_ref(),
        prior_meta.as_ref(),
        &discovered.resource,
        &discovered.issuer,
    );
    let requested_scope = merge_scopes(discovered.scope.as_deref(), prior_scope.as_deref());
    let auth_endpoint = discovered.authorization_endpoint.clone();
    let token_endpoint = discovered.token_endpoint.clone();
    let issuer = discovered.issuer.clone();
    let meta = MetaFile {
        resource: discovered.resource.clone(),
        issuer: issuer.clone(),
        authorization_endpoint: Some(auth_endpoint.clone()),
        token_endpoint: token_endpoint.clone(),
        code_challenge_methods_supported: discovered.code_challenge_methods_supported.clone(),
        authorization_response_iss_parameter_supported: discovered
            .authorization_response_iss_parameter_supported,
        extra: discovered.extra.clone(),
    };
    // ── 2. Client registration ────────────────────────────────────────────
    let (client, _migrated) = resolve_client_registration(
        stored_client,
        &discovered,
        requested_scope.as_deref(),
        false,
        policy,
    )
    .await?;
    let callback = callback_target(&client)?;

    // Bind the callback listener NOW (before opening the browser) so the port
    // is guaranteed to be open when the browser redirects back.
    let listener = TcpListener::bind(callback.bind_addr)?;
    let actual_port = listener.local_addr()?.port();

    // If the stored redirect_uri had no fixed port, use the OS-selected port.
    let redirect_uri = if callback.bind_addr.port() == 0 {
        let mut actual = reqwest::Url::parse(&callback.redirect_uri)?;
        actual
            .set_port(Some(actual_port))
            .map_err(|()| anyhow::anyhow!("cannot set callback port"))?;
        actual.into()
    } else {
        callback.redirect_uri
    };

    // ── 3. PKCE ───────────────────────────────────────────────────────────
    let pkce = gen_pkce()?;
    let state = random_state()?;

    // ── 4. Authorization URL ──────────────────────────────────────────────
    let auth_url = build_authorization_url(
        &auth_endpoint,
        &client.client_id,
        &redirect_uri,
        &pkce.challenge,
        &discovered.resource,
        &state,
        requested_scope.as_deref(),
    )?;

    // ── 5. Open browser ───────────────────────────────────────────────────
    println!("\nMCP OAuth: authorization required for `{server_name}`.");
    println!("Opening your browser to complete the login…\n");
    println!("  {auth_url}\n");
    println!("(If the browser did not open, paste the URL above manually.)");

    // Best-effort browser open — failure is not fatal; the user can paste.
    let _ = open_browser(&auth_url);

    // ── 6. Wait for callback ──────────────────────────────────────────────
    println!("\nWaiting for authorization callback on port {actual_port}…");

    // Hand the already-bound listener to the blocking callback waiter via a
    // tokio blocking task so we don't freeze the async runtime.
    let expected_callback_path = callback.path;
    let expected_state = state.clone();
    let expected_issuer = issuer.clone();
    let issuer_parameter_advertised = discovered.authorization_response_iss_parameter_supported;
    let code = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        listener.set_nonblocking(true)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!("timed out waiting for OAuth callback");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
            let mut buf = [0u8; 8192];
            let n = match stream.read(&mut buf) {
                Ok(n) => n,
                Err(_) => {
                    write_callback_response(&mut stream, "408 Request Timeout", b"Invalid callback");
                    continue;
                }
            };
            let request = match std::str::from_utf8(&buf[..n]) {
                Ok(request) if request.contains("\r\n\r\n") => request,
                _ => {
                    write_callback_response(&mut stream, "400 Bad Request", b"Invalid callback");
                    continue;
                }
            };
            let target = match callback_request_target(request) {
                Ok(target) => target,
                Err(_) => {
                    write_callback_response(&mut stream, "400 Bad Request", b"Invalid callback");
                    continue;
                }
            };
            let returned_path = target.split_once('?').map_or(target, |(path, _)| path);
            if returned_path != expected_callback_path {
                write_callback_response(&mut stream, "404 Not Found", b"Unknown callback path");
                continue;
            }
            let callback = parse_callback(target);
            match validate_authorization_response(
                &callback,
                &expected_state,
                &expected_issuer,
                issuer_parameter_advertised,
            ) {
                Ok(code) => {
                    write_callback_response(
                        &mut stream,
                        "200 OK",
                        b"<html><body><h2>Authorization successful</h2><p>You can close this tab and return to newt.</p></body></html>",
                    );
                    return Ok(code);
                }
                Err(error)
                    if callback.state.as_deref() == Some(expected_state.as_str())
                        && callback.error.is_some() =>
                {
                    write_callback_response(&mut stream, "400 Bad Request", b"Authorization rejected");
                    return Err(error);
                }
                Err(_) => {
                    write_callback_response(&mut stream, "400 Bad Request", b"Invalid callback");
                }
            }
        }
    })
    .await??;

    // ── 7. Token exchange ─────────────────────────────────────────────────
    println!("Authorization code received — exchanging for tokens…");

    let token_url =
        validate_discovery_hop_with_policy(&token_endpoint, "token endpoint", false, policy)?;
    let token_http = fenced_client_for_url_with_policy(&token_url, false, policy)?;
    let mut token_form = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code),
        ("redirect_uri".into(), redirect_uri.clone()),
        ("code_verifier".into(), pkce.verifier.clone()),
        ("resource".into(), discovered.resource.clone()),
    ];
    let token_request = apply_client_authentication(
        token_http.post(token_url.clone())?,
        &client,
        &mut token_form,
    )?;
    let resp = token_request.form(&token_form).send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = bounded_response_body(resp).await.unwrap_or_default();
        let detail = safe_oauth_error(&body)
            .map(|error| format!(" — {error}"))
            .unwrap_or_default();
        anyhow::bail!("Token exchange failed: HTTP {status}{detail}");
    }

    let body = bounded_response_body(resp).await?;
    let tok = parse_token_response(&body)?;

    // ── 8. Persist ────────────────────────────────────────────────────────
    let transaction = acquire_credential_lock(&dir, server_name)?;
    ensure_credential_snapshot(&dir, server_name, &initial_snapshot)?;
    let token = updated_token_file(
        &tok.access_token,
        tok.refresh_token.as_deref(),
        tok.expires_in,
        &tok.extra,
        &discovered.resource,
        &discovered.issuer,
    );
    publish_credential_generation(
        &dir,
        server_name,
        &CredentialBundle {
            token,
            meta,
            client,
        },
        initial_hermes_cursor,
        &transaction,
    )
    .with_context(|| format!("persisting OAuth credential generation for `{server_name}`"))?;

    println!("✓ Authenticated `{server_name}`. Newt credential generation saved privately.");
    Ok(())
}

#[cfg(test)]
pub(super) fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}
