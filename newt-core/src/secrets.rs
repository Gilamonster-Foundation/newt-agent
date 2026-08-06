//! **Encrypted-at-rest API-token store** (age v1).
//!
//! Tokens pasted into `newt setup` land on disk as age ciphertext
//! (`~/.newt/backends/<name>.token.age`), never plaintext. Two modes:
//!
//! - **Blank passphrase** (the default): encrypted to a machine-local X25519
//!   identity at `~/.newt/secrets/identity.txt` (0600, dir 0700), generated
//!   on first use. Decryption is transparent. This is deliberately
//!   key-beside-lock — an encoding-plus-permissions upgrade over plaintext
//!   and a real win for backups/dotfile repos that copy `backends/` but not
//!   `secrets/`, not a vault. Documented honestly in the setup guide.
//! - **Passphrase**: age scrypt encryption; the key never touches disk.
//!   Interactive sessions collect the passphrase once (TUI preflight);
//!   headless runs read `NEWT_TOKEN_PASSPHRASE` lazily — a locked token is a
//!   warn-once + `None`, never a hang.
//!
//! Files are ASCII-armored (self-describing, text-safe, decryptable with the
//! stock age/rage CLIs: `age -d -i ~/.newt/secrets/identity.txt <file>`).
//! Legacy plaintext `.token` files keep working forever through the same
//! choke point ([`token_from_file_bytes`] — the ONE reader behind every
//! `resolve_api_key`).
//!
//! Layering (the `dgx_pull.rs` discipline): a pure, fs-free core
//! ([`classify`], the encrypt/decrypt pairs, [`token_from_file_bytes`] over
//! an injected [`UnlockProvider`]) under a thin IO shell ([`store_token`],
//! [`resolve_token_file`], the process-wide [`session`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use age::secrecy::{ExposeSecret, SecretString};

/// Binary age v1 header magic.
pub const AGE_BINARY_MAGIC: &[u8] = b"age-encryption.org/v1";
/// Armored age magic (first line).
pub const AGE_ARMOR_MAGIC: &[u8] = b"-----BEGIN AGE ENCRYPTED FILE-----";
/// The headless unlock channel for passphrase-protected tokens.
pub const PASSPHRASE_ENV: &str = "NEWT_TOKEN_PASSPHRASE";

/// What a token file's bytes are, sniffed without any key material. Pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenFileKind {
    /// Neither age magic: legacy plaintext (first non-empty line is the token).
    Plaintext,
    /// Age file whose header carries an X25519 stanza (machine identity).
    AgeX25519,
    /// Age file whose header carries a scrypt stanza (passphrase-protected).
    AgeScrypt,
    /// Age magic present but the stanza could not be sniffed — still routed
    /// to age decryption; failures surface as `Corrupt`.
    AgeUnknown,
}

/// Typed failures — every message is actionable (what to set / re-run).
#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error(
        "token file {path} is passphrase-protected and no passphrase is available; \
         set {PASSPHRASE_ENV} for headless runs, or start newt interactively to unlock"
    )]
    PassphraseRequired { path: PathBuf },
    #[error("wrong passphrase for {path}")]
    WrongPassphrase { path: PathBuf },
    #[error(
        "token file {path} is encrypted to a machine identity, but this machine's \
         ~/.newt/secrets/identity.txt is missing — re-run `newt setup` to re-store the token"
    )]
    IdentityMissing { path: PathBuf },
    #[error("token file {path} is corrupt or not a valid age file: {detail}")]
    Corrupt { path: PathBuf, detail: String },
    #[error("could not locate the home directory for ~/.newt/secrets")]
    NoHome,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------------
// Pure core — no fs, no env
// ---------------------------------------------------------------------------

/// Sniff `bytes` — see [`TokenFileKind`]. Pure.
pub fn classify(bytes: &[u8]) -> TokenFileKind {
    if bytes.starts_with(AGE_BINARY_MAGIC) {
        return classify_header(bytes);
    }
    if bytes.starts_with(AGE_ARMOR_MAGIC) {
        // Decode the first few base64 payload lines into header bytes; the
        // stanza tag sits well within them.
        let text = String::from_utf8_lossy(bytes);
        let b64: String = text
            .lines()
            .skip(1)
            .take_while(|l| !l.starts_with("---"))
            .take(8)
            .collect();
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&b64));
        return match decoded {
            Ok(header) => classify_header(&header),
            Err(_) => TokenFileKind::AgeUnknown,
        };
    }
    TokenFileKind::Plaintext
}

fn classify_header(header: &[u8]) -> TokenFileKind {
    let text = String::from_utf8_lossy(header);
    if text.contains("-> scrypt") {
        TokenFileKind::AgeScrypt
    } else if text.contains("-> X25519") {
        TokenFileKind::AgeX25519
    } else {
        TokenFileKind::AgeUnknown
    }
}

/// Newtype over `age::x25519::Identity` so age types never appear in
/// newt-core's public API (semver insulation).
#[derive(Clone)]
pub struct TokenIdentity(age::x25519::Identity);

impl TokenIdentity {
    pub fn generate() -> Self {
        Self(age::x25519::Identity::generate())
    }

    /// Parse a standard age identity file body: `#` comments and blank lines
    /// skipped, first `AGE-SECRET-KEY-1…` line wins.
    pub fn from_file_str(s: &str) -> Result<Self, String> {
        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            return line
                .parse::<age::x25519::Identity>()
                .map(Self)
                .map_err(|e| e.to_string());
        }
        Err("no AGE-SECRET-KEY line found".to_string())
    }

    /// Serialize as a standard age identity file (interoperable with the
    /// age/rage CLIs).
    pub fn to_file_string(&self, created: &str) -> SecretString {
        SecretString::from(format!(
            "# created: {created}\n# public key: {}\n{}\n",
            self.public(),
            self.0.to_string().expose_secret()
        ))
    }

    /// The Bech32 public half (`age1…`) — for the identity-file comment and
    /// doctor output.
    pub fn public(&self) -> String {
        self.0.to_public().to_string()
    }
}

fn armor_encrypt(recipient: &dyn age::Recipient, plaintext: &[u8]) -> Result<String, String> {
    use std::io::Write as _;
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(recipient)).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let armored = age::armor::ArmoredWriter::wrap_output(&mut out, age::armor::Format::AsciiArmor)
        .map_err(|e| e.to_string())?;
    let mut writer = encryptor.wrap_output(armored).map_err(|e| e.to_string())?;
    writer.write_all(plaintext).map_err(|e| e.to_string())?;
    writer
        .finish()
        .and_then(|armored| armored.finish())
        .map_err(|e| e.to_string())?;
    String::from_utf8(out).map_err(|e| e.to_string())
}

fn armor_decrypt(identity: &dyn age::Identity, bytes: &[u8]) -> Result<Vec<u8>, age::DecryptError> {
    use std::io::Read as _;
    let reader = age::armor::ArmoredReader::new(bytes);
    let decryptor = age::Decryptor::new_buffered(reader)?;
    let mut plaintext = Vec::new();
    decryptor
        .decrypt(std::iter::once(identity))?
        .read_to_end(&mut plaintext)
        .map_err(age::DecryptError::from)?;
    Ok(plaintext)
}

/// Encrypt to the machine identity's recipient. Armored output. Pure.
pub fn encrypt_to_identity(ident: &TokenIdentity, plaintext: &[u8]) -> Result<String, String> {
    armor_encrypt(&ident.0.to_public(), plaintext)
}

/// Encrypt under an age scrypt passphrase. Armored output. Pure (CPU-heavy:
/// scrypt runs its full work factor).
pub fn encrypt_with_passphrase(pass: &SecretString, plaintext: &[u8]) -> Result<String, String> {
    armor_encrypt(&age::scrypt::Recipient::new(pass.clone()), plaintext)
}

/// Decrypt (armored or binary — auto-detected) with the machine identity.
pub fn decrypt_with_identity(
    ident: &TokenIdentity,
    bytes: &[u8],
) -> Result<Vec<u8>, age::DecryptError> {
    armor_decrypt(&ident.0, bytes)
}

/// Decrypt (armored or binary) with a passphrase.
pub fn decrypt_with_passphrase(
    pass: &SecretString,
    bytes: &[u8],
) -> Result<Vec<u8>, age::DecryptError> {
    armor_decrypt(&age::scrypt::Identity::new(pass.clone()), bytes)
}

/// The seam between pure token resolution and where keys come from. The
/// production implementation is the process [`session`]; unit tests inject
/// [`StaticUnlock`].
pub trait UnlockProvider {
    /// The machine identity, if one exists on this host (the READ path never
    /// generates one).
    fn machine_identity(&self) -> Option<TokenIdentity>;
    /// The session passphrase, if one has been provided (TUI prompt or env).
    fn passphrase(&self) -> Option<SecretString>;
}

/// A fixed-value [`UnlockProvider`] for tests and one-shot unlock attempts.
pub struct StaticUnlock {
    pub identity: Option<TokenIdentity>,
    pub passphrase: Option<SecretString>,
}

impl UnlockProvider for StaticUnlock {
    fn machine_identity(&self) -> Option<TokenIdentity> {
        self.identity.clone()
    }
    fn passphrase(&self) -> Option<SecretString> {
        self.passphrase.clone()
    }
}

/// THE choke point: the bytes of an `api_key_file` → bearer token.
///
/// Plaintext → first non-empty trimmed line (exactly the legacy
/// `resolve_api_key` rule). Age → decrypt via `unlock`, then the same
/// first-line rule on the decrypted text. `path` is error context only — no
/// IO happens here. Pure.
pub fn token_from_file_bytes(
    bytes: &[u8],
    path: &Path,
    unlock: &dyn UnlockProvider,
) -> Result<Option<String>, SecretsError> {
    let kind = classify(bytes);
    let plaintext: Vec<u8> = match kind {
        TokenFileKind::Plaintext => bytes.to_vec(),
        TokenFileKind::AgeScrypt => {
            let Some(pass) = unlock.passphrase() else {
                return Err(SecretsError::PassphraseRequired {
                    path: path.to_path_buf(),
                });
            };
            decrypt_with_passphrase(&pass, bytes).map_err(|e| match e {
                age::DecryptError::DecryptionFailed | age::DecryptError::NoMatchingKeys => {
                    SecretsError::WrongPassphrase {
                        path: path.to_path_buf(),
                    }
                }
                other => SecretsError::Corrupt {
                    path: path.to_path_buf(),
                    detail: other.to_string(),
                },
            })?
        }
        TokenFileKind::AgeX25519 => {
            let Some(ident) = unlock.machine_identity() else {
                return Err(SecretsError::IdentityMissing {
                    path: path.to_path_buf(),
                });
            };
            decrypt_with_identity(&ident, bytes).map_err(|e| match e {
                age::DecryptError::NoMatchingKeys => SecretsError::IdentityMissing {
                    path: path.to_path_buf(),
                },
                other => SecretsError::Corrupt {
                    path: path.to_path_buf(),
                    detail: other.to_string(),
                },
            })?
        }
        TokenFileKind::AgeUnknown => {
            // Route through whatever key material exists; failures are honest.
            let attempt = unlock
                .machine_identity()
                .map(|ident| decrypt_with_identity(&ident, bytes))
                .or_else(|| {
                    unlock
                        .passphrase()
                        .map(|p| decrypt_with_passphrase(&p, bytes))
                });
            match attempt {
                Some(Ok(pt)) => pt,
                Some(Err(e)) => {
                    return Err(SecretsError::Corrupt {
                        path: path.to_path_buf(),
                        detail: e.to_string(),
                    })
                }
                None => {
                    return Err(SecretsError::PassphraseRequired {
                        path: path.to_path_buf(),
                    })
                }
            }
        }
    };
    let text = String::from_utf8(plaintext).map_err(|_| SecretsError::Corrupt {
        path: path.to_path_buf(),
        detail: "decrypted payload is not UTF-8".to_string(),
    })?;
    Ok(text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string))
}

// ---------------------------------------------------------------------------
// Process-wide unlock session (memory only)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SessionState {
    passphrase: Option<SecretString>,
    /// `NEWT_TOKEN_PASSPHRASE` is read at most once (memoized), so a mid-run
    /// env change cannot flip behavior.
    env_checked: bool,
    /// Memoized identity load: `None` = not loaded yet; `Some(None)` = loaded,
    /// absent.
    identity: Option<Option<TokenIdentity>>,
    /// Per-file decrypted plaintext — scrypt runs once per file per process.
    decrypted: HashMap<PathBuf, String>,
    /// Warn-once bookkeeping for locked/corrupt tokens.
    warned: HashSet<PathBuf>,
}

/// The process-wide unlock session. In-memory only; nothing here ever
/// persists.
pub struct SecretsSession {
    state: Mutex<SessionState>,
}

/// The global session (headless binaries need zero wiring — the first
/// `resolve_api_key` that meets a scrypt file consults the env itself).
pub fn session() -> &'static SecretsSession {
    static SESSION: OnceLock<SecretsSession> = OnceLock::new();
    SESSION.get_or_init(|| SecretsSession {
        state: Mutex::new(SessionState::default()),
    })
}

impl SecretsSession {
    fn lock(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Provide the session passphrase (TUI preflight / wizard).
    pub fn set_passphrase(&self, pass: SecretString) {
        self.lock().passphrase = Some(pass);
    }

    pub fn has_passphrase(&self) -> bool {
        self.provided_passphrase().is_some()
    }

    /// Forget a wrong passphrase so the retry loop can try again.
    pub fn clear_passphrase(&self) {
        let mut s = self.lock();
        s.passphrase = None;
        s.env_checked = false;
    }

    fn provided_passphrase(&self) -> Option<SecretString> {
        let mut s = self.lock();
        if s.passphrase.is_none() && !s.env_checked {
            s.env_checked = true;
            if let Ok(v) = std::env::var(PASSPHRASE_ENV) {
                if !v.trim().is_empty() {
                    s.passphrase = Some(SecretString::from(v));
                }
            }
        }
        s.passphrase.clone()
    }

    fn cached(&self, path: &Path) -> Option<String> {
        self.lock().decrypted.get(path).cloned()
    }

    fn cache(&self, path: &Path, token: String) {
        self.lock().decrypted.insert(path.to_path_buf(), token);
    }

    /// True the FIRST time `path` is reported; later calls return false so
    /// callers warn once per process.
    pub fn first_warning_for(&self, path: &Path) -> bool {
        self.lock().warned.insert(path.to_path_buf())
    }

    /// Test-only: wipe all session state (env memoization included).
    #[cfg(any(test, feature = "test-util"))]
    pub fn reset_for_test(&self) {
        *self.lock() = SessionState::default();
    }
}

impl UnlockProvider for SecretsSession {
    fn machine_identity(&self) -> Option<TokenIdentity> {
        {
            let s = self.lock();
            if let Some(memo) = &s.identity {
                return memo.clone();
            }
        }
        let loaded = load_identity().ok().flatten();
        self.lock().identity = Some(loaded.clone());
        loaded
    }

    fn passphrase(&self) -> Option<SecretString> {
        self.provided_passphrase()
    }
}

// ---------------------------------------------------------------------------
// IO shell
// ---------------------------------------------------------------------------

/// `~/.newt/secrets` (or `$NEWT_CONFIG_DIR/secrets`), created 0700 on demand.
pub fn secrets_dir() -> Result<PathBuf, SecretsError> {
    let dir = crate::Config::user_config_dir()
        .ok_or(SecretsError::NoHome)?
        .join("secrets");
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|source| SecretsError::Io {
            path: dir.clone(),
            source,
        })?;
        restrict_permissions(&dir, 0o700);
    }
    Ok(dir)
}

/// `~/.newt/secrets/identity.txt`.
pub fn identity_path() -> Result<PathBuf, SecretsError> {
    Ok(secrets_dir()?.join("identity.txt"))
}

/// Load the machine identity if one exists (read path — never generates).
pub fn load_identity() -> Result<Option<TokenIdentity>, SecretsError> {
    let path = identity_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(SecretsError::Io { path, source }),
    };
    TokenIdentity::from_file_str(&text)
        .map(Some)
        .map_err(|detail| SecretsError::Corrupt { path, detail })
}

/// Load the machine identity, generating (0600) on first use (write path).
pub fn load_or_generate_identity() -> Result<TokenIdentity, SecretsError> {
    if let Some(ident) = load_identity()? {
        return Ok(ident);
    }
    let ident = TokenIdentity::generate();
    let path = identity_path()?;
    let body = ident.to_file_string(
        &chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S%z")
            .to_string(),
    );
    std::fs::write(&path, body.expose_secret()).map_err(|source| SecretsError::Io {
        path: path.clone(),
        source,
    })?;
    restrict_permissions(&path, 0o600);
    Ok(ident)
}

fn restrict_permissions(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

/// Encrypt `token` and write `<backends_dir>/<name>.token.age` (0600).
/// `passphrase` `None`/blank → machine identity (generated on first use).
/// Seeds the session cache with the value just written — the post-wizard
/// probe and first session resolve without a re-prompt.
pub fn store_token(
    backends_dir: &Path,
    name: &str,
    token: &str,
    passphrase: Option<&SecretString>,
) -> Result<PathBuf, SecretsError> {
    let passphrase = passphrase.filter(|p| !p.expose_secret().trim().is_empty());
    std::fs::create_dir_all(backends_dir).map_err(|source| SecretsError::Io {
        path: backends_dir.to_path_buf(),
        source,
    })?;
    let path = backends_dir.join(format!("{name}.token.age"));
    let armored = match passphrase {
        Some(pass) => encrypt_with_passphrase(pass, token.as_bytes()).map_err(|detail| {
            SecretsError::Corrupt {
                path: path.clone(),
                detail,
            }
        })?,
        None => {
            let ident = load_or_generate_identity()?;
            encrypt_to_identity(&ident, token.as_bytes()).map_err(|detail| {
                SecretsError::Corrupt {
                    path: path.clone(),
                    detail,
                }
            })?
        }
    };
    std::fs::write(&path, &armored).map_err(|source| SecretsError::Io {
        path: path.clone(),
        source,
    })?;
    restrict_permissions(&path, 0o600);
    session().cache(&path, token.to_string());
    if let Some(pass) = passphrase {
        session().set_passphrase(pass.clone());
    }
    Ok(path)
}

/// Read + resolve one `api_key_file` through the session. Missing file →
/// `Ok(None)` (the legacy behavior every consumer relies on).
pub fn resolve_token_file(path: &Path) -> Result<Option<String>, SecretsError> {
    if let Some(token) = session().cached(path) {
        return Ok(Some(token));
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SecretsError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let token = token_from_file_bytes(&bytes, path, session())?;
    if let Some(t) = &token {
        if classify(&bytes) != TokenFileKind::Plaintext {
            session().cache(path, t.clone());
        }
    }
    Ok(token)
}

/// TUI preflight: does `path` need a passphrase that is not yet available
/// (cache, session, or env)? Never prompts, never hangs.
pub fn needs_passphrase(path: &Path) -> bool {
    if session().cached(path).is_some() || session().has_passphrase() {
        return false;
    }
    match std::fs::read(path) {
        Ok(bytes) => classify(&bytes) == TokenFileKind::AgeScrypt,
        Err(_) => false,
    }
}

/// One prompt-retry step: attempt to decrypt `path` with `pass`. Success
/// caches passphrase + plaintext; failure caches NOTHING (so the next
/// attempt starts clean).
pub fn try_unlock(path: &Path, pass: SecretString) -> Result<(), SecretsError> {
    let bytes = std::fs::read(path).map_err(|source| SecretsError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let unlock = StaticUnlock {
        identity: None,
        passphrase: Some(pass.clone()),
    };
    let token = token_from_file_bytes(&bytes, path, &unlock)?;
    if let Some(token) = token {
        session().set_passphrase(pass);
        session().cache(path, token);
    }
    Ok(())
}

/// Doctor's per-backend credential verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenStatus {
    /// No credential configured at all.
    Unset,
    /// `api_key_env` names a set, non-empty variable.
    FromEnv { var: String },
    /// Legacy plaintext file — works, nudge to encrypt.
    PlaintextFile { path: PathBuf },
    /// Encrypted and decryptable right now.
    EncryptedUnlocked { path: PathBuf },
    /// Encrypted and NOT currently decryptable (reason inside).
    EncryptedLocked { path: PathBuf, reason: String },
    /// `api_key_file` points at nothing.
    MissingFile { path: PathBuf },
}

/// Classify a backend's credential configuration for `newt doctor`.
pub fn token_status(api_key_env: Option<&str>, api_key_file: Option<&str>) -> TokenStatus {
    if let Some(var) = api_key_env {
        if std::env::var(var).is_ok_and(|v| !v.trim().is_empty()) {
            return TokenStatus::FromEnv {
                var: var.to_string(),
            };
        }
    }
    let Some(file) = api_key_file else {
        return TokenStatus::Unset;
    };
    let path = crate::config::expand_tilde(file);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return TokenStatus::MissingFile { path },
    };
    match classify(&bytes) {
        TokenFileKind::Plaintext => TokenStatus::PlaintextFile { path },
        _ => match token_from_file_bytes(&bytes, &path, session()) {
            Ok(Some(_)) => TokenStatus::EncryptedUnlocked { path },
            Ok(None) => TokenStatus::EncryptedLocked {
                path,
                reason: "decrypted payload holds no token".to_string(),
            },
            Err(e) => TokenStatus::EncryptedLocked {
                path,
                reason: e.to_string(),
            },
        },
    }
}

/// Warn once per path about a locked/broken token — the lossy
/// `resolve_api_key` wrapper calls this so a locked token is never a silent
/// `None`.
pub fn warn_once(path_hint: &str, err: &SecretsError) {
    let key = Path::new(path_hint);
    if session().first_warning_for(key) {
        tracing::warn!(token = %path_hint, error = %err, "API token unavailable");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(s: &str) -> SecretString {
        SecretString::from(s.to_string())
    }

    // --- classify ---

    #[test]
    fn classify_distinguishes_plaintext_and_both_age_modes() {
        assert_eq!(classify(b"sk-plain-token\n"), TokenFileKind::Plaintext);
        assert_eq!(classify(b""), TokenFileKind::Plaintext);

        let ident = TokenIdentity::generate();
        let x = encrypt_to_identity(&ident, b"tok").unwrap();
        assert!(x.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
        assert_eq!(classify(x.as_bytes()), TokenFileKind::AgeX25519);

        let s = encrypt_with_passphrase(&pass("pw"), b"tok").unwrap();
        assert_eq!(classify(s.as_bytes()), TokenFileKind::AgeScrypt);

        // Binary magic with a garbage header: age, but unknown stanza.
        let garbage = b"age-encryption.org/v1\n-> mystery\n";
        assert_eq!(classify(garbage), TokenFileKind::AgeUnknown);
    }

    // --- roundtrips ---

    #[test]
    fn x25519_roundtrip_and_wrong_identity() {
        let ident = TokenIdentity::generate();
        let armored = encrypt_to_identity(&ident, b"sk-secret").unwrap();
        let back = decrypt_with_identity(&ident, armored.as_bytes()).unwrap();
        assert_eq!(back, b"sk-secret");

        let other = TokenIdentity::generate();
        assert!(
            decrypt_with_identity(&other, armored.as_bytes()).is_err(),
            "a different identity must not decrypt"
        );
    }

    #[test]
    fn scrypt_roundtrip_and_wrong_passphrase() {
        let armored = encrypt_with_passphrase(&pass("correct horse"), b"sk-secret").unwrap();
        let back = decrypt_with_passphrase(&pass("correct horse"), armored.as_bytes()).unwrap();
        assert_eq!(back, b"sk-secret");
        assert!(
            decrypt_with_passphrase(&pass("wrong"), armored.as_bytes()).is_err(),
            "a wrong passphrase must not decrypt"
        );
    }

    #[test]
    fn corrupted_body_is_an_error_not_a_panic() {
        let ident = TokenIdentity::generate();
        let mut armored = encrypt_to_identity(&ident, b"sk-secret").unwrap();
        // Flip a payload character (past the armor header line).
        let idx = armored.len() / 2;
        let flipped = if armored.as_bytes()[idx] == b'A' {
            "B"
        } else {
            "A"
        };
        armored.replace_range(idx..=idx, flipped);
        assert!(decrypt_with_identity(&ident, armored.as_bytes()).is_err());
    }

    // --- identity file format ---

    #[test]
    fn identity_file_roundtrips_and_skips_comments() {
        let ident = TokenIdentity::generate();
        let body = ident.to_file_string("2026-08-06");
        let text = body.expose_secret();
        assert!(text.contains("# created: 2026-08-06"));
        assert!(text.contains(&format!("# public key: {}", ident.public())));
        let parsed = TokenIdentity::from_file_str(text).unwrap();
        assert_eq!(parsed.public(), ident.public());
        assert!(TokenIdentity::from_file_str("# only comments\n").is_err());
        assert!(TokenIdentity::from_file_str("garbage\n").is_err());
    }

    // --- token_from_file_bytes (the choke point) with a fake provider ---

    #[test]
    fn choke_point_plaintext_first_nonempty_line_rule() {
        let unlock = StaticUnlock {
            identity: None,
            passphrase: None,
        };
        let p = Path::new("t.token");
        assert_eq!(
            token_from_file_bytes(b"\n  \n sk-tok \nrest\n", p, &unlock).unwrap(),
            Some("sk-tok".to_string())
        );
        assert_eq!(token_from_file_bytes(b"\n \n", p, &unlock).unwrap(), None);
    }

    #[test]
    fn choke_point_x25519_decrypts_or_names_missing_identity() {
        let ident = TokenIdentity::generate();
        let armored = encrypt_to_identity(&ident, b"sk-x\n").unwrap();
        let p = Path::new("t.token.age");

        let with = StaticUnlock {
            identity: Some(ident),
            passphrase: None,
        };
        assert_eq!(
            token_from_file_bytes(armored.as_bytes(), p, &with).unwrap(),
            Some("sk-x".to_string())
        );

        let without = StaticUnlock {
            identity: None,
            passphrase: None,
        };
        assert!(matches!(
            token_from_file_bytes(armored.as_bytes(), p, &without),
            Err(SecretsError::IdentityMissing { .. })
        ));
    }

    #[test]
    fn choke_point_scrypt_requires_and_verifies_the_passphrase() {
        let armored = encrypt_with_passphrase(&pass("pw"), b"sk-s\n").unwrap();
        let p = Path::new("t.token.age");

        let none = StaticUnlock {
            identity: None,
            passphrase: None,
        };
        assert!(matches!(
            token_from_file_bytes(armored.as_bytes(), p, &none),
            Err(SecretsError::PassphraseRequired { .. })
        ));

        let wrong = StaticUnlock {
            identity: None,
            passphrase: Some(pass("nope")),
        };
        assert!(matches!(
            token_from_file_bytes(armored.as_bytes(), p, &wrong),
            Err(SecretsError::WrongPassphrase { .. })
        ));

        let right = StaticUnlock {
            identity: None,
            passphrase: Some(pass("pw")),
        };
        assert_eq!(
            token_from_file_bytes(armored.as_bytes(), p, &right).unwrap(),
            Some("sk-s".to_string())
        );
    }

    #[test]
    fn choke_point_multiline_decrypted_payload_obeys_first_line_rule() {
        let ident = TokenIdentity::generate();
        let armored = encrypt_to_identity(&ident, b"\n sk-first \n sk-second \n").unwrap();
        let unlock = StaticUnlock {
            identity: Some(ident),
            passphrase: None,
        };
        assert_eq!(
            token_from_file_bytes(armored.as_bytes(), Path::new("t"), &unlock).unwrap(),
            Some("sk-first".to_string())
        );
    }

    // --- session semantics (process-global: serialized) ---

    #[serial_test::serial(secrets_session)]
    #[test]
    fn session_passphrase_set_clear_and_env_memoization() {
        session().reset_for_test();
        std::env::remove_var(PASSPHRASE_ENV);
        assert!(!session().has_passphrase());

        session().set_passphrase(pass("live"));
        assert!(session().has_passphrase());
        session().clear_passphrase();
        assert!(!session().has_passphrase());

        // Env fallback is honored…
        session().reset_for_test();
        std::env::set_var(PASSPHRASE_ENV, "from-env");
        assert!(session().has_passphrase());
        // …and memoized: a mid-run env change cannot flip behavior.
        std::env::remove_var(PASSPHRASE_ENV);
        assert!(
            session().has_passphrase(),
            "memoized once — mid-run env removal does not flip behavior"
        );
        session().reset_for_test();
    }

    #[serial_test::serial(secrets_session)]
    #[test]
    fn warn_once_reports_each_path_exactly_once() {
        session().reset_for_test();
        let p = Path::new("/tmp/x.token.age");
        assert!(session().first_warning_for(p));
        assert!(!session().first_warning_for(p));
        session().reset_for_test();
    }

    // --- real-fs tier (grounds the fake-provider tests above: proves the
    // mocked UnlockProvider behavior matches what the on-disk shell does) ---

    fn pin_config_dir(dir: &Path) -> Option<std::ffi::OsString> {
        let prev = std::env::var_os(crate::config::NEWT_CONFIG_DIR_ENV);
        std::env::set_var(crate::config::NEWT_CONFIG_DIR_ENV, dir);
        prev
    }

    fn restore_config_dir(prev: Option<std::ffi::OsString>) {
        match prev {
            Some(v) => std::env::set_var(crate::config::NEWT_CONFIG_DIR_ENV, v),
            None => std::env::remove_var(crate::config::NEWT_CONFIG_DIR_ENV),
        }
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn store_token_blank_passphrase_creates_identity_and_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let prev = pin_config_dir(dir.path());
        session().reset_for_test();

        let backends = dir.path().join("backends");
        let written = store_token(&backends, "anthropic", "sk-ant-test", None).unwrap();
        assert_eq!(written, backends.join("anthropic.token.age"));

        // Ciphertext on disk, identity created, and the stock-age format.
        let body = std::fs::read_to_string(&written).unwrap();
        assert!(body.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
        assert!(!body.contains("sk-ant-test"));
        let ident_file = dir.path().join("secrets").join("identity.txt");
        assert!(ident_file.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&ident_file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "identity file is private");
            let dmode = std::fs::metadata(dir.path().join("secrets"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dmode & 0o777, 0o700, "secrets dir is private");
        }

        // A FRESH session (no seeded cache) still resolves transparently.
        session().reset_for_test();
        assert_eq!(
            resolve_token_file(&written).unwrap(),
            Some("sk-ant-test".to_string())
        );

        // Second store reuses the same identity.
        let before = std::fs::read_to_string(&ident_file).unwrap();
        store_token(&backends, "openai", "sk-two", None).unwrap();
        assert_eq!(std::fs::read_to_string(&ident_file).unwrap(), before);

        session().reset_for_test();
        restore_config_dir(prev);
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn store_token_with_passphrase_locks_until_unlocked() {
        let dir = tempfile::tempdir().unwrap();
        let prev = pin_config_dir(dir.path());
        session().reset_for_test();
        std::env::remove_var(PASSPHRASE_ENV);

        let backends = dir.path().join("backends");
        let written =
            store_token(&backends, "gated", "sk-locked", Some(&pass("open sesame"))).unwrap();
        // The writing session is seeded (post-wizard probe works)…
        assert_eq!(
            resolve_token_file(&written).unwrap(),
            Some("sk-locked".to_string())
        );

        // …but a fresh session is locked until try_unlock.
        session().reset_for_test();
        assert!(needs_passphrase(&written));
        assert!(matches!(
            resolve_token_file(&written),
            Err(SecretsError::PassphraseRequired { .. })
        ));
        assert!(try_unlock(&written, pass("wrong")).is_err());
        assert!(!session().has_passphrase(), "failure caches nothing");
        try_unlock(&written, pass("open sesame")).unwrap();
        assert_eq!(
            resolve_token_file(&written).unwrap(),
            Some("sk-locked".to_string())
        );

        session().reset_for_test();
        restore_config_dir(prev);
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn legacy_plaintext_token_files_keep_working() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("legacy.token");
        std::fs::write(&plain, "sk-legacy\n").unwrap();
        assert_eq!(
            resolve_token_file(&plain).unwrap(),
            Some("sk-legacy".to_string())
        );
        // Missing file stays the legacy None.
        assert_eq!(
            resolve_token_file(&dir.path().join("absent.token")).unwrap(),
            None
        );
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn token_status_classifies_every_shape() {
        let dir = tempfile::tempdir().unwrap();
        let prev = pin_config_dir(dir.path());
        session().reset_for_test();
        std::env::remove_var(PASSPHRASE_ENV);

        assert_eq!(token_status(None, None), TokenStatus::Unset);

        std::env::set_var("NEWT_TEST_TOKEN_VAR", "sk-env");
        assert_eq!(
            token_status(Some("NEWT_TEST_TOKEN_VAR"), None),
            TokenStatus::FromEnv {
                var: "NEWT_TEST_TOKEN_VAR".to_string()
            }
        );
        std::env::remove_var("NEWT_TEST_TOKEN_VAR");

        let plain = dir.path().join("p.token");
        std::fs::write(&plain, "sk\n").unwrap();
        assert!(matches!(
            token_status(None, plain.to_str()),
            TokenStatus::PlaintextFile { .. }
        ));

        let backends = dir.path().join("backends");
        let unlocked = store_token(&backends, "open", "sk-open", None).unwrap();
        assert!(matches!(
            token_status(None, unlocked.to_str()),
            TokenStatus::EncryptedUnlocked { .. }
        ));

        let locked = store_token(&backends, "shut", "sk-shut", Some(&pass("pw"))).unwrap();
        session().reset_for_test(); // drop the seeded cache
        assert!(matches!(
            token_status(None, locked.to_str()),
            TokenStatus::EncryptedLocked { .. }
        ));

        assert!(matches!(
            token_status(None, Some("/nope/absent.token")),
            TokenStatus::MissingFile { .. }
        ));

        session().reset_for_test();
        restore_config_dir(prev);
    }
}
