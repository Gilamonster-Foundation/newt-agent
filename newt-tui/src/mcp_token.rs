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

struct PkceChallenge {
    verifier: String,
    challenge: String,
}

fn gen_pkce() -> anyhow::Result<PkceChallenge> {
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

fn fenced_client_for_url_with_policy(
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
fn validate_discovery_hop(
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

fn validate_discovery_hop_with_policy(
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

fn validate_https_resource(resource_url: &str) -> anyhow::Result<reqwest::Url> {
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
fn canonical_resource_identifier(input: &str, parsed: &reqwest::Url) -> String {
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

fn validate_https_endpoint(endpoint: &str, kind: &str) -> anyhow::Result<reqwest::Url> {
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

fn resource_matches(bound: &str, selected: &str) -> bool {
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
fn protected_resource_metadata_url(resource_url: &str) -> anyhow::Result<String> {
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
fn authorization_server_metadata_url(issuer: &str) -> anyhow::Result<String> {
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
fn resource_origin(resource_url: &str) -> anyhow::Result<String> {
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
fn parse_bearer_challenge(header: &str) -> anyhow::Result<Option<BearerChallenge>> {
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

fn merge_scopes(current: Option<&str>, previously_granted: Option<&str>) -> Option<String> {
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

fn prior_scope_for_binding(
    token: Option<&TokenFile>,
    metadata: Option<&MetaFile>,
    resource: &str,
    issuer: &str,
) -> Option<String> {
    let token = token?;
    let metadata = metadata?;
    if metadata.resource != resource
        || metadata.issuer != issuer
        || token
            .resource
            .as_deref()
            .is_some_and(|bound| bound != resource)
        || token.issuer.as_deref().is_some_and(|bound| bound != issuer)
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
/// the selected authorization server using RFC 8414. The pre-RFC MCP fallback
/// is retained only when the protected-resource well-known document is absent:
/// the MCP resource origin itself is treated as the issuer. Invalid metadata
/// never downgrades to the legacy path.
#[cfg(test)]
async fn discover_oauth_meta_with_policy(
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

async fn discover_oauth_meta_for_client_with_policy(
    server_url: &str,
    allow_test_loopback_http: bool,
    client: Option<&ClientFile>,
    allow_re_registration: bool,
    policy: &OAuthHopPolicy,
) -> anyhow::Result<DiscoveredOAuthMeta> {
    let resource = validate_resource_url(server_url, allow_test_loopback_http)?;
    let canonical_resource = canonical_resource_identifier(server_url, &resource);
    let (protected, challenge_scope) =
        fetch_protected_resource_meta(server_url, allow_test_loopback_http, policy).await?;
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
        match fetch_authorization_server_meta(&issuer, allow_test_loopback_http, policy).await {
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
fn parse_callback(path: &str) -> CallbackParams {
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

fn validate_authorization_response(
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

fn callback_request_target(request: &str) -> anyhow::Result<&str> {
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

fn urlencoding_decode(s: &str) -> String {
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
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(windows)]
    let mut command = {
        let mut command = std::process::Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(url).spawn()
}

struct CallbackTarget {
    redirect_uri: String,
    bind_addr: std::net::SocketAddr,
    path: String,
}

fn callback_target(client: &ClientFile) -> anyhow::Result<CallbackTarget> {
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

fn bind_client_registration(
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

fn client_is_portable_cimd(client: &ClientFile) -> bool {
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
fn dcr_registration_is_eligible(
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

fn order_authorization_servers(
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

async fn register_public_client(
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

fn issuer_redirect_uris(issuer: &str) -> Vec<String> {
    vec![format!(
        "http://127.0.0.1:0{}",
        issuer_callback_path(issuer)
    )]
}

fn client_has_issuer_distinct_redirect(client: &ClientFile, issuer: &str) -> bool {
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

async fn resolve_client_registration(
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

fn build_authorization_url(
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
fn urlencoding_encode(s: &str) -> String {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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

        let allowed =
            OAuthHopPolicy::new(&newt_core::Scope::only(["auth.example.test".to_string()]));
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
        let callback = parse_callback(
            "/callback?code=AUTH_CODE_HERE&state=abc123&iss=https%3A%2F%2Fauth.example",
        );
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
        let value =
            r#"Bearer realm="mcp", resource_metadata="https://mcp.example.test/oauth-meta""#;
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

    #[test]
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

    #[test]
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

        let combined =
            r#"Bearer resource_metadata="https://mcp.example/meta", Basic realm="legacy""#;
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
        let (rebound, changed) =
            bind_client_registration(client, "https://new.example", true).unwrap();
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
        let error =
            validate_authorization_response(&mismatch, "state", "https://auth.example", true)
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
            validate_authorization_response(&absent, "state", "https://auth.example", true)
                .is_err()
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
            callback_request_target("GET https://attacker.example/callback HTTP/1.1\r\n\r\n")
                .is_err()
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
                    format!(
                        "Bearer resource_metadata=\"{metadata_url}\", scope=\"challenge:scope\""
                    )
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
            resolve_client_registration(
                Some(client),
                &discovered,
                None,
                true,
                &test_oauth_policy(),
            )
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

    #[test]
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

    #[test]
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

        let (registered, migrated) = resolve_client_registration(
            Some(stored),
            &discovered,
            None,
            true,
            &test_oauth_policy(),
        )
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

        let (registered, migrated) = resolve_client_registration(
            Some(stored),
            &discovered,
            None,
            true,
            &test_oauth_policy(),
        )
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[tokio::test]
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

    #[test]
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
}
