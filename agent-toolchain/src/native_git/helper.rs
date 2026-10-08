//! Native Git hook/signing helpers. They hold no signing key and grant no rights.
//! The embedding executable supplies an authenticated, invocation-bound transport.

use content_addressable::{canonical, ContentAddressable, ContentError};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;

use super::MAX_COMMIT_BYTES;

/// Host events emitted by a native Git helper. Candidate object IDs are foreign
/// Git identifiers; the host independently reads and verifies their contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum HookRequest {
    /// Git assembled a message; apply the harness's canonical attribution.
    Message { message: String },
    /// Git requests a signature over these exact unsigned commit bytes.
    Sign {
        #[serde(with = "serde_bytes")]
        commit: Vec<u8>,
    },
    /// Git's reference-transaction hook reports its phase and original input.
    References { phase: String, updates: String },
}

impl ContentAddressable for HookRequest {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

/// Explicit success or refusal from the host policy; no response is an approval
/// merely because a socket was readable or a helper could connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookResponse {
    Success(#[serde(with = "serde_bytes")] Vec<u8>),
    Refused(String),
}

impl ContentAddressable for HookResponse {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

/// Decode only the canonical representation supplied by the shared codec.
pub fn decode_request(bytes: &[u8]) -> Result<HookRequest, String> {
    if bytes.len() > MAX_COMMIT_BYTES + 4096 {
        return Err("native Git hook request exceeds the transport bound".into());
    }
    canonical::from_canonical_dagcbor_checked(bytes).map_err(|error| error.to_string())
}

/// Invocation-scoped authenticated exchange for canonical helper records.
/// The embedding transport retains authority and enforces its own frame bound.
pub type HookTransport<'a> = dyn FnMut(&[u8]) -> Result<Vec<u8>, String> + 'a;

fn request(value: HookRequest, transport: &mut HookTransport<'_>) -> Result<Vec<u8>, String> {
    let bytes = value.canonical_form().map_err(|error| error.to_string())?;
    let response = transport(&bytes)?;
    if response.len() > MAX_COMMIT_BYTES + 4096 {
        return Err("native Git hook response exceeds the transport bound".into());
    }
    match canonical::from_canonical_dagcbor_checked::<HookResponse>(&response)
        .map_err(|error| error.to_string())?
    {
        HookResponse::Success(bytes) => Ok(bytes),
        HookResponse::Refused(reason) => Err(reason),
    }
}

/// Fixed helper roles selected by the trusted executable entrypoint. A role or
/// filename is not authentication; the supplied transport must authenticate it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperRole {
    PrepareMessage,
    CommitMessage,
    ReferenceTransaction,
    Sign,
}

impl HelperRole {
    /// Names Git executes from the invocation-owned helper directory.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "prepare-commit-msg" => Some(Self::PrepareMessage),
            "commit-msg" => Some(Self::CommitMessage),
            "reference-transaction" => Some(Self::ReferenceTransaction),
            "agent-toolchain-git-sign" => Some(Self::Sign),
            _ => None,
        }
    }
}

fn bounded_read(input: impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    input
        .take(MAX_COMMIT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_COMMIT_BYTES {
        return Err("native Git helper input exceeds the commit size bound".into());
    }
    Ok(bytes)
}

/// Execute one helper with its original Git arguments and streams. Repository
/// hook chaining is supplied by the embedding caller before/after this policy
/// step; this function never parses Git's commit CLI or spawns a command.
pub fn run(
    role: HelperRole,
    args: &[OsString],
    input: &mut dyn Read,
    output: &mut dyn Write,
    diagnostics: &mut dyn Write,
    transport: &mut HookTransport<'_>,
) -> Result<(), String> {
    match role {
        HelperRole::PrepareMessage | HelperRole::CommitMessage => {
            let path = args
                .first()
                .ok_or("Git message hook omitted its message file")?;
            let bytes = bounded_read(
                std::fs::File::open(Path::new(path)).map_err(|error| error.to_string())?,
            )?;
            let message = String::from_utf8(bytes).map_err(|_| "Git message is not UTF-8")?;
            let finalized = request(HookRequest::Message { message }, transport)?;
            // This helper remains under native Git's inherited kernel fence.
            // Refusal occurs before truncation, preserving Git's original file.
            std::fs::write(Path::new(path), finalized).map_err(|error| error.to_string())?;
        }
        HelperRole::Sign => {
            if args.len() != 3 || args[0] != "--status-fd=2" || args[1] != "-bsau" {
                return Err("unsupported native Git signing helper invocation".into());
            }
            let commit = bounded_read(input)?;
            let signature = request(HookRequest::Sign { commit }, transport)?;
            output
                .write_all(&signature)
                .map_err(|error| error.to_string())?;
            // Git's gpg-compatible signing transport requires this completion
            // marker. The returned armor retains the actual SSH/OpenPGP format;
            // Git verifies that format from the resulting signature itself.
            diagnostics
                .write_all(b"[GNUPG:] SIG_CREATED \n")
                .map_err(|error| error.to_string())?;
        }
        HelperRole::ReferenceTransaction => {
            let phase = args
                .first()
                .and_then(|arg| arg.to_str())
                .ok_or("Git reference hook omitted its phase")?;
            if !matches!(phase, "preparing" | "prepared" | "committed" | "aborted") {
                return Err("unknown Git reference transaction phase".into());
            }
            let updates = String::from_utf8(bounded_read(input)?)
                .map_err(|_| "Git reference update is not UTF-8")?;
            request(
                HookRequest::References {
                    phase: phase.into(),
                    updates,
                },
                transport,
            )?;
        }
    }
    Ok(())
}
