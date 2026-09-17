use super::*;

fn payload() -> Payload {
    Payload {
        schema: SCHEMA.into(),
        workspaces: BTreeMap::from([(
            if cfg!(windows) {
                "C:\\workspace"
            } else {
                "/workspace"
            }
            .into(),
            GrantSet::from([
                (DenialKind::RemoteTool, "example__read_messages".into()),
                (DenialKind::GitWrite, "add".into()),
            ]),
        )]),
    }
}

#[test]
fn payload_identity_covers_schema_workspace_kind_and_target() {
    let original = payload();
    let id = original.content_id().unwrap();
    let mut modified = original.clone();
    modified.schema.push('2');
    assert_ne!(modified.content_id().unwrap(), id);
    let mut modified = original.clone();
    let (workspace, grants) = modified.workspaces.pop_first().unwrap();
    modified
        .workspaces
        .insert(format!("{workspace}/other"), grants);
    assert_ne!(modified.content_id().unwrap(), id);
    for (kind, target) in [(DenialKind::Exec, "add"), (DenialKind::GitWrite, "commit")] {
        let mut modified = original.clone();
        let grants = modified.workspaces.values_mut().next().unwrap();
        grants.remove(&(DenialKind::GitWrite, "add".into()));
        grants.insert((kind, target.into()));
        assert_ne!(modified.content_id().unwrap(), id);
    }
}

#[test]
fn valid_encryption_does_not_make_a_tampered_signature_trusted() {
    let root = UserKey::generate();
    let encryption = TokenIdentity::generate();
    let ciphertext = encode(&payload(), &root, &encryption).unwrap();
    let plaintext =
        crate::secrets::decrypt_with_identity(&encryption, ciphertext.as_bytes()).unwrap();
    let mut signed = SignedPayload::from_canonical_form(&plaintext).unwrap();
    signed
        .payload
        .workspaces
        .values_mut()
        .next()
        .unwrap()
        .insert((DenialKind::Exec, "unexpected".into()));
    let tampered =
        crate::secrets::encrypt_to_identity(&encryption, &signed.canonical_form().unwrap())
            .unwrap();
    assert!(decode(tampered.as_bytes(), &root.public(), &encryption).is_err());
    assert_eq!(
        decode(ciphertext.as_bytes(), &root.public(), &encryption).unwrap(),
        payload()
    );
}

#[test]
fn signed_unknown_schema_and_noncanonical_payload_are_rejected() {
    let root = UserKey::generate();
    let encryption = TokenIdentity::generate();
    let mut payload = payload();
    payload.schema = "newt.durable-grants/unknown".into();
    let signed = SignedPayload {
        signature: SerdeSig(root.sign(&payload.canonical_form().unwrap())),
        payload,
    };
    let unknown =
        crate::secrets::encrypt_to_identity(&encryption, &signed.canonical_form().unwrap())
            .unwrap();
    assert!(decode(unknown.as_bytes(), &root.public(), &encryption).is_err());
    let json =
        crate::secrets::encrypt_to_identity(&encryption, &serde_json::to_vec(&signed).unwrap())
            .unwrap();
    assert!(decode(json.as_bytes(), &root.public(), &encryption).is_err());
}

#[test]
fn durable_grants_bounds_reject_before_encrypting_or_parsing() {
    let root = UserKey::generate();
    let encryption = TokenIdentity::generate();
    assert!(decode(&vec![0; MAX_STORE_BYTES + 1], &root.public(), &encryption).is_err());
    let mut oversized = payload();
    oversized
        .workspaces
        .values_mut()
        .next()
        .unwrap()
        .insert((DenialKind::FsRead, "x".repeat(MAX_TARGET_BYTES + 1)));
    assert!(encode(&oversized, &root, &encryption).is_err());
    let mut excessive = payload();
    *excessive.workspaces.values_mut().next().unwrap() = (0..=MAX_GRANTS)
        .map(|index| (DenialKind::Exec, format!("command-{index}")))
        .collect();
    assert!(encode(&excessive, &root, &encryption).is_err());
    let mut excessive = payload();
    let (workspace, grants) = excessive.workspaces.pop_first().unwrap();
    excessive.workspaces = (0..=MAX_WORKSPACES)
        .map(|index| (format!("{workspace}/{index}"), grants.clone()))
        .collect();
    assert!(encode(&excessive, &root, &encryption).is_err());
}
