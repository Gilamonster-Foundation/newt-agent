use agent_toolchain::native_git::helper::{self, HelperRole, HookRequest, HookResponse};
use content_addressable::{canonical, ContentAddressable};
use std::io::Cursor;

#[test]
fn message_helper_uses_checked_codec_and_preserves_file_on_refusal() {
    let directory = tempfile::tempdir().unwrap();
    let message = directory.path().join("COMMIT_EDITMSG");
    std::fs::write(&message, "original\n").unwrap();
    let args = [message.clone().into_os_string()];
    let mut seen = None;
    helper::run(
        HelperRole::PrepareMessage,
        &args,
        &mut Cursor::new([]),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut |bytes| {
            let request = helper::decode_request(bytes)?;
            // Identity is derived from the same canonical value, not a sequence or
            // path supplied by the native child.
            assert_eq!(request.canonical_form().unwrap(), bytes);
            seen = Some(request);
            HookResponse::Success(b"finalized\n".to_vec())
                .canonical_form()
                .map_err(|error| error.to_string())
        },
    )
    .unwrap();
    assert_eq!(
        seen,
        Some(HookRequest::Message {
            message: "original\n".into()
        })
    );
    assert_eq!(std::fs::read(&message).unwrap(), b"finalized\n");
    let error = helper::run(
        HelperRole::CommitMessage,
        &args,
        &mut Cursor::new([]),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut |_| {
            HookResponse::Refused("cancelled".into())
                .canonical_form()
                .map_err(|error| error.to_string())
        },
    )
    .unwrap_err();
    assert!(error.contains("cancelled"));
    assert_eq!(std::fs::read(&message).unwrap(), b"finalized\n");
}

#[test]
fn signing_helper_preserves_exact_payload_and_actual_signature_armor() {
    let args = [
        "--status-fd=2".into(),
        "-bsau".into(),
        "invocation-key".into(),
    ];
    let payload = b"tree example\n\nmessage\n";
    let armor = b"-----BEGIN SSH SIGNATURE-----\nexact\n-----END SSH SIGNATURE-----\n";
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();
    helper::run(
        HelperRole::Sign,
        &args,
        &mut Cursor::new(payload),
        &mut output,
        &mut diagnostics,
        &mut |bytes| {
            assert_eq!(
                helper::decode_request(bytes)?,
                HookRequest::Sign {
                    commit: payload.to_vec()
                }
            );
            HookResponse::Success(armor.to_vec())
                .canonical_form()
                .map_err(|error| error.to_string())
        },
    )
    .unwrap();
    assert_eq!(output, armor);
    assert_eq!(diagnostics, b"[GNUPG:] SIG_CREATED \n");
}

#[test]
fn helper_rejects_bad_role_arguments_before_transport_or_success_marker() {
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();
    assert!(helper::run(
        HelperRole::Sign,
        &["--verify".into()],
        &mut Cursor::new([]),
        &mut output,
        &mut diagnostics,
        &mut |_| panic!("invalid request reached signer")
    )
    .is_err());
    assert!(output.is_empty());
    assert!(diagnostics.is_empty());
    assert!(helper::run(
        HelperRole::ReferenceTransaction,
        &["invented".into()],
        &mut Cursor::new([]),
        &mut output,
        &mut diagnostics,
        &mut |_| panic!("invalid phase reached host")
    )
    .is_err());
}

#[test]
fn reference_helper_forwards_original_phase_and_update_bytes() {
    let updates = "0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 refs/heads/task\n";
    helper::run(
        HelperRole::ReferenceTransaction,
        &["prepared".into()],
        &mut Cursor::new(updates),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut |bytes| {
            assert_eq!(
                helper::decode_request(bytes)?,
                HookRequest::References {
                    phase: "prepared".into(),
                    updates: updates.into()
                }
            );
            HookResponse::Success(Vec::new())
                .canonical_form()
                .map_err(|error| error.to_string())
        },
    )
    .unwrap();
}

#[test]
fn malformed_or_substituted_wire_value_is_not_an_approval() {
    assert!(helper::decode_request(b"not dag cbor").is_err());
    let bytes = HookRequest::Sign {
        commit: b"exact".to_vec(),
    }
    .canonical_form()
    .unwrap();
    let decoded: HookRequest = canonical::from_canonical_dagcbor_checked(&bytes).unwrap();
    assert_eq!(
        decoded,
        HookRequest::Sign {
            commit: b"exact".to_vec()
        }
    );
    assert!(helper::run(
        HelperRole::Sign,
        &["--status-fd=2".into(), "-bsau".into(), "key".into()],
        &mut Cursor::new([]),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut |_| Ok(bytes.clone())
    )
    .is_err());
}
