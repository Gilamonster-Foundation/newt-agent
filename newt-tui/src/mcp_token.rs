//! Hermes-compatible MCP OAuth token loader and interactive auth flow.
//!
//! **Token loading** (`load_bearer_token`): reads tokens stored by hermes-agent
//! in `~/.hermes/mcp-tokens/<name>.json`, refreshes via refresh_token grant if
//! expired, and writes updated tokens back so hermes picks them up too.
//!
//! **Interactive auth** (`run_oauth_flow`): implements the MCP OAuth 2.1
//! authorization-code + PKCE flow end-to-end — metadata discovery, PKCE
//! generation, browser open, local callback server, token exchange — and writes
//! the resulting tokens to `~/.hermes/mcp-tokens/` in the same format hermes
//! uses, so both tools share the same auth state going forward.
//!
//! Token file layout (all under `~/.hermes/mcp-tokens/`):
//!   `<name>.json`        — access_token, refresh_token, expires_at (Unix f64)
//!   `<name>.meta.json`   — authorization_endpoint, token_endpoint, …
//!   `<name>.client.json` — client_id, redirect_uris, …

use std::collections::BTreeMap;
use std::io::{Read, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    // Pass through the rest so we can rewrite the file without losing fields.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

/// The subset of `<name>.meta.json` we need for auth.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct MetaFile {
    #[serde(default)]
    authorization_endpoint: Option<String>,
    token_endpoint: String,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

/// The subset of `<name>.client.json` we need.
#[derive(Debug, Clone, Deserialize)]
struct ClientFile {
    client_id: String,
    #[serde(default)]
    redirect_uris: Vec<String>,
}

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

/// OAuth server metadata from `/.well-known/oauth-authorization-server`.
#[derive(Debug, Deserialize, Serialize)]
struct OAuthMeta {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn hermes_token_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".hermes").join("mcp-tokens");
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

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write `data` to `path` with 0o600 permissions (owner-only). Uses an atomic
/// rename via a temp file so a crash never leaves a half-written token file.
fn write_token_file(path: &Path, data: &impl Serialize) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        let json = serde_json::to_string_pretty(data)?;
        f.write_all(json.as_bytes())?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Persist an updated token dict. Takes the *existing* flat extra dict so we
/// don't lose fields like `token_type` and `scope` that aren't in TokenResponse.
fn persist_tokens(
    path: &Path,
    access_token: &str,
    refresh_token: Option<&str>,
    expires_in: Option<f64>,
    existing_extra: &BTreeMap<String, serde_json::Value>,
) {
    let mut out = existing_extra.clone();
    out.insert(
        "access_token".into(),
        serde_json::Value::String(access_token.to_owned()),
    );
    if let Some(rt) = refresh_token {
        out.insert(
            "refresh_token".into(),
            serde_json::Value::String(rt.to_owned()),
        );
    }
    if let Some(ei) = expires_in {
        out.insert("expires_in".into(), serde_json::Value::from(ei));
        out.insert(
            "expires_at".into(),
            serde_json::Value::from(unix_now() + ei),
        );
    }
    if let Err(e) = write_token_file(path, &out) {
        tracing::warn!("failed to write token file {}: {e}", path.display());
    }
}

// ---------------------------------------------------------------------------
// Refresh path (called by load_bearer_token when the stored token is expired)
// ---------------------------------------------------------------------------

async fn try_refresh(name: &str, tok: &TokenFile, token_dir: &Path) -> Option<String> {
    let refresh_token = tok.refresh_token.as_deref()?;
    let meta: MetaFile = read_json(&token_dir.join(format!("{name}.meta.json")))?;
    let reg: ClientFile = read_json(&token_dir.join(format!("{name}.client.json")))?;

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;

    let resp = http
        .post(&meta.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &reg.client_id),
        ])
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!(
            "MCP OAuth refresh for `{name}` failed: HTTP {}",
            resp.status()
        );
        return None;
    }

    let new_tok: TokenResponse = resp.json().await.ok()?;
    let path = token_dir.join(format!("{name}.json"));

    // Merge: start from the existing extra so scope/token_type survive.
    let mut extra = tok.extra.clone();
    for (k, v) in &new_tok.extra {
        extra.insert(k.clone(), v.clone());
    }
    persist_tokens(
        &path,
        &new_tok.access_token,
        new_tok.refresh_token.as_deref().or(Some(refresh_token)),
        new_tok.expires_in,
        &extra,
    );

    tracing::info!("MCP OAuth token refreshed for `{name}`");
    Some(new_tok.access_token)
}

// ---------------------------------------------------------------------------
// Public: load (and refresh if needed)
// ---------------------------------------------------------------------------

/// Return a valid Bearer token for `server_name` from `~/.hermes/mcp-tokens/`.
/// Refreshes via refresh_token grant if expired. Returns `None` when no token
/// file exists or refresh fails — caller should run `newt auth <server>`.
pub async fn load_bearer_token(server_name: &str) -> Option<String> {
    let dir = hermes_token_dir()?;
    let tok: TokenFile = read_json(&dir.join(format!("{server_name}.json")))?;

    let is_valid = tok
        .expires_at
        .map(|ea| ea - unix_now() > 30.0)
        .unwrap_or(false);
    if is_valid {
        tracing::debug!("MCP OAuth token for `{server_name}` is current");
        return Some(tok.access_token.clone());
    }

    tracing::debug!("MCP OAuth token for `{server_name}` is expired — attempting refresh");
    try_refresh(server_name, &tok, &dir).await
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
    /// Token file present and not yet expired (or no expiry field).
    Valid,
    /// Token file present but expired; refresh_token may save it.
    Expired,
    /// No token file, but a client registration exists — can run the flow.
    NeedsFlow,
    /// Neither token nor client registration — registration step needed first.
    Unregistered,
}

/// Scan `~/.hermes/mcp-tokens/` and report per-server auth state for every
/// server in `server_names`.
pub fn auth_status(server_names: &[String]) -> Vec<AuthStatus> {
    let dir = match hermes_token_dir() {
        Some(d) => d,
        None => {
            return server_names
                .iter()
                .map(|n| AuthStatus {
                    name: n.clone(),
                    state: AuthState::Unregistered,
                })
                .collect();
        }
    };

    server_names
        .iter()
        .map(|name| {
            let tok_path = dir.join(format!("{name}.json"));
            let client_path = dir.join(format!("{name}.client.json"));
            let state = if tok_path.exists() {
                let tok: Option<TokenFile> = read_json(&tok_path);
                match tok {
                    Some(t)
                        if t.expires_at
                            .map(|ea| ea - unix_now() > 30.0)
                            .unwrap_or(true) =>
                    {
                        AuthState::Valid
                    }
                    _ => AuthState::Expired,
                }
            } else if client_path.exists() {
                AuthState::NeedsFlow
            } else {
                AuthState::Unregistered
            };
            AuthStatus {
                name: name.clone(),
                state,
            }
        })
        .collect()
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
    // Use /dev/urandom directly — no extra dep, always available on Unix/macOS.
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;

    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let verifier = engine.encode(buf);

    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = engine.encode(digest);

    Ok(PkceChallenge {
        verifier,
        challenge,
    })
}

/// Discover OAuth metadata from `<server_url>/.well-known/oauth-authorization-server`.
/// Falls back to trying `<server_url>/.well-known/openid-configuration`.
async fn discover_oauth_meta(
    http: &reqwest::Client,
    server_url: &str,
) -> anyhow::Result<OAuthMeta> {
    let base = server_url.trim_end_matches('/');
    for path in &[
        "/.well-known/oauth-authorization-server",
        "/.well-known/openid-configuration",
    ] {
        let url = format!("{base}{path}");
        let resp = http.get(&url).send().await?;
        if resp.status().is_success() {
            return Ok(resp.json::<OAuthMeta>().await?);
        }
    }
    anyhow::bail!("OAuth metadata discovery failed for {server_url}");
}

/// Parse `code` and `state` from a callback path like `/callback?code=X&state=Y`.
fn parse_callback(path: &str) -> (Option<String>, Option<String>) {
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k {
                "code" => code = Some(urlencoding_decode(v)),
                "state" => state = Some(urlencoding_decode(v)),
                _ => {}
            }
        }
    }
    (code, state)
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
        out.push(b as char);
    }
    out
}

fn random_state() -> String {
    let mut buf = [0u8; 16];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf));
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Run the full MCP OAuth 2.1 authorization-code + PKCE flow for `server_name`.
///
/// `server_url` is the MCP endpoint URL — used for OAuth metadata discovery when
/// no `<name>.meta.json` is present.
///
/// On success writes `~/.hermes/mcp-tokens/<name>.json` (access_token +
/// refresh_token + expires_at) so both newt and hermes can use it.
pub async fn run_oauth_flow(server_name: &str, server_url: &str) -> anyhow::Result<()> {
    let dir = {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
        let d = home.join(".hermes").join("mcp-tokens");
        std::fs::create_dir_all(&d)?;
        d
    };

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // ── 1. OAuth server metadata ──────────────────────────────────────────
    let meta_path = dir.join(format!("{server_name}.meta.json"));
    let (auth_endpoint, token_endpoint, meta_extra) =
        if let Some(m) = read_json::<OAuthMeta>(&meta_path) {
            // Already discovered — reuse.
            (m.authorization_endpoint, m.token_endpoint, m.extra)
        } else {
            // Discover and cache for next time.
            let m = discover_oauth_meta(&http, server_url).await?;
            let me = OAuthMeta {
                authorization_endpoint: m.authorization_endpoint.clone(),
                token_endpoint: m.token_endpoint.clone(),
                extra: m.extra.clone(),
            };
            let _ = write_token_file(&meta_path, &me);
            (m.authorization_endpoint, m.token_endpoint, m.extra)
        };
    let _ = meta_extra; // kept for future use (e.g. registration endpoint)

    // ── 2. Client registration ────────────────────────────────────────────
    let client_path = dir.join(format!("{server_name}.client.json"));
    let client: ClientFile = read_json(&client_path).ok_or_else(|| {
        anyhow::anyhow!(
            "No client registration found for `{server_name}` \
             (~/.hermes/mcp-tokens/{server_name}.client.json missing).\n\
             Run `hermes mcp login {server_name}` first to register this client,\n\
             or configure a client_id in the newt MCP server config."
        )
    })?;

    let redirect_uri = client
        .redirect_uris
        .first()
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:0/callback".to_string());

    // Parse the port from the stored redirect URI so we bind to the right one.
    let callback_port: u16 = redirect_uri
        .split(':')
        .nth(2)
        .and_then(|s| s.split('/').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0); // 0 = OS picks a free port (TcpListener bind handles it)

    // Bind the callback listener NOW (before opening the browser) so the port
    // is guaranteed to be open when the browser redirects back.
    let listener = TcpListener::bind(("127.0.0.1", callback_port))?;
    let actual_port = listener.local_addr()?.port();

    // If the stored redirect_uri used port 0, rewrite to the actual port.
    let redirect_uri = if callback_port == 0 {
        format!("http://127.0.0.1:{actual_port}/callback")
    } else {
        redirect_uri
    };

    // ── 3. PKCE ───────────────────────────────────────────────────────────
    let pkce = gen_pkce()?;
    let state = random_state();

    // ── 4. Authorization URL ──────────────────────────────────────────────
    let auth_url = format!(
        "{auth_endpoint}?response_type=code\
         &client_id={client_id}\
         &redirect_uri={redirect_uri_enc}\
         &code_challenge={challenge}\
         &code_challenge_method=S256\
         &state={state}",
        client_id = urlencoding_encode(&client.client_id),
        redirect_uri_enc = urlencoding_encode(&redirect_uri),
        challenge = pkce.challenge,
    );

    // ── 5. Open browser ───────────────────────────────────────────────────
    println!("\nMCP OAuth: authorization required for `{server_name}`.");
    println!("Opening your browser to complete the login…\n");
    println!("  {auth_url}\n");
    println!("(If the browser did not open, paste the URL above manually.)");

    // Best-effort browser open — failure is not fatal; the user can paste.
    let _ = std::process::Command::new("open").arg(&auth_url).spawn();

    // ── 6. Wait for callback ──────────────────────────────────────────────
    println!("\nWaiting for authorization callback on port {actual_port}…");

    // Hand the already-bound listener to the blocking callback waiter via a
    // tokio blocking task so we don't freeze the async runtime.
    let callback_path = tokio::task::spawn_blocking(move || {
        let (mut stream, _) = listener.accept()?;
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf)?;
        let req = std::str::from_utf8(&buf[..n]).unwrap_or("").to_string();
        let body = b"<html><body><h2>Authorization successful</h2>\
                      <p>You can close this tab and return to newt.</p></body></html>";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.write_all(body);
        let path = req
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();
        Ok::<String, std::io::Error>(path)
    })
    .await??;

    let (code, returned_state) = parse_callback(&callback_path);

    if returned_state.as_deref() != Some(&state) {
        anyhow::bail!("OAuth state mismatch — possible CSRF; aborting");
    }
    let code = code.ok_or_else(|| anyhow::anyhow!("No authorization code in callback"))?;

    // ── 7. Token exchange ─────────────────────────────────────────────────
    println!("Authorization code received — exchanging for tokens…");

    let resp = http
        .post(&token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("client_id", &client.client_id),
            ("code_verifier", &pkce.verifier),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token exchange failed: HTTP {status} — {body}");
    }

    let tok: TokenResponse = resp.json().await?;

    // ── 8. Persist ────────────────────────────────────────────────────────
    let tok_path = dir.join(format!("{server_name}.json"));
    persist_tokens(
        &tok_path,
        &tok.access_token,
        tok.refresh_token.as_deref(),
        tok.expires_in,
        &tok.extra,
    );

    println!("✓ Authenticated `{server_name}`. Token saved to ~/.hermes/mcp-tokens/");
    Ok(())
}

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
    fn parse_callback_extracts_code_and_state() {
        let (code, state) = parse_callback("/callback?code=AUTH_CODE_HERE&state=abc123");
        assert_eq!(code.as_deref(), Some("AUTH_CODE_HERE"));
        assert_eq!(state.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_callback_returns_none_for_missing_params() {
        let (code, state) = parse_callback("/callback?error=access_denied");
        assert!(code.is_none());
        assert!(state.is_none());
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
}
