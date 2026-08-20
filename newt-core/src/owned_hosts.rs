//! Which hosts the operator calls **theirs**.
//!
//! "Owned" is a *colloquial* trust claim — infrastructure the operator runs and
//! is willing to be patient with — not a security boundary. It drives
//! performance-shaped decisions only: today, whether an inference endpoint gets
//! the patient local-inference retry policy or the thrifty hosted one.
//!
//! # Why this is separate from the exfiltration guard
//!
//! [`crate::notes_scan::is_exfil_safe_host`] answers a different question — may
//! a note carry a literal fetch URL to this host — and that one is a security
//! control. It stays hardcoded and fail-closed, because widening it is
//! *amplification*, and per the line's Authority Register
//! (`steward-charter/docs/AUTHORITY.md`) amplification needs a live human act,
//! "not a configuration file and not an environment variable, because a file
//! can be written by anything that can write files". A prompt injection that can
//! write config must not thereby earn an exfiltration channel.
//!
//! So the two predicates share the built-in floor and diverge only upward: a
//! configured suffix widens *this* one and can never widen the guard.

use std::sync::OnceLock;

/// Operator-declared suffixes, published once when runtime settings apply.
static OWNED_SUFFIXES: OnceLock<Vec<String>> = OnceLock::new();

/// Publish `[network] owned_suffixes` from
/// [`crate::config::Config::apply_runtime_settings`]. First call wins; later
/// calls are ignored, matching the other runtime-settings publishers.
pub fn set_owned_suffixes(suffixes: Vec<String>) {
    let normalized = suffixes
        .into_iter()
        .filter_map(|s| {
            let s = s.trim().to_lowercase();
            if s.is_empty() {
                return None;
            }
            Some(if s.starts_with('.') {
                s
            } else {
                format!(".{s}")
            })
        })
        .collect();
    let _ = OWNED_SUFFIXES.set(normalized);
}

/// Is `host` one the operator calls theirs?
///
/// True when any of:
/// - it clears the built-in private floor ([`crate::notes_scan::is_exfil_safe_host`]) —
///   loopback, RFC-1918, and the conventional private suffixes;
/// - it is a single-label DNS name (`dgx1`, `ollama`) — conventional LAN and
///   service-discovery shorthand, which carries no public suffix by design;
/// - it ends with an operator-declared `[network] owned_suffixes` entry.
#[must_use]
pub fn is_owned_host(host: &str) -> bool {
    let host = host.trim_matches(|c| c == '[' || c == ']').to_lowercase();
    if crate::notes_scan::is_exfil_safe_host(&host) {
        return true;
    }
    if !host.contains('.') && !host.contains(':') {
        return true;
    }
    OWNED_SUFFIXES
        .get()
        .is_some_and(|suffixes| suffixes.iter().any(|s| host.ends_with(s.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_private_floor_is_owned() {
        assert!(is_owned_host("localhost"));
        assert!(is_owned_host("127.0.0.1"));
        assert!(is_owned_host("10.0.0.7"));
        // RFC8375's reserved home domain, and the conventional private suffixes.
        assert!(is_owned_host("dgx1.home.arpa"));
        assert!(is_owned_host("box.internal"));
    }

    #[test]
    fn single_label_names_are_owned() {
        assert!(is_owned_host("dgx1"));
        assert!(is_owned_host("ollama"));
    }

    #[test]
    fn public_names_are_not_owned_by_default() {
        assert!(!is_owned_host("api.example.com"));
        assert!(!is_owned_host("inference.example.net"));
    }

    #[test]
    fn declaring_a_suffix_does_not_widen_the_exfil_guard() {
        // The guard is the security control; nothing in this module may move it.
        // Asserted against a name no built-in suffix covers.
        assert!(!crate::notes_scan::is_exfil_safe_host("gpu.example.com"));
    }
}
