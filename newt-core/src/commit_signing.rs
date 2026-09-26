//! Commit signing by the harness (`[agent-identity.git] signing`).
//!
//! Signing happens in the harness's own commit path (the embedded git tool),
//! never inside the sandbox: a key the model's shell could read is a key the
//! model could sign anything with. The operator chooses the key:
//!
//! - [`SigningMode::Harness`](crate::agent_identity::SigningMode): a
//!   harness key (`signing_key` in `agent-identity.toml`, an Ed25519 PKCS#8
//!   PEM), signed in-process as an SSH signature (OpenSSH `SSHSIG`, namespace
//!   `git`), the format `git verify-commit` and GitHub accept.
//! - [`SigningMode::Operator`](crate::agent_identity::SigningMode): the
//!   operator's own git signing setup (`user.signingkey`, `gpg.format`,
//!   `gpg.ssh.program` / `gpg.program`), run as git would run it, so a key
//!   held by ssh-agent, 1Password or gpg-agent keeps working.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use sha2::Digest as _;

use crate::agent_identity::{AgentIdentity, SigningMode};

/// Signs the bytes of an unsigned commit object.
pub trait CommitSigner: Send + Sync {
    /// An ASCII-armored detached signature over `payload`.
    ///
    /// # Errors
    /// When the key cannot be used; the reason is written for the operator.
    fn sign(&self, payload: &[u8]) -> Result<String, String>;
}

/// `commit` with `armored` added as its `gpgsig` header, where git puts it:
/// the last header, continuation lines indented by one space.
#[must_use]
pub fn with_signature(commit: &[u8], armored: &str) -> Vec<u8> {
    let split = commit
        .windows(2)
        .position(|w| w == b"\n\n")
        .map_or(commit.len(), |at| at + 1);
    let header = format!("gpgsig {}\n", armored.trim_end().replace('\n', "\n "));
    let mut out = Vec::with_capacity(commit.len() + header.len());
    out.extend_from_slice(&commit[..split]);
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&commit[split..]);
    out
}

/// The signer the identity's git profile asks for, or `None` when signing is
/// off.
///
/// # Errors
/// When the profile asks for signing that cannot be set up; commits must not
/// silently go out unsigned when the operator asked for signatures.
pub fn signer_for(identity: &AgentIdentity) -> Result<Option<Arc<dyn CommitSigner>>, String> {
    match identity.git.signing {
        SigningMode::Off => Ok(None),
        SigningMode::Harness => {
            let path = identity.signing_key_path().ok_or(
                "harness signing needs `signing_key` in agent-identity.toml \
                 (/settings git-signing harness creates one)",
            )?;
            Ok(Some(Arc::new(HarnessSshSigner::load(&path)?)))
        }
        SigningMode::Operator => {
            let listing = crate::git_hardening::ambient_git_config_listing()
                .map_err(|e| format!("could not read your git config: {e}"))?;
            Ok(Some(Arc::new(OperatorSigner::from_listing(&listing)?)))
        }
    }
}

/// The signer a session uses: [`signer_for`], except that a signing setup the
/// operator asked for but that cannot be built becomes a signer refusing every
/// commit with the reason, rather than commits silently going out unsigned.
#[must_use]
pub fn session_signer(identity: &AgentIdentity) -> Option<Arc<dyn CommitSigner>> {
    match signer_for(identity) {
        Ok(signer) => signer,
        Err(reason) => Some(Arc::new(Refusing(reason))),
    }
}

/// Refuses every commit, naming why signing is unavailable.
struct Refusing(String);

impl CommitSigner for Refusing {
    fn sign(&self, _payload: &[u8]) -> Result<String, String> {
        Err(self.0.clone())
    }
}

/// Create a fresh harness signing key at `path` (0600, never overwriting) and
/// return its public key as an OpenSSH line, for GitHub's signing keys and an
/// `allowed_signers` file.
///
/// # Errors
/// When the key file exists already or cannot be written.
pub fn generate_harness_key(path: &Path) -> Result<String, String> {
    let key = agent_mesh_protocol::UserKey::generate();
    key.save(path).map_err(|e| e.to_string())?;
    Ok(openssh_public(&key.public().as_bytes()))
}

/// A harness key signing in-process with an SSH signature.
pub struct HarnessSshSigner {
    key: agent_mesh_protocol::UserKey,
}

impl HarnessSshSigner {
    /// Load the Ed25519 PKCS#8 PEM at `path`.
    ///
    /// # Errors
    /// When the file is missing or not an Ed25519 key.
    pub fn load(path: &Path) -> Result<Self, String> {
        agent_mesh_protocol::UserKey::load(path)
            .map(|key| Self { key })
            .map_err(|e| format!("signing key {}: {e}", path.display()))
    }

    /// The public key as an OpenSSH line (`ssh-ed25519 AAAA…`).
    #[must_use]
    pub fn public_openssh(&self) -> String {
        openssh_public(&self.key.public().as_bytes())
    }
}

impl CommitSigner for HarnessSshSigner {
    fn sign(&self, payload: &[u8]) -> Result<String, String> {
        const NAMESPACE: &str = "git";
        const HASH: &str = "sha512";
        let public = ssh_ed25519_public_blob(&self.key.public().as_bytes());
        let mut signed = b"SSHSIG".to_vec();
        ssh_string(&mut signed, NAMESPACE.as_bytes());
        ssh_string(&mut signed, b"");
        ssh_string(&mut signed, HASH.as_bytes());
        ssh_string(&mut signed, &sha2::Sha512::digest(payload));
        let signature = self.key.sign(&signed).to_bytes();

        let mut sig_blob = Vec::new();
        ssh_string(&mut sig_blob, b"ssh-ed25519");
        ssh_string(&mut sig_blob, &signature);
        let mut blob = b"SSHSIG".to_vec();
        blob.extend_from_slice(&1u32.to_be_bytes());
        ssh_string(&mut blob, &public);
        ssh_string(&mut blob, NAMESPACE.as_bytes());
        ssh_string(&mut blob, b"");
        ssh_string(&mut blob, HASH.as_bytes());
        ssh_string(&mut blob, &sig_blob);

        let encoded = base64::engine::general_purpose::STANDARD.encode(blob);
        let mut armored = String::from("-----BEGIN SSH SIGNATURE-----\n");
        for line in encoded.as_bytes().chunks(70) {
            armored.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
            armored.push('\n');
        }
        armored.push_str("-----END SSH SIGNATURE-----\n");
        Ok(armored)
    }
}

/// An SSH wire-format string: a big-endian `u32` length, then the bytes.
fn ssh_string(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("SSH fields are far below 4 GiB");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn ssh_ed25519_public_blob(public: &[u8; 32]) -> Vec<u8> {
    let mut blob = Vec::new();
    ssh_string(&mut blob, b"ssh-ed25519");
    ssh_string(&mut blob, public);
    blob
}

fn openssh_public(public: &[u8; 32]) -> String {
    format!(
        "ssh-ed25519 {}",
        base64::engine::general_purpose::STANDARD.encode(ssh_ed25519_public_blob(public))
    )
}

/// The operator's git signing format (`gpg.format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Ssh,
    OpenPgp,
}

/// The operator's own git signing setup, run as git would run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorSigner {
    format: Format,
    key: String,
    program: String,
}

/// What a signing program may see: enough to find the operator's agent and
/// terminal, and nothing of newt's own environment.
const SIGNER_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SSH_AUTH_SOCK",
    "GNUPGHOME",
    "GPG_TTY",
    "TERM",
    "DISPLAY",
];

impl OperatorSigner {
    /// Read the signing setup out of a `git config --list` listing.
    ///
    /// # Errors
    /// When no `user.signingkey` is set, or `gpg.format` is one git signs
    /// with but this does not (`x509`).
    pub fn from_listing(listing: &str) -> Result<Self, String> {
        let get = |key: &str| {
            listing
                .lines()
                .filter_map(|line| line.split_once('='))
                .filter(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.to_owned())
                .next_back()
        };
        let key = get("user.signingkey").ok_or(
            "operator signing needs `user.signingkey` in your git config \
             (or choose /settings git-signing harness)",
        )?;
        let format = match get("gpg.format").as_deref() {
            None | Some("openpgp") => Format::OpenPgp,
            Some("ssh") => Format::Ssh,
            Some(other) => {
                return Err(format!("gpg.format = {other} is not supported for signing"))
            }
        };
        let program = match format {
            Format::Ssh => get("gpg.ssh.program").unwrap_or_else(|| "ssh-keygen".to_owned()),
            Format::OpenPgp => get("gpg.program").unwrap_or_else(|| "gpg".to_owned()),
        };
        Ok(Self {
            format,
            key,
            program,
        })
    }
}

impl CommitSigner for OperatorSigner {
    fn sign(&self, payload: &[u8]) -> Result<String, String> {
        let literal_key = self
            .format
            .eq(&Format::Ssh)
            .then(|| self.key.strip_prefix("key::"))
            .flatten();
        let key_file = literal_key
            .map(|public| KeyFile::write(public.as_bytes()))
            .transpose()?;
        let mut args: Vec<String> = match self.format {
            Format::Ssh => vec!["-Y".into(), "sign".into(), "-n".into(), "git".into()],
            Format::OpenPgp => vec!["--status-fd=2".into(), "-bsau".into(), self.key.clone()],
        };
        if self.format == Format::Ssh {
            match &key_file {
                // A literal public key names a key held by the agent.
                Some(file) => args.extend(["-U".into(), "-f".into(), path_arg(&file.0)]),
                None => args.extend([
                    "-f".into(),
                    crate::config::expand_tilde(&self.key)
                        .to_string_lossy()
                        .into_owned(),
                ]),
            }
        }
        // The operator's own signing program, configured by the operator, run
        // with a fixed argv outside the sandbox: `trusted-infra` in
        // docs/security/spawn-inventory.toml.
        let mut command = std::process::Command::new(&self.program);
        command
            .args(&args)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for name in SIGNER_ENV {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("could not run {}: {e}", self.program))?;
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(payload)
            .map_err(|e| e.to_string())?;
        let output = child.wait_with_output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "{} could not sign: {}",
                self.program,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        String::from_utf8(output.stdout).map_err(|e| e.to_string())
    }
}

/// A literal agent-held public key written where `ssh-keygen -f` can name it,
/// removed on drop.
struct KeyFile(std::path::PathBuf);

impl KeyFile {
    fn write(bytes: &[u8]) -> Result<Self, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "newt-signing-key-{}-{nanos}.pub",
            std::process::id()
        ));
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        Ok(Self(path))
    }
}

impl Drop for KeyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &[u8] =
        b"tree abc\nauthor A <a@b> 1 +0000\ncommitter A <a@b> 1 +0000\n\nsubject\n\nbody\n";

    #[test]
    fn the_signature_is_the_last_header_with_continuation_lines() {
        let signed = with_signature(COMMIT, "-----BEGIN X-----\nabc\n-----END X-----\n");
        let text = String::from_utf8(signed).unwrap();
        assert_eq!(
            text,
            "tree abc\nauthor A <a@b> 1 +0000\ncommitter A <a@b> 1 +0000\n\
             gpgsig -----BEGIN X-----\n abc\n -----END X-----\n\nsubject\n\nbody\n"
        );
    }

    #[test]
    fn a_harness_signature_is_armored_sshsig_over_the_git_preimage() {
        let signer = HarnessSshSigner {
            key: agent_mesh_protocol::UserKey::generate(),
        };
        let armored = signer.sign(COMMIT).unwrap();
        assert!(armored.starts_with("-----BEGIN SSH SIGNATURE-----\n"));
        assert!(armored.ends_with("-----END SSH SIGNATURE-----\n"));
        let body: String = armored
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        let blob = base64::engine::general_purpose::STANDARD
            .decode(body)
            .unwrap();
        assert_eq!(&blob[..6], b"SSHSIG");
        // The trailing 64 bytes are the Ed25519 signature over the SSHSIG
        // preimage. Ed25519 is deterministic, so re-signing the expected
        // preimage must reproduce them exactly.
        let mut preimage = b"SSHSIG".to_vec();
        ssh_string(&mut preimage, b"git");
        ssh_string(&mut preimage, b"");
        ssh_string(&mut preimage, b"sha512");
        ssh_string(&mut preimage, &sha2::Sha512::digest(COMMIT));
        assert_eq!(
            blob[blob.len() - 64..],
            signer.key.sign(&preimage).to_bytes()
        );
        assert!(signer
            .public_openssh()
            .starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5"));
    }

    #[test]
    fn the_operator_setup_follows_git_config() {
        let ssh = OperatorSigner::from_listing(
            "user.name=Op\ngpg.format=ssh\nuser.signingkey=~/.ssh/id_ed25519.pub\n",
        )
        .unwrap();
        assert_eq!(ssh.format, Format::Ssh);
        assert_eq!(ssh.program, "ssh-keygen");
        let gpg =
            OperatorSigner::from_listing("user.signingkey=ABCD1234\ngpg.program=gpg2\n").unwrap();
        assert_eq!(gpg.format, Format::OpenPgp);
        assert_eq!(gpg.program, "gpg2");
        assert!(OperatorSigner::from_listing("user.name=Op\n").is_err());
        assert!(OperatorSigner::from_listing("user.signingkey=k\ngpg.format=x509\n").is_err());
    }

    #[test]
    fn signing_off_needs_no_key() {
        assert!(signer_for(&AgentIdentity::default()).unwrap().is_none());
        assert!(session_signer(&AgentIdentity::default()).is_none());
    }

    #[test]
    fn signing_asked_for_but_unavailable_refuses_rather_than_skipping() {
        let mut identity = AgentIdentity::default();
        identity.git.signing = SigningMode::Harness; // and no `signing_key`
        let signer = session_signer(&identity).expect("a refusing signer, not None");
        let refusal = signer.sign(COMMIT).unwrap_err();
        assert!(refusal.contains("signing_key"), "{refusal}");
    }
}
