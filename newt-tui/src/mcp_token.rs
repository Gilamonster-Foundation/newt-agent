//! Hermes-compatible MCP OAuth token loader and interactive auth flow.
//!
//! **Token loading** (`load_bearer_token`): imports exactly-bound legacy tokens
//! stored by hermes-agent in `~/.hermes/mcp-tokens/<name>.json`, then keeps
//! Newt-owned refresh generations behind a private manifest. Hermes' flat trio
//! is read-only input: Newt never overwrites another client's refresh rotation.
//!
//! **Interactive auth** (`run_oauth_flow`): implements the MCP OAuth 2.1
//! authorization-code + PKCE flow end-to-end — metadata discovery, PKCE
//! generation, browser open, local callback server, token exchange — and writes
//! the resulting tokens to Newt-owned hidden generations in the same directory.
//!
//! Legacy Hermes input retains the flat `<name>{,.meta,.client}.json` layout;
//! Newt state uses `.newt-oauth-*.generation-*.{token,meta,client}.json` plus an
//! atomic `.newt-oauth-*.manifest.json` commit pointer.
//!
//! Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 15:53 EDT | Date: 2026-08-12

use std::collections::BTreeMap;
use std::io::{Read, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// File-level types
// ---------------------------------------------------------------------------

/// The fields we care about from `<name>.json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct TokenFile {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    /// Unix timestamp (floating-point seconds) when the access_token expires.
    #[serde(default)]
    expires_at: Option<f64>,
    /// Exact protected-resource binding for credentials written by newt.
    /// Legacy Hermes tokens omit this and are withheld until an explicit Newt
    /// authorization flow can establish an authoritative binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resource: Option<String>,
    /// Authorization-server issuer that minted this token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issuer: Option<String>,
    // Pass through the rest so we can rewrite the file without losing fields.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

/// The subset of `<name>.meta.json` we need for auth.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct MetaFile {
    /// Exact MCP resource identifier this token was requested for. A token is
    /// never loaded for a config entry whose URL differs from this value.
    /// Legacy Hermes metadata omitted it; the empty default remains unusable
    /// until `newt auth` rediscovers and binds the protected resource.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    resource: String,
    /// Authorization-server issuer selected through RFC 9728 discovery.
    issuer: String,
    #[serde(default)]
    authorization_endpoint: Option<String>,
    token_endpoint: String,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

/// The subset of `<name>.client.json` we need.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct ClientFile {
    client_id: String,
    #[serde(default)]
    redirect_uris: Vec<String>,
    /// Hermes/newt extension used to prevent reusing AS-local client IDs after
    /// protected-resource metadata selects a different issuer. Legacy client
    /// files without this field may be migrated only after the current issuer
    /// has been validated through discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issuer: Option<String>,
    /// Preserve registration-specific fields such as `client_secret` and
    /// `token_endpoint_auth_method` when adding the issuer binding.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CredentialManifest {
    version: u32,
    server_name: String,
    generation: String,
    /// Last Hermes flat token record observed while holding Newt's credential
    /// lock. This is an import cursor, not ownership: Newt never writes the
    /// flat trio and only considers a later, changed token for adoption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hermes_token_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hermes_meta_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hermes_client_sha256: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct HermesCursor {
    token: Option<String>,
    meta: Option<String>,
    client: Option<String>,
}

#[derive(Debug)]
struct HermesSnapshot {
    bundle: CredentialBundle,
    cursor: HermesCursor,
}

#[derive(Debug, Clone)]
struct CredentialBundle {
    token: TokenFile,
    meta: MetaFile,
    client: ClientFile,
}

#[derive(Debug, Default)]
struct CredentialRecords {
    token: Option<TokenFile>,
    meta: Option<MetaFile>,
    client: Option<ClientFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CredentialPaths {
    token: PathBuf,
    meta: PathBuf,
    client: PathBuf,
}

impl CredentialPaths {
    fn all(&self) -> [&Path; 3] {
        [&self.token, &self.meta, &self.client]
    }

    fn complete(&self) -> bool {
        self.all().into_iter().all(Path::is_file)
    }

    fn any_present(&self) -> bool {
        self.all().into_iter().any(Path::exists)
    }
}

const CREDENTIAL_MANIFEST_VERSION: u32 = 2;

/// Response body from a token endpoint (both refresh and code-exchange).
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: Option<f64>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

const MAX_OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;

/// Ceiling on the `authorization_servers` a protected resource may advertise.
///
/// RFC 9728 puts no bound on that array, so a hostile or compromised protected
/// resource could list thousands of issuers and make one authentication
/// recovery fan out into an arbitrarily large number of outbound discovery
/// attempts. An over-long list is **rejected**, never truncated: truncating
/// would silently change which authorization servers the resource asked for,
/// and quietly picking a subset of an attacker-chosen list is not a safe
/// default. Real deployments advertise one issuer, occasionally two; four
/// leaves failover headroom while keeping the fan-out a small constant.
const MAX_ADVERTISED_AUTHORIZATION_SERVERS: usize = 4;

/// Wall-clock allowance for one whole OAuth discovery operation.
///
/// Security invariant: **one authentication-recovery operation performs a
/// bounded amount of external discovery work.** Per-request timeouts alone do
/// not give that — N candidate issuers that each stall multiply into N times
/// the per-request timeout. Every hop of a discovery draws from this single
/// budget instead, so the total is bounded no matter how many candidates fail
/// or stall, and exhaustion fails closed. Ninety seconds is many times a
/// healthy discovery (a few sub-second requests) yet well inside the
/// interactive patience of an operator waiting to re-authenticate.
const OAUTH_DISCOVERY_BUDGET: std::time::Duration = std::time::Duration::from_secs(90);

/// The shared deadline every hop of one discovery operation draws from.
struct DiscoveryBudget {
    deadline: std::time::Instant,
}

impl DiscoveryBudget {
    fn new(total: std::time::Duration) -> Self {
        Self {
            deadline: std::time::Instant::now() + total,
        }
    }

    fn remaining(&self) -> std::time::Duration {
        self.deadline
            .saturating_duration_since(std::time::Instant::now())
    }

    fn is_exhausted(&self) -> bool {
        self.remaining().is_zero()
    }

    /// Run one discovery hop against whatever is left of the budget. A hop that
    /// would outlive the budget is dropped — cancelled, not awaited — so the
    /// aggregate operation cannot be extended by a stalling candidate.
    async fn bound<T>(
        &self,
        what: &str,
        hop: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let remaining = self.remaining();
        if remaining.is_zero() {
            anyhow::bail!(
                "OAuth discovery budget of {}s exhausted before {what}",
                OAUTH_DISCOVERY_BUDGET.as_secs()
            );
        }
        match tokio::time::timeout(remaining, hop).await {
            Ok(result) => result,
            Err(_) => anyhow::bail!(
                "OAuth discovery budget of {}s exhausted during {what}",
                OAUTH_DISCOVERY_BUDGET.as_secs()
            ),
        }
    }
}

async fn bounded_response_body(mut response: reqwest::Response) -> anyhow::Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_OAUTH_RESPONSE_BYTES as u64)
    {
        anyhow::bail!("OAuth response exceeded the 1 MiB limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_OAUTH_RESPONSE_BYTES {
            anyhow::bail!("OAuth response exceeded the 1 MiB limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_token_response(body: &[u8]) -> anyhow::Result<TokenResponse> {
    let response: TokenResponse =
        serde_json::from_slice(body).context("parsing OAuth token response")?;
    if response.access_token.trim().is_empty() {
        anyhow::bail!("OAuth token response contained an empty access_token");
    }
    let token_type = response
        .extra
        .get("token_type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("OAuth token response omitted token_type"))?;
    if !token_type.eq_ignore_ascii_case("Bearer") {
        anyhow::bail!("OAuth token response returned unsupported token_type");
    }
    if response
        .expires_in
        .is_some_and(|seconds| !seconds.is_finite() || seconds <= 0.0)
    {
        anyhow::bail!("OAuth token response returned invalid expires_in");
    }
    Ok(response)
}

fn safe_oauth_error_code(code: &str) -> Option<&str> {
    (!code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    .then_some(code)
}

fn safe_oauth_error(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object()?;
    let code = object.get("error")?.as_str()?;
    let code = safe_oauth_error_code(code)?;
    Some(format!("OAuth error `{code}`"))
}

fn apply_client_authentication(
    request: reqwest::RequestBuilder,
    client: &ClientFile,
    form: &mut Vec<(String, String)>,
) -> anyhow::Result<reqwest::RequestBuilder> {
    let secret = client
        .extra
        .get("client_secret")
        .and_then(serde_json::Value::as_str);
    let method = client_auth_method(client);
    match method {
        "none" => {
            form.push(("client_id".into(), client.client_id.clone()));
            Ok(request)
        }
        "client_secret_basic" => {
            let secret = secret.ok_or_else(|| {
                anyhow::anyhow!("client_secret_basic registration has no client_secret")
            })?;
            Ok(request.basic_auth(&client.client_id, Some(secret)))
        }
        "client_secret_post" => {
            let secret = secret.ok_or_else(|| {
                anyhow::anyhow!("client_secret_post registration has no client_secret")
            })?;
            form.push(("client_id".into(), client.client_id.clone()));
            form.push(("client_secret".into(), secret.to_string()));
            Ok(request)
        }
        _ => anyhow::bail!("unsupported token_endpoint_auth_method `{method}`"),
    }
}

fn client_auth_method(client: &ClientFile) -> &str {
    let explicit_method = client
        .extra
        .get("token_endpoint_auth_method")
        .and_then(serde_json::Value::as_str);
    let secret_present = client.extra.contains_key("client_secret");
    // RFC 7591 defaults an omitted method to client_secret_basic. Preserve
    // public legacy registrations without a secret, but never silently turn a
    // secret-bearing registration into a public client.
    let registered_method = client
        .extra
        .get("registration_method")
        .and_then(serde_json::Value::as_str);
    explicit_method.unwrap_or(match (registered_method, secret_present) {
        (Some("dcr" | "cimd"), _) | (_, true) => "client_secret_basic",
        _ => "none",
    })
}

fn client_auth_is_usable(client: &ClientFile) -> bool {
    if client.client_id.trim().is_empty() {
        return false;
    }
    let secret_field_present = client.extra.contains_key("client_secret");
    let secret = client
        .extra
        .get("client_secret")
        .and_then(serde_json::Value::as_str)
        .filter(|secret| !secret.is_empty());
    match client_auth_method(client) {
        "none" => !secret_field_present,
        "client_secret_basic" | "client_secret_post" => secret.is_some(),
        _ => false,
    }
}

/// OAuth server metadata from RFC 8414 discovery.
#[derive(Debug, Deserialize, Serialize)]
struct OAuthMeta {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

/// RFC 9728 metadata published by the MCP protected resource.
#[derive(Debug, Deserialize)]
struct ProtectedResourceMeta {
    resource: String,
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

/// Validated OAuth metadata plus the protected resource it applies to.
#[derive(Debug)]
struct DiscoveredOAuthMeta {
    resource: String,
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scope: Option<String>,
    code_challenge_methods_supported: Vec<String>,
    authorization_response_iss_parameter_supported: bool,
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BearerChallenge {
    resource_metadata: Option<String>,
    scope: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    issuer: Option<String>,
    error: Option<String>,
    duplicate_issuer: bool,
}

/// The complete session network capability carried through every OAuth hop.
/// `Scope::All` permits public egress but deliberately does not count as an
/// exact approval for SSRF-sensitive private address resolution.
#[derive(Clone, Debug)]
pub struct OAuthHopPolicy {
    net: newt_core::Scope<String>,
}

impl OAuthHopPolicy {
    #[must_use]
    pub fn new(net: &newt_core::Scope<String>) -> Self {
        Self { net: net.clone() }
    }

    fn permits_host(&self, host: &str) -> bool {
        newt_mcp_client::net_scope_permits_http_host(&self.net, host)
    }

    fn explicitly_grants_host(&self, host: &str) -> bool {
        match &self.net {
            newt_core::Scope::Only(hosts) => hosts
                .iter()
                .any(|granted| newt_mcp_client::http_host_grant_matches(granted, host)),
            newt_core::Scope::All => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn platform_home_from(
    home: Option<&std::ffi::OsStr>,
    userprofile: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    home.filter(|value| !value.is_empty())
        .or_else(|| userprofile.filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

fn platform_home_dir() -> Option<PathBuf> {
    platform_home_from(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
    )
}

fn hermes_token_dir() -> Option<PathBuf> {
    let dir = platform_home_dir()?.join(".hermes").join("mcp-tokens");
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<Option<T>> {
    let body = match std::fs::read(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    serde_json::from_slice(&body)
        .with_context(|| format!("parsing MCP credential record `{}`", path.display()))
        .map(Some)
}

/// Map an arbitrary discovered MCP name to one fixed, traversal-proof cache
/// component. The full SHA-256 digest of the exact UTF-8 name is the identity;
/// the readable prefix is diagnostic only.
fn token_cache_component(name: &str) -> String {
    let readable: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let readable = if readable.is_empty() {
        "server".to_string()
    } else {
        readable
    };
    let digest = Sha256::digest(name.as_bytes());
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{readable}-{hash}")
}

fn canonical_hashed_token_path(dir: &Path, name: &str, suffix: &str) -> PathBuf {
    let digest = Sha256::digest(name.as_bytes());
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    dir.join(format!(".newt-oauth-name-{hash}{suffix}"))
}

fn legacy_full_hashed_token_path(dir: &Path, name: &str, suffix: &str) -> PathBuf {
    dir.join(format!("{}{suffix}", token_cache_component(name)))
}

/// Filename emitted by the first Newt OAuth implementation. This truncated
/// digest is migration input only; new ambiguous names use the full digest.
fn hashed_token_path(dir: &Path, name: &str, suffix: &str) -> PathBuf {
    let component = token_cache_component(name);
    let (readable, digest) = component
        .rsplit_once('-')
        .expect("token cache components always include a digest");
    dir.join(format!("{readable}-{}{suffix}", &digest[..24]))
}

/// Return a Hermes-compatible raw filename component when that representation
/// is portable and cannot overlap one of Hermes' own companion suffixes.
///
/// Mixed case and ordinary dots are intentionally allowed: Hermes has always
/// used the exact configured name. Names ending in `.meta` or `.client` are
/// intrinsically ambiguous (`foo.meta.json` can be either `foo`'s metadata or
/// `foo.meta`'s token). Leading-dot names and legacy hash-shaped names are also
/// reserved so raw names cannot overlap Newt's internal or migration layouts;
/// Windows device names are never safe path components.
fn hermes_raw_component(name: &str) -> Option<&str> {
    let lowercase = name.to_ascii_lowercase();
    let looks_like_legacy_hash = name.rsplit_once('-').is_some_and(|(_, suffix)| {
        matches!(suffix.len(), 24 | 64)
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if name.is_empty()
        || name.len() > 128
        || name.starts_with('.')
        || looks_like_legacy_hash
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || matches!(name, "." | "..")
        || name.ends_with('.')
        || lowercase.ends_with(".meta")
        || lowercase.ends_with(".client")
    {
        return None;
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    if reserved {
        return None;
    }
    Some(name)
}

fn exact_raw_paths(dir: &Path, name: &str) -> Option<CredentialPaths> {
    let component = hermes_raw_component(name)?;
    Some(CredentialPaths {
        token: dir.join(format!("{component}.json")),
        meta: dir.join(format!("{component}.meta.json")),
        client: dir.join(format!("{component}.client.json")),
    })
}

/// Reject a case-fold alias instead of silently sharing credentials. The
/// case-folded manifest/lock key serializes `Foo` and `foo`; this directory scan
/// also catches pre-manifest Hermes files on case-sensitive filesystems.
fn reject_raw_case_aliases(dir: &Path, name: &str, paths: &CredentialPaths) -> anyhow::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let expected: Vec<_> = paths
        .all()
        .into_iter()
        .filter_map(Path::file_name)
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    for entry in std::fs::read_dir(dir)? {
        let actual = entry?.file_name().to_string_lossy().into_owned();
        if expected
            .iter()
            .any(|wanted| actual != *wanted && actual.eq_ignore_ascii_case(wanted))
        {
            anyhow::bail!(
                "MCP credential name `{name}` has a case-fold alias on disk (`{actual}`); rename one server before authenticating"
            );
        }
    }
    Ok(())
}

fn portable_credential_paths(dir: &Path, name: &str) -> anyhow::Result<CredentialPaths> {
    if let Some(paths) = exact_raw_paths(dir, name) {
        reject_raw_case_aliases(dir, name, &paths)?;
        Ok(paths)
    } else {
        Ok(CredentialPaths {
            token: canonical_hashed_token_path(dir, name, ".json"),
            meta: canonical_hashed_token_path(dir, name, ".meta.json"),
            client: canonical_hashed_token_path(dir, name, ".client.json"),
        })
    }
}

#[cfg(test)]
fn token_path(dir: &Path, name: &str, suffix: &str) -> anyhow::Result<PathBuf> {
    let paths = portable_credential_paths(dir, name)?;
    match suffix {
        ".json" => Ok(paths.token),
        ".meta.json" => Ok(paths.meta),
        ".client.json" => Ok(paths.client),
        _ => anyhow::bail!("unsupported MCP credential suffix `{suffix}`"),
    }
}

fn casefold_credential_key(name: &str) -> String {
    let digest = Sha256::digest(name.to_ascii_lowercase().as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn credential_manifest_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!(
        ".newt-oauth-{}.manifest.json",
        casefold_credential_key(name)
    ))
}

fn credential_lock_path(dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    Ok(credential_manifest_destination(dir, name)?.lock_path())
}

fn credential_manifest_destination(
    dir: &Path,
    name: &str,
) -> anyhow::Result<newt_core::atomic_fs::ResolvedPath> {
    newt_core::atomic_fs::ResolvedPath::resolve(&credential_manifest_path(dir, name))
}

struct CredentialLock {
    manifest_destination: newt_core::atomic_fs::ResolvedPath,
    _guard: newt_core::atomic_fs::LockGuard,
}

fn acquire_credential_lock(dir: &Path, name: &str) -> anyhow::Result<CredentialLock> {
    let manifest_destination = credential_manifest_destination(dir, name)?;
    let guard = newt_core::atomic_fs::acquire_lock(&manifest_destination.lock_path())?;
    Ok(CredentialLock {
        manifest_destination,
        _guard: guard,
    })
}

fn full_hashed_credential_paths(dir: &Path, name: &str) -> CredentialPaths {
    CredentialPaths {
        token: legacy_full_hashed_token_path(dir, name, ".json"),
        meta: legacy_full_hashed_token_path(dir, name, ".meta.json"),
        client: legacy_full_hashed_token_path(dir, name, ".client.json"),
    }
}

fn truncated_hashed_credential_paths(dir: &Path, name: &str) -> CredentialPaths {
    CredentialPaths {
        token: hashed_token_path(dir, name, ".json"),
        meta: hashed_token_path(dir, name, ".meta.json"),
        client: hashed_token_path(dir, name, ".client.json"),
    }
}

fn legacy_credential_candidates(dir: &Path, name: &str) -> anyhow::Result<Vec<CredentialPaths>> {
    let mut candidates = vec![portable_credential_paths(dir, name)?];
    for candidate in [
        full_hashed_credential_paths(dir, name),
        truncated_hashed_credential_paths(dir, name),
    ] {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn valid_generation_id(generation: &str) -> bool {
    generation.len() == 32
        && generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn generation_paths(dir: &Path, name: &str, generation: &str) -> CredentialPaths {
    let prefix = format!(
        ".newt-oauth-{}.generation-{generation}",
        casefold_credential_key(name)
    );
    CredentialPaths {
        token: dir.join(format!("{prefix}.token.json")),
        meta: dir.join(format!("{prefix}.meta.json")),
        client: dir.join(format!("{prefix}.client.json")),
    }
}

fn read_credential_manifest(dir: &Path, name: &str) -> anyhow::Result<Option<CredentialManifest>> {
    let destination = credential_manifest_destination(dir, name)?;
    let path = destination.as_path();
    let body = match std::fs::read(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading MCP credential manifest `{}`", path.display()))
        }
    };
    let manifest: CredentialManifest = serde_json::from_slice(&body)
        .with_context(|| format!("parsing MCP credential manifest `{}`", path.display()))?;
    let cursor_is_valid = |digest: &Option<String>| {
        digest.as_ref().is_none_or(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    };
    if !matches!(manifest.version, 1 | CREDENTIAL_MANIFEST_VERSION)
        || manifest.server_name != name
        || !valid_generation_id(&manifest.generation)
        || (manifest.version == CREDENTIAL_MANIFEST_VERSION
            && (!cursor_is_valid(&manifest.hermes_token_sha256)
                || !cursor_is_valid(&manifest.hermes_meta_sha256)
                || !cursor_is_valid(&manifest.hermes_client_sha256)))
    {
        anyhow::bail!(
            "MCP credential manifest for `{name}` has an invalid version, name, or generation"
        );
    }
    Ok(Some(manifest))
}

fn bytes_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_optional_bytes(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_trio_bytes(paths: &CredentialPaths) -> anyhow::Result<[Option<Vec<u8>>; 3]> {
    Ok([
        read_optional_bytes(&paths.token)?,
        read_optional_bytes(&paths.meta)?,
        read_optional_bytes(&paths.client)?,
    ])
}

/// Read one externally-owned Hermes trio without ever associating parsed bytes
/// with hashes from a later rotation. Two identical consecutive reads make an
/// in-flight atomic replacement observable as a retryable error; a replacement
/// after the second read is picked up on the next import and cannot be skipped.
fn stable_hermes_snapshot(paths: &CredentialPaths) -> anyhow::Result<Option<HermesSnapshot>> {
    let first = read_trio_bytes(paths)?;
    let second = read_trio_bytes(paths)?;
    if first != second {
        anyhow::bail!("Hermes MCP credential trio changed while it was being read; retrying later");
    }
    let [Some(token_bytes), Some(meta_bytes), Some(client_bytes)] = second else {
        return Ok(None);
    };
    let token = serde_json::from_slice(&token_bytes)
        .with_context(|| format!("parsing MCP token `{}`", paths.token.display()))?;
    let meta = serde_json::from_slice(&meta_bytes)
        .with_context(|| format!("parsing MCP metadata `{}`", paths.meta.display()))?;
    let client = serde_json::from_slice(&client_bytes)
        .with_context(|| format!("parsing MCP client `{}`", paths.client.display()))?;
    Ok(Some(HermesSnapshot {
        bundle: CredentialBundle {
            token,
            meta,
            client,
        },
        cursor: HermesCursor {
            token: Some(bytes_sha256(&token_bytes)),
            meta: Some(bytes_sha256(&meta_bytes)),
            client: Some(bytes_sha256(&client_bytes)),
        },
    }))
}

fn observed_hermes_cursor(dir: &Path, name: &str) -> anyhow::Result<HermesCursor> {
    let paths = portable_credential_paths(dir, name)?;
    let bytes = read_trio_bytes(&paths)?;
    Ok(HermesCursor {
        token: bytes[0].as_deref().map(bytes_sha256),
        meta: bytes[1].as_deref().map(bytes_sha256),
        client: bytes[2].as_deref().map(bytes_sha256),
    })
}

fn manifest_hermes_cursor(manifest: &CredentialManifest) -> HermesCursor {
    HermesCursor {
        token: manifest.hermes_token_sha256.clone(),
        meta: manifest.hermes_meta_sha256.clone(),
        client: manifest.hermes_client_sha256.clone(),
    }
}

fn legacy_active_paths(dir: &Path, name: &str) -> anyhow::Result<Option<CredentialPaths>> {
    let complete: Vec<_> = legacy_credential_candidates(dir, name)?
        .into_iter()
        .filter(CredentialPaths::complete)
        .collect();
    let Some(first) = complete.first() else {
        return Ok(None);
    };
    let first_bytes: Vec<_> = first
        .all()
        .into_iter()
        .map(std::fs::read)
        .collect::<Result<_, _>>()?;
    for candidate in complete.iter().skip(1) {
        let candidate_bytes: Vec<_> = candidate
            .all()
            .into_iter()
            .map(std::fs::read)
            .collect::<Result<_, _>>()?;
        if candidate_bytes != first_bytes {
            anyhow::bail!(
                "multiple conflicting legacy MCP credential generations exist for `{name}`; run `newt auth {name}` after removing the stale copy"
            );
        }
    }
    Ok(Some(first.clone()))
}

fn active_credential_paths(dir: &Path, name: &str) -> anyhow::Result<Option<CredentialPaths>> {
    if let Some(manifest) = read_credential_manifest(dir, name)? {
        let paths = generation_paths(dir, name, &manifest.generation);
        if !paths.complete() {
            anyhow::bail!(
                "MCP credential manifest for `{name}` points to an incomplete generation"
            );
        }
        return Ok(Some(paths));
    }
    legacy_active_paths(dir, name)
}

fn read_bundle_from_paths(paths: &CredentialPaths) -> anyhow::Result<CredentialBundle> {
    let token = serde_json::from_slice(&std::fs::read(&paths.token)?)
        .with_context(|| format!("parsing MCP token `{}`", paths.token.display()))?;
    let meta = serde_json::from_slice(&std::fs::read(&paths.meta)?)
        .with_context(|| format!("parsing MCP metadata `{}`", paths.meta.display()))?;
    let client = serde_json::from_slice(&std::fs::read(&paths.client)?)
        .with_context(|| format!("parsing MCP client `{}`", paths.client.display()))?;
    Ok(CredentialBundle {
        token,
        meta,
        client,
    })
}

fn read_credential_bundle(dir: &Path, name: &str) -> anyhow::Result<Option<CredentialBundle>> {
    active_credential_paths(dir, name)?
        .as_ref()
        .map(read_bundle_from_paths)
        .transpose()
}

fn read_credential_records(dir: &Path, name: &str) -> anyhow::Result<CredentialRecords> {
    if let Some(bundle) = read_credential_bundle(dir, name)? {
        return Ok(CredentialRecords {
            token: Some(bundle.token),
            meta: Some(bundle.meta),
            client: Some(bundle.client),
        });
    }
    if read_credential_manifest(dir, name)?.is_some() {
        anyhow::bail!("MCP credential manifest for `{name}` is incomplete");
    }
    let present: Vec<_> = legacy_credential_candidates(dir, name)?
        .into_iter()
        .filter(CredentialPaths::any_present)
        .collect();
    if present.len() > 1 {
        anyhow::bail!(
            "multiple partial legacy MCP credential layouts exist for `{name}`; refusing to combine them"
        );
    }
    let Some(paths) = present.first() else {
        return Ok(CredentialRecords::default());
    };
    Ok(CredentialRecords {
        token: read_json(&paths.token)?,
        meta: read_json(&paths.meta)?,
        client: read_json(&paths.client)?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CredentialSnapshot(Vec<(PathBuf, Option<Vec<u8>>)>);

fn credential_snapshot(dir: &Path, name: &str) -> anyhow::Result<CredentialSnapshot> {
    let mut paths = vec![credential_manifest_path(dir, name)];
    for candidate in legacy_credential_candidates(dir, name)? {
        paths.extend(candidate.all().into_iter().map(Path::to_path_buf));
    }
    if let Some(manifest) = read_credential_manifest(dir, name)? {
        paths.extend(
            generation_paths(dir, name, &manifest.generation)
                .all()
                .into_iter()
                .map(Path::to_path_buf),
        );
    }
    paths.sort();
    paths.dedup();
    let records = paths
        .into_iter()
        .map(|path| match std::fs::read(&path) {
            Ok(bytes) => Ok((path, Some(bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((path, None)),
            Err(error) => Err(error),
        })
        .collect::<Result<Vec<_>, std::io::Error>>()?;
    Ok(CredentialSnapshot(records))
}

fn ensure_credential_snapshot(
    dir: &Path,
    name: &str,
    expected: &CredentialSnapshot,
) -> anyhow::Result<()> {
    if credential_snapshot(dir, name)? != *expected {
        anyhow::bail!(
            "MCP credentials for `{name}` changed during browser authorization; refusing to overwrite newer state"
        );
    }
    Ok(())
}

fn hermes_cursor_from_snapshot(
    snapshot: &CredentialSnapshot,
    paths: &CredentialPaths,
) -> HermesCursor {
    let digest_for = |wanted: &Path| {
        snapshot
            .0
            .iter()
            .find(|(path, _)| path == wanted)
            .and_then(|(_, bytes)| bytes.as_deref())
            .map(bytes_sha256)
    };
    HermesCursor {
        token: digest_for(&paths.token),
        meta: digest_for(&paths.meta),
        client: digest_for(&paths.client),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublishPhase {
    GenerationClient,
    GenerationMeta,
    GenerationToken,
    Manifest,
}

fn new_generation_id() -> anyhow::Result<String> {
    let mut random = [0_u8; 16];
    getrandom::getrandom(&mut random)
        .map_err(|error| anyhow::anyhow!("creating MCP credential generation: {error}"))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn publish_credential_generation_with_hook(
    dir: &Path,
    name: &str,
    bundle: &CredentialBundle,
    hermes_cursor: HermesCursor,
    manifest_destination: &newt_core::atomic_fs::ResolvedPath,
    mut after: impl FnMut(PublishPhase) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let generation = new_generation_id()?;
    let generation_paths = generation_paths(dir, name, &generation);
    write_token_file(&generation_paths.client, &bundle.client)?;
    after(PublishPhase::GenerationClient)?;
    write_token_file(&generation_paths.meta, &bundle.meta)?;
    after(PublishPhase::GenerationMeta)?;
    write_token_file(&generation_paths.token, &bundle.token)?;
    after(PublishPhase::GenerationToken)?;

    let manifest = CredentialManifest {
        version: CREDENTIAL_MANIFEST_VERSION,
        server_name: name.to_string(),
        generation: generation.clone(),
        hermes_token_sha256: hermes_cursor.token,
        hermes_meta_sha256: hermes_cursor.meta,
        hermes_client_sha256: hermes_cursor.client,
    };
    let manifest = serde_json::to_vec_pretty(&manifest)?;
    manifest_destination.atomic_write_private(&manifest)?;
    after(PublishPhase::Manifest)?;
    if let Err(error) = cleanup_old_generations(dir, name, &generation) {
        // The manifest commit is already durable. Cleanup failure must not make
        // callers retry a committed token exchange, but stale secret material
        // remains visible in diagnostics for operator remediation.
        tracing::warn!("failed to clean old MCP OAuth generations for `{name}`: {error}");
    }
    Ok(())
}

fn cleanup_old_generations(dir: &Path, name: &str, keep: &str) -> anyhow::Result<()> {
    let prefix = format!(".newt-oauth-{}.generation-", casefold_credential_key(name));
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(remainder) = file_name.strip_prefix(&prefix) else {
            continue;
        };
        let Some((generation, suffix)) = remainder.split_once('.') else {
            continue;
        };
        if generation == keep
            || !valid_generation_id(generation)
            || !matches!(suffix, "token.json" | "meta.json" | "client.json")
        {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn publish_credential_generation(
    dir: &Path,
    name: &str,
    bundle: &CredentialBundle,
    hermes_cursor: HermesCursor,
    transaction: &CredentialLock,
) -> anyhow::Result<()> {
    publish_credential_generation_with_hook(
        dir,
        name,
        bundle,
        hermes_cursor,
        &transaction.manifest_destination,
        |_| Ok(()),
    )
}

fn serialized_equal<T: Serialize>(left: &T, right: &T) -> bool {
    serde_json::to_value(left).ok() == serde_json::to_value(right).ok()
}

fn migrate_legacy_credentials(
    dir: &Path,
    name: &str,
    transaction: &CredentialLock,
) -> anyhow::Result<bool> {
    if let Some(manifest) = read_credential_manifest(dir, name)? {
        let active = read_credential_bundle(dir, name)?.ok_or_else(|| {
            anyhow::anyhow!("MCP credential manifest for `{name}` has no complete generation")
        })?;
        let mirror_paths = portable_credential_paths(dir, name)?;
        // Once Newt owns a complete manifested generation, malformed external
        // Hermes bytes are unusable import input, not a reason to withhold the
        // already-validated Newt credential forever. This matters after an
        // explicit `newt auth` repairs a malformed legacy trio without
        // overwriting it. Never advance the v2 cursor for such bytes.
        let observed = match stable_hermes_snapshot(&mirror_paths) {
            Ok(observed) => observed,
            Err(error) if manifest.version == CREDENTIAL_MANIFEST_VERSION => {
                tracing::warn!(
                    "ignoring malformed or concurrently changing Hermes MCP credentials for `{name}`: {error:#}"
                );
                cleanup_old_generations(dir, name, &manifest.generation)?;
                return Ok(false);
            }
            // A v1 manifest has no cursor. Upgrade it below using a raw digest
            // baseline even when the old mirror cannot be parsed; the active
            // Newt generation remains the only adopted credential.
            Err(error) => {
                tracing::warn!(
                    "recording malformed or concurrently changing Hermes MCP credentials for `{name}` as a v1 migration baseline: {error:#}"
                );
                None
            }
        };
        let observed_cursor = match observed.as_ref() {
            Some(snapshot) => snapshot.cursor.clone(),
            None => observed_hermes_cursor(dir, name)?,
        };
        let previous = manifest_hermes_cursor(&manifest);

        // Version 1 was written by the former mirror publisher and has no
        // cursor. Establish a v2 baseline exactly once. A token-only change may
        // be adopted only when the token itself carries the exact issuer and
        // resource binding; separate flat files have no cross-file commit point,
        // so unchanged companion bytes cannot prove an unbound token belongs to
        // them. Divergent, partial, or unbound external state is merely recorded
        // as the baseline and never copied into Newt's generation.
        if manifest.version == 1 {
            let mut replacement = active.clone();
            if let Some(snapshot) = observed.as_ref() {
                let same_meta = serialized_equal(&snapshot.bundle.meta, &active.meta);
                let same_client = serialized_equal(&snapshot.bundle.client, &active.client);
                let token = &snapshot.bundle.token;
                if same_meta
                    && same_client
                    && token_has_exact_binding(token, &active.meta)
                    && !token.access_token.trim().is_empty()
                    && !serialized_equal(token, &active.token)
                {
                    replacement.token = token.clone();
                }
            }
            publish_credential_generation(dir, name, &replacement, observed_cursor, transaction)?;
            return Ok(true);
        }

        if let Some(snapshot) = observed {
            let same_meta = serialized_equal(&snapshot.bundle.meta, &active.meta);
            let same_client = serialized_equal(&snapshot.bundle.client, &active.client);
            let stable_meta = previous.meta.is_some() && snapshot.cursor.meta == previous.meta;
            let stable_client =
                previous.client.is_some() && snapshot.cursor.client == previous.client;
            let changed_token = previous.token.is_some() && snapshot.cursor.token != previous.token;
            if same_meta && same_client && stable_meta && stable_client && changed_token {
                let token = snapshot.bundle.token;
                if token_has_exact_binding(&token, &active.meta)
                    && !token.access_token.trim().is_empty()
                {
                    let adopted = CredentialBundle {
                        token,
                        meta: active.meta,
                        client: active.client,
                    };
                    publish_credential_generation(
                        dir,
                        name,
                        &adopted,
                        snapshot.cursor,
                        transaction,
                    )?;
                    return Ok(true);
                }
                tracing::warn!(
                    "ignoring Hermes MCP token rotation for `{name}` with an invalid issuer/resource binding"
                );
            }
            // The flat trio is external input. Metadata/client byte changes,
            // partial writes, invalid bindings, or an unchanged token are not
            // imported and do not advance the cursor.
        }
        cleanup_old_generations(dir, name, &manifest.generation)?;
        return Ok(false);
    }
    let Some(paths) = legacy_active_paths(dir, name)? else {
        return Ok(false);
    };
    let snapshot = stable_hermes_snapshot(&paths)?.ok_or_else(|| {
        anyhow::anyhow!("legacy MCP credential trio for `{name}` became incomplete")
    })?;
    let bundle = snapshot.bundle;
    if bundle.token.access_token.trim().is_empty()
        || bundle.meta.resource.is_empty()
        || bundle.meta.issuer.trim().is_empty()
        || validate_https_resource(&bundle.meta.resource).is_err()
        || validate_https_endpoint(&bundle.meta.issuer, "issuer").is_err()
        || validate_https_endpoint(&bundle.meta.token_endpoint, "token endpoint").is_err()
        || !token_has_exact_binding(&bundle.token, &bundle.meta)
        || !registration_matches_meta(&bundle.client, &bundle.meta)
        || !client_auth_is_usable(&bundle.client)
    {
        // Explicit authentication must remain able to repair this state. The
        // loader will still withhold it because no trusted manifest exists and
        // the strict binding checks below fail.
        return Ok(false);
    }
    let cursor = if paths == portable_credential_paths(dir, name)? {
        snapshot.cursor
    } else {
        HermesCursor::default()
    };
    publish_credential_generation(dir, name, &bundle, cursor, transaction)?;
    Ok(true)
}

/// Write one private JSON record through the shared crash-safe replacement
/// primitive. `ResolvedPath` binds lock/stage/commit to one path and
/// `atomic_write_private` supplies Windows replace semantics plus durable sync.
fn write_token_file(path: &Path, data: &impl Serialize) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(data)?;
    newt_core::atomic_fs::ResolvedPath::resolve(path)?.atomic_write_private(&bytes)
}

/// Build an updated token record while preserving extension fields such as
/// `token_type` and `scope` without allowing flattened duplicates to shadow the
/// protected binding fields.
fn updated_token_file(
    access_token: &str,
    refresh_token: Option<&str>,
    expires_in: Option<f64>,
    existing_extra: &BTreeMap<String, serde_json::Value>,
    resource: &str,
    issuer: &str,
) -> TokenFile {
    let mut extra = existing_extra.clone();
    for protected in [
        "access_token",
        "refresh_token",
        "expires_at",
        "resource",
        "issuer",
    ] {
        extra.remove(protected);
    }
    if let Some(ei) = expires_in {
        extra.insert("expires_in".into(), serde_json::Value::from(ei));
    }
    TokenFile {
        access_token: access_token.to_owned(),
        refresh_token: refresh_token.map(str::to_owned),
        expires_at: expires_in.map(|seconds| unix_now() + seconds),
        resource: Some(resource.to_owned()),
        issuer: Some(issuer.to_owned()),
        extra,
    }
}

// ---------------------------------------------------------------------------
// Refresh path (called by load_bearer_token when the stored token is expired)
// ---------------------------------------------------------------------------

fn refresh_form<'a>(refresh_token: &'a str, resource_url: &'a str) -> Vec<(String, String)> {
    vec![
        ("grant_type".into(), "refresh_token".into()),
        ("refresh_token".into(), refresh_token.into()),
        ("resource".into(), resource_url.into()),
    ]
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RefreshSnapshot {
    access_token: String,
    refresh_token: String,
    hermes_cursor: HermesCursor,
    credentials: CredentialSnapshot,
}

fn refresh_snapshot(
    dir: &Path,
    name: &str,
    bundle: &CredentialBundle,
) -> anyhow::Result<RefreshSnapshot> {
    let manifest = read_credential_manifest(dir, name)?
        .ok_or_else(|| anyhow::anyhow!("MCP credential manifest disappeared before refresh"))?;
    Ok(RefreshSnapshot {
        access_token: bundle.token.access_token.clone(),
        refresh_token: bundle
            .token
            .refresh_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("MCP OAuth token has no refresh_token"))?,
        hermes_cursor: manifest_hermes_cursor(&manifest),
        credentials: credential_snapshot(dir, name)?,
    })
}

fn concurrent_winner_bearer(
    current: &CredentialBundle,
    rejected_or_expected: &str,
    resource_url: &str,
) -> anyhow::Result<String> {
    if current.token.access_token.trim().is_empty()
        || current.token.access_token == rejected_or_expected
        || !resource_matches(&current.meta.resource, resource_url)
        || !registration_matches_meta(&current.client, &current.meta)
        || !client_auth_is_usable(&current.client)
        || !token_matches_meta(&current.token, &current.meta)
        || matches!(
            token_load_action(&current.token, unix_now()),
            TokenLoadAction::RefreshRequired
        )
    {
        anyhow::bail!(
            "concurrent MCP credential change did not produce a usable replacement bearer"
        );
    }
    Ok(current.token.access_token.clone())
}

fn persist_refreshed_bundle(
    dir: &Path,
    name: &str,
    expected: &RefreshSnapshot,
    refreshed: &CredentialBundle,
    resource_url: &str,
) -> anyhow::Result<String> {
    let transaction = acquire_credential_lock(dir, name)?;
    migrate_legacy_credentials(dir, name, &transaction)?;
    let current = read_credential_bundle(dir, name)?
        .ok_or_else(|| anyhow::anyhow!("MCP credentials disappeared during refresh"))?;
    if credential_snapshot(dir, name)? != expected.credentials {
        // Another Newt/Hermes writer won while the network request was in
        // flight. Never replay or overwrite a rotated refresh token, and never
        // return the same bearer that triggered the failed refresh.
        return concurrent_winner_bearer(&current, &expected.access_token, resource_url);
    }
    if current.token.access_token != expected.access_token
        || current.token.refresh_token.as_deref() != Some(expected.refresh_token.as_str())
    {
        anyhow::bail!("MCP credential bytes changed without a detectable snapshot change");
    }
    publish_credential_generation(
        dir,
        name,
        refreshed,
        expected.hermes_cursor.clone(),
        &transaction,
    )?;
    Ok(refreshed.token.access_token.clone())
}

async fn try_refresh(
    name: &str,
    resource_url: &str,
    bundle: &CredentialBundle,
    policy: &OAuthHopPolicy,
) -> anyhow::Result<CredentialBundle> {
    let tok = &bundle.token;
    let meta = &bundle.meta;
    let reg = &bundle.client;
    let refresh_token = tok
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("MCP OAuth token has no refresh_token"))?;
    if !resource_matches(meta.resource.as_str(), resource_url) {
        tracing::warn!(
            "MCP OAuth token for `{name}` is bound to `{}`, not `{resource_url}` — withholding",
            meta.resource
        );
        anyhow::bail!("MCP OAuth token resource binding does not match the selected resource");
    }
    if !token_matches_meta(tok, meta) {
        tracing::warn!("MCP OAuth token for `{name}` has a stale issuer/resource binding");
        anyhow::bail!("MCP OAuth token has a stale issuer/resource binding");
    }
    let token_endpoint =
        validate_discovery_hop_with_policy(&meta.token_endpoint, "token endpoint", false, policy)?;
    if reg.issuer.as_deref() != Some(meta.issuer.as_str()) {
        tracing::warn!(
            "MCP OAuth client registration for `{name}` is not bound to issuer `{}` — withholding",
            meta.issuer
        );
        anyhow::bail!("MCP OAuth client registration does not match the selected issuer");
    }

    let http = fenced_client_for_url_with_policy(&token_endpoint, false, policy)?;

    let mut form = refresh_form(refresh_token, &meta.resource);
    let request = apply_client_authentication(http.post(token_endpoint.clone())?, reg, &mut form)?;
    let resp = request
        .form(&form)
        .send()
        .await
        .context("sending MCP OAuth refresh request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = bounded_response_body(resp).await.unwrap_or_default();
        let detail = safe_oauth_error(&body)
            .map(|error| format!(" — {error}"))
            .unwrap_or_default();
        anyhow::bail!("MCP OAuth refresh failed: HTTP {status}{detail}");
    }

    let body = bounded_response_body(resp).await?;
    let new_tok = parse_token_response(&body)?;
    // Merge: start from the existing extra so scope/token_type survive.
    let mut extra = tok.extra.clone();
    for (k, v) in &new_tok.extra {
        extra.insert(k.clone(), v.clone());
    }
    let token = updated_token_file(
        &new_tok.access_token,
        new_tok.refresh_token.as_deref().or(Some(refresh_token)),
        new_tok.expires_in,
        &extra,
        &meta.resource,
        &meta.issuer,
    );
    let refreshed = CredentialBundle {
        token,
        meta: meta.clone(),
        client: reg.clone(),
    };
    Ok(refreshed)
}

// ---------------------------------------------------------------------------
// Public: load (and refresh if needed)
// ---------------------------------------------------------------------------

/// Return a Bearer token only when its stored resource binding matches the
/// selected HTTPS MCP URL. A name-only token or a token for another path or
/// origin is withheld rather than following a mutable config name.
pub async fn load_bearer_token(
    server_name: &str,
    resource_url: &str,
    policy: &OAuthHopPolicy,
) -> Option<String> {
    let dir = hermes_token_dir()?;
    load_bearer_token_from_dir(server_name, resource_url, &dir, policy).await
}

async fn load_bearer_token_from_dir(
    server_name: &str,
    resource_url: &str,
    dir: &Path,
    policy: &OAuthHopPolicy,
) -> Option<String> {
    validate_https_resource(resource_url).ok()?;
    let transaction = acquire_credential_lock(dir, server_name)
        .map_err(|error| {
            tracing::warn!(
                "failed to lock MCP credentials for `{server_name}` — withholding: {error:#}"
            );
            error
        })
        .ok()?;
    if let Err(error) = migrate_legacy_credentials(dir, server_name, &transaction) {
        tracing::warn!("failed to migrate legacy MCP credentials for `{server_name}`: {error:#}");
        return None;
    }
    let bundle = read_credential_bundle(dir, server_name)
        .map_err(|error| {
            tracing::warn!(
                "failed to load MCP credentials for `{server_name}` — withholding: {error:#}"
            );
            error
        })
        .ok()??;
    let meta = &bundle.meta;
    if !resource_matches(meta.resource.as_str(), resource_url) {
        tracing::warn!(
            "MCP OAuth token for `{server_name}` is bound to `{}`, not `{resource_url}` — withholding",
            meta.resource
        );
        return None;
    }
    let client = &bundle.client;
    if !registration_matches_meta(client, meta) {
        tracing::warn!(
            "MCP OAuth client registration for `{server_name}` is not bound to issuer `{}` — withholding",
            meta.issuer
        );
        return None;
    }
    let tok = &bundle.token;
    if !token_matches_meta(tok, meta) {
        tracing::warn!(
            "MCP OAuth token for `{server_name}` does not match its metadata binding — withholding"
        );
        return None;
    }
    if tok.access_token.trim().is_empty() {
        tracing::warn!("MCP OAuth token for `{server_name}` has an empty access_token");
        return None;
    }

    let action = token_load_action(tok, unix_now());
    let current_token = tok.access_token.clone();
    let refresh_state = if matches!(action, TokenLoadAction::UseCurrent) {
        None
    } else {
        Some(
            refresh_snapshot(dir, server_name, &bundle)
                .map_err(|error| {
                    tracing::warn!("failed to snapshot MCP credentials for refresh: {error:#}");
                    error
                })
                .ok()?,
        )
    };
    drop(transaction);

    match action {
        TokenLoadAction::RefreshWithFallback => {
            // Unknown-lifetime access tokens cannot rely on a future 401 retry in
            // the current session-start connection path. Attempt one proactive
            // refresh; an unavailable AS is not a reason to discard the existing
            // bearer because OAuth permits lifetime omission.
            match try_refresh(server_name, resource_url, &bundle, policy).await {
                Ok(refreshed) => {
                    let snapshot = refresh_state
                        .as_ref()
                        .expect("refresh action has a snapshot");
                    match persist_refreshed_bundle(
                        dir,
                        server_name,
                        snapshot,
                        &refreshed,
                        resource_url,
                    ) {
                        Ok(token) => return Some(token),
                        Err(error) => {
                            tracing::warn!(
                                "failed to persist refreshed MCP OAuth token for `{server_name}`: {error:#}"
                            );
                            return None;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        "MCP OAuth refresh for `{server_name}` failed safely: {error:#}"
                    );
                }
            }
            tracing::warn!(
                "MCP OAuth token for `{server_name}` has no declared lifetime and refresh failed; using the existing token once"
            );
            Some(current_token)
        }
        TokenLoadAction::UseCurrent => {
            tracing::debug!("MCP OAuth token for `{server_name}` is current");
            Some(current_token)
        }
        TokenLoadAction::RefreshRequired => {
            tracing::debug!("MCP OAuth token for `{server_name}` is expired — attempting refresh");
            let refreshed = try_refresh(server_name, resource_url, &bundle, policy)
                .await
                .map_err(|error| {
                    tracing::warn!(
                        "MCP OAuth refresh for `{server_name}` failed safely: {error:#}"
                    );
                    error
                })
                .ok()?;
            persist_refreshed_bundle(
                dir,
                server_name,
                refresh_state
                    .as_ref()
                    .expect("refresh action has a snapshot"),
                &refreshed,
                resource_url,
            )
            .map_err(|error| {
                tracing::warn!(
                    "failed to persist refreshed MCP OAuth token for `{server_name}`: {error:#}"
                );
                error
            })
            .ok()
        }
    }
}

/// Force one refresh after the MCP resource rejects a stored Bearer with 401.
/// The same credential lock serializes this with proactive refresh and browser
/// login; callers retry the MCP connection exactly once with the returned token.
pub async fn refresh_bearer_token(
    server_name: &str,
    resource_url: &str,
    rejected_bearer: &str,
    policy: &OAuthHopPolicy,
) -> Option<String> {
    let dir = hermes_token_dir()?;
    refresh_bearer_token_from_dir(server_name, resource_url, rejected_bearer, &dir, policy).await
}

async fn refresh_bearer_token_from_dir(
    server_name: &str,
    resource_url: &str,
    rejected_bearer: &str,
    dir: &Path,
    policy: &OAuthHopPolicy,
) -> Option<String> {
    validate_https_resource(resource_url).ok()?;
    let transaction = acquire_credential_lock(dir, server_name).ok()?;
    migrate_legacy_credentials(dir, server_name, &transaction).ok()?;
    let bundle = read_credential_bundle(dir, server_name).ok()??;
    if !resource_matches(bundle.meta.resource.as_str(), resource_url)
        || !registration_matches_meta(&bundle.client, &bundle.meta)
        || !token_matches_meta(&bundle.token, &bundle.meta)
    {
        return None;
    }
    if bundle.token.access_token != rejected_bearer {
        return concurrent_winner_bearer(&bundle, rejected_bearer, resource_url).ok();
    }
    let snapshot = refresh_snapshot(dir, server_name, &bundle).ok()?;
    drop(transaction);
    let refreshed = try_refresh(server_name, resource_url, &bundle, policy)
        .await
        .ok()?;
    persist_refreshed_bundle(dir, server_name, &snapshot, &refreshed, resource_url).ok()
}

#[derive(Debug, PartialEq, Eq)]
enum TokenLoadAction {
    UseCurrent,
    RefreshWithFallback,
    RefreshRequired,
}

fn token_load_action(token: &TokenFile, now: f64) -> TokenLoadAction {
    match (token.expires_at, token.refresh_token.is_some()) {
        (None, true) => TokenLoadAction::RefreshWithFallback,
        (None, false) => TokenLoadAction::UseCurrent,
        (Some(expires), _) if expires - now > 30.0 => TokenLoadAction::UseCurrent,
        (Some(_), _) => TokenLoadAction::RefreshRequired,
    }
}

// ---------------------------------------------------------------------------
// Servers needing auth (for `newt auth` list view)
// ---------------------------------------------------------------------------

/// A server entry reported by `auth_status()`.
#[derive(Debug, Clone)]
pub struct AuthStatus {
    pub name: String,
    pub state: AuthState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    /// Token file present and not yet expired.
    Valid,
    /// Token file present but expired; refresh_token may save it.
    Expired,
    /// No token file, but a client registration exists — can run the flow.
    NeedsFlow,
    /// Token/client files exist but cannot be safely tied to the configured
    /// resource and authorization server. A fresh `newt auth` migrates them.
    NeedsMigration,
    /// Neither token nor client registration — registration step needed first.
    Unregistered,
}

/// Scan `~/.hermes/mcp-tokens/` and report only state that the connection path
/// can actually use for each configured `(name, resource URL)`.
pub fn auth_status(servers: &[(String, String)]) -> Vec<AuthStatus> {
    let dir = match hermes_token_dir() {
        Some(d) => d,
        None => {
            return servers
                .iter()
                .map(|(name, _)| AuthStatus {
                    name: name.clone(),
                    state: AuthState::Unregistered,
                })
                .collect();
        }
    };

    auth_status_from_dir(servers, &dir)
}

fn auth_status_from_dir(servers: &[(String, String)], dir: &Path) -> Vec<AuthStatus> {
    servers
        .iter()
        .map(|(name, resource)| {
            let records = acquire_credential_lock(dir, name)
                .and_then(|_transaction| read_credential_records(dir, name));
            let state = match records {
                Ok(records) => classify_auth_state(
                    resource,
                    records.token.as_ref(),
                    records.meta.as_ref(),
                    records.client.as_ref(),
                    unix_now(),
                ),
                Err(_) => AuthState::NeedsMigration,
            };
            AuthStatus {
                name: name.clone(),
                state,
            }
        })
        .collect()
}

fn classify_auth_state(
    resource: &str,
    token: Option<&TokenFile>,
    meta: Option<&MetaFile>,
    client: Option<&ClientFile>,
    now: f64,
) -> AuthState {
    let usable_binding = validate_https_resource(resource).is_ok()
        && meta.is_some_and(|meta| {
            resource_matches(meta.resource.as_str(), resource)
                && validate_https_endpoint(&meta.token_endpoint, "token endpoint").is_ok()
        })
        && matches!((client, meta), (Some(client), Some(meta)) if registration_matches_meta(client, meta))
        && token.is_none_or(|token| meta.is_some_and(|meta| token_matches_meta(token, meta)));
    match (token, client, meta, usable_binding) {
        (Some(token), Some(_), Some(_), true)
            if token
                .expires_at
                .map(|expires| expires - now > 30.0)
                .unwrap_or(true) =>
        {
            AuthState::Valid
        }
        (Some(token), Some(_), Some(_), true) if token.refresh_token.is_some() => {
            AuthState::Expired
        }
        (Some(_), Some(_), Some(_), true) => AuthState::NeedsFlow,
        (Some(_), _, _, _) | (_, Some(_), Some(_), false) => AuthState::NeedsMigration,
        (None, Some(client), _, _) if client.issuer.is_none() => AuthState::NeedsMigration,
        (None, Some(_), _, _) => AuthState::NeedsFlow,
        _ => AuthState::Unregistered,
    }
}

fn registration_matches_meta(client: &ClientFile, meta: &MetaFile) -> bool {
    client.issuer.as_deref() == Some(meta.issuer.as_str())
}

fn token_matches_meta(token: &TokenFile, meta: &MetaFile) -> bool {
    token_has_exact_binding(token, meta)
}

fn token_has_exact_binding(token: &TokenFile, meta: &MetaFile) -> bool {
    token.resource.as_deref() == Some(meta.resource.as_str())
        && token.issuer.as_deref() == Some(meta.issuer.as_str())
}

// ---------------------------------------------------------------------------
// Interactive OAuth flow
// ---------------------------------------------------------------------------

mod oauth_flow;

// `lib.rs` calls this as `mcp_token::run_oauth_flow`; the path is preserved.
pub(crate) use oauth_flow::run_oauth_flow;
// Reached by the sections above, which stayed behind.
use oauth_flow::{
    fenced_client_for_url_with_policy, resource_matches, validate_discovery_hop_with_policy,
    validate_https_endpoint, validate_https_resource,
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "mcp_token_tests/tests.rs"]
mod tests;
