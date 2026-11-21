//! Secret redaction: replace credential-looking strings with `[REDACTED]`.
//!
//! Moved out of `agentic::compress` (#3140) so the policy layer can use it
//! without the compactor.

use std::sync::OnceLock;

/// `(pattern, replacement)` table for [`redact_secrets`]. Deliberately small
/// and high-precision: each row matches a *credential value shape*, not
/// prose about credentials — "the api key is in the keychain" must pass.
const REDACTION_TABLE: &[(&str, &str)] = &[
    // Private key blocks (redact even when the END line was truncated away).
    (
        r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
        "[REDACTED]",
    ),
    // OpenAI-style secret keys.
    (r"\bsk-[A-Za-z0-9_-]{20,}", "[REDACTED]"),
    // GitHub tokens (classic + fine-grained).
    (r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}", "[REDACTED]"),
    (r"\bgithub_pat_[A-Za-z0-9_]{20,}", "[REDACTED]"),
    // AWS access key ids.
    (r"\bAKIA[0-9A-Z]{16}\b", "[REDACTED]"),
    // JWTs (`eyJ` = base64 of `{"`).
    (
        r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}",
        "[REDACTED]",
    ),
    // HTTP bearer credentials. The value class has no spaces, so prose like
    // "bearer of good news" never reaches the 20-char floor.
    (
        r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{20,}",
        "Bearer [REDACTED]",
    ),
    // Generic credential assignment: a secret-ish key, `=`/`:`, and a
    // value of 8+ non-space chars. The key list is closed (no bare
    // "token"/"key") so token-budget talk passes. The optional quote after
    // the key matches the JSON-quoted shape (`"api_key": "…"`) — the native
    // form tool-call args take in this pipeline's summarizer input (F6).
    (
        r#"(?i)\b(api[_-]?key|secret[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|password|passwd)\b["']?\s*[:=]\s*["']?[^\s"']{8,}["']?"#,
        "${1}=[REDACTED]",
    ),
];

fn redaction_patterns() -> &'static Vec<(regex::Regex, &'static str)> {
    static PATTERNS: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        REDACTION_TABLE
            .iter()
            .map(|(pat, rep)| {
                (
                    regex::Regex::new(pat).expect("redaction pattern must compile"),
                    *rep,
                )
            })
            .collect()
    })
}

/// Replace credential-looking strings with `[REDACTED]`. Applied to ALL
/// summarizer input — the summarizer LLM may ignore prompt instructions and
/// echo secrets back verbatim, and summaries persist for the conversation.
pub(crate) fn redact_secrets(input: &str) -> String {
    let mut out = input.to_string();
    for (re, rep) in redaction_patterns() {
        if re.is_match(&out) {
            out = re.replace_all(&out, *rep).into_owned();
        }
    }
    // Then the by-VALUE session disclosure filter (registered secrets, every
    // encoding + chunk-split), so the observation / compaction / spill memory
    // paths that funnel through here cannot carry a KNOWN secret into model
    // context — not just the shape-matched patterns above. Identity when no
    // session filter is installed (the guard is set per driven turn).
    crate::ocap::redact_session_ingress(&out)
}
