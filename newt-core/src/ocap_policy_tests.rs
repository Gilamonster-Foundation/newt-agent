//! newt's own tests of `ocap_policy`: where the copy deliberately differs from
//! agent-bridle-core's `policy` module (#3160). They name no bridle crate, so they stay when
//! the parity tests are gated with the dependency.

use crate::ocap_policy::{hex_decode, Ed25519ApproveVerifier, PolicyFile};

/// Found in review of #3208: a `sig` that is not ASCII was sliced mid-character, which is a
/// panic, not a refusal. `verified_approves` runs on every approve entry at startup, so one
/// bad character in `approve.toml` took the session down instead of dropping the entry.
#[test]
fn a_signature_that_is_not_ascii_is_refused_not_a_panic() {
    // Four bytes, so the even-length check passes; `é` straddles the second pair.
    assert_eq!(
        hex_decode("aéb"),
        Err("hex string is not ASCII".to_string())
    );
    // ASCII input decodes and fails exactly as before.
    assert_eq!(hex_decode("0aFf"), Ok(vec![0x0a, 0xff]));
    assert_eq!(
        hex_decode("abc"),
        Err("hex string has odd length".to_string())
    );
    assert!(hex_decode("zz").is_err());

    let file = PolicyFile::parse("[[exec]]\ntarget = \"cargo\"\nsig = \"aéb\"\n").unwrap();
    let (kept, warnings) = file.verified_approves(&Ed25519ApproveVerifier {
        verifying_key: [7u8; 32],
    });
    assert!(kept.exec.is_empty(), "{kept:?}");
    assert_eq!(
        warnings,
        ["approve entry `cargo` dropped: hex string is not ASCII"]
    );
}
