use super::*;
use crate::ocap_policy::{hex_encode, Ed25519ApproveVerifier};
use ed25519_dalek::{Signer, SigningKey};

fn body() -> Body {
    Body {
        schema: "newt.build-toolchains/v1".into(),
        roots: vec![
            Root {
                path: if cfg!(windows) {
                    r"C:\opt\curated"
                } else {
                    "/opt/curated"
                }
                .into(),
                expected_tree: None,
            },
            Root {
                path: if cfg!(windows) {
                    r"C:\opt\other"
                } else {
                    "/opt/other"
                }
                .into(),
                expected_tree: Some(ContentId::from_canonical_bytes(
                    &canonical::to_canonical_dagcbor(&"tree").unwrap(),
                )),
            },
        ],
    }
}

fn signed(body: &Body) -> (String, Ed25519ApproveVerifier) {
    let key = SigningKey::from_bytes(&[42; 32]);
    let sig = hex_encode(&key.sign(&body.canonical_form().unwrap()).to_bytes());
    let text = format!("sig = \"{sig}\"\n{}", toml::to_string(body).unwrap());
    (
        text,
        Ed25519ApproveVerifier {
            verifying_key: key.verifying_key().to_bytes(),
        },
    )
}

#[test]
fn signed_ordered_body_round_trips_and_rejects_every_mutation() {
    let original = body();
    let (text, verifier) = signed(&original);
    let accepted =
        verify(&text, &verifier).expect("signed operator roots must reach build preparation");
    assert_eq!(
        accepted.content_id().unwrap(),
        original.content_id().unwrap()
    );
    assert_eq!(
        Body::from_canonical_form(&original.canonical_form().unwrap())
            .unwrap()
            .content_id()
            .unwrap(),
        original.content_id().unwrap()
    );
    for changed in [
        text.replace(&original.roots[0].path, "/opt/injected"),
        text.replace("newt.build-toolchains/v1", "newt.build-toolchains/v2"),
        text.lines()
            .filter(|l| !l.starts_with("expected_tree"))
            .collect::<Vec<_>>()
            .join("\n"),
        format!("unknown = true\n{text}"),
        text.lines()
            .filter(|l| !l.starts_with("sig"))
            .collect::<Vec<_>>()
            .join("\n"),
        format!("{text}\nextra = true"),
    ] {
        assert!(verify(&changed, &verifier).is_err(), "{changed}");
    }
    assert!(verify(
        &text,
        &Ed25519ApproveVerifier {
            verifying_key: [0; 32]
        }
    )
    .is_err());
    for mutate in [0, 1, 2] {
        let mut changed = original.clone();
        match mutate {
            0 => changed.roots.reverse(),
            1 => {
                changed.roots.pop();
            }
            _ => changed.roots.push(changed.roots[0].clone()),
        }
        let changed_text = format!(
            "{}\n{}",
            text.lines().next().unwrap(),
            toml::to_string(&changed).unwrap()
        );
        assert!(verify(&changed_text, &verifier).is_err());
    }
}

#[test]
fn even_a_valid_signature_cannot_authorize_bad_structure() {
    for path in ["relative/bin", "/opt/../escape"] {
        let mut b = body();
        b.roots[0].path = path.into();
        let (text, verifier) = signed(&b);
        assert!(verify(&text, &verifier).is_err());
    }
    let mut b = body();
    b.roots.push(b.roots[0].clone());
    let (text, verifier) = signed(&b);
    assert!(verify(&text, &verifier)
        .unwrap_err()
        .to_string()
        .contains("duplicate"));
    let (text, verifier) = signed(&body());
    for invalid in [
        "not-a-cid",
        "bafkreigh2akiscaildc6k2h5urub2tuib4twjodjdiqhl7gm4mk2c6i3ia",
    ] {
        let cid = body().roots[1].expected_tree.unwrap().to_string();
        assert!(verify(&text.replace(&cid, invalid), &verifier).is_err());
    }
}

#[test]
fn ownership_modes_aliases_and_special_files_fail_closed() {
    for owner in [0, 1000] {
        member_facts(owner, 1000, 0o755, true, false, 1).unwrap();
        member_facts(owner, 1000, 0o700, false, true, 3).unwrap();
    }
    for (owner, mode, regular, directory, links) in [
        (1001, 0o755, true, false, 1),
        (1000, 0o775, true, false, 1),
        (1000, 0o702, false, true, 2),
        (1000, 0o777, false, false, 1),
        (1000, 0o600, false, false, 1),
        (1000, 0o755, true, false, 2),
    ] {
        assert!(member_facts(owner, 1000, mode, regular, directory, links).is_err());
    }
    // Host root ownership remains valid even though it would look unmapped in
    // a user namespace. The executor consumes evidence, not child-visible uid.
    assert!(check_owner(0, 1000).is_ok());
    assert!(check_owner(65534, 1000).is_err());
}

#[cfg(unix)]
#[test]
fn paths_exclude_ancestors_descendants_aliases_and_private_mounts() {
    let root = Path::new("/opt/curated");
    for other in ["/opt", "/opt/curated", "/opt/curated/cache"] {
        assert!(check_location(root, root, &[other.into()]).is_err());
    }
    check_location(root, root, &["/opt/curated-other".into()]).unwrap();
    assert!(check_location(Path::new("/alias"), root, &[]).is_err());
    for hidden in ["/", "/tmp/curated", "/dev/shm/curated", "/opt/../curated"] {
        assert!(check_location(Path::new(hidden), Path::new(hidden), &[]).is_err());
    }
}

#[test]
fn a_later_refusal_never_installs_the_earlier_root() {
    let mut checked = Vec::new();
    let mut dispatched = false;
    let result = admit_with(&body(), |root| {
        checked.push(root.path.clone());
        if root.path.ends_with("other") {
            bail!("mock: missing bin or mismatched tree")
        }
        Ok(root.path.clone())
    })
    .inspect(|_| {
        dispatched = true;
    });
    assert!(result.is_err());
    assert_eq!(checked.len(), 2);
    assert!(!dispatched);
}

#[cfg(unix)]
#[test]
fn composition_preserves_writes_and_reports_host_evidence_outside_child_output() {
    use crate::confined_exec::{ExecOrigin, ExecRequest};
    let caveats = crate::Caveats {
        fs_read: Scope::only(["/work".into()]),
        fs_write: Scope::only(["/work".into()]),
        net: Scope::none(),
        ..crate::Caveats::top()
    };
    let evidence: Vec<_> = body()
        .roots
        .iter()
        .map(|root| Admission {
            policy: body().content_id().unwrap(),
            root: root.path.clone(),
            bin: format!("{}/bin", root.path),
            expected_tree: root.expected_tree,
            observed_tree: root.expected_tree,
            identity_check: if root.expected_tree.is_some() {
                "verified"
            } else {
                "unpinned"
            },
            access: "read + executable runtime; no new writes",
            root_binding: "mock host descriptor",
        })
        .collect();
    let request = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        "make",
        ["test"],
        "/work",
        caveats.clone(),
    )
    .env("PATH", "/usr/bin:/opt/curated/bin")
    .env("NEWT_CONFIG_DIR", "/model/injected")
    .env("HOME", "/model/home")
    .install_toolchains(evidence, Vec::new())
    .unwrap();
    assert_eq!(request.caveats().fs_write, caveats.fs_write);
    assert_eq!(request.caveats().exec, caveats.exec);
    assert_eq!(request.caveats().net, caveats.net);
    assert_eq!(
        request
            .env_grants()
            .iter()
            .find(|(key, _)| key == "PATH")
            .unwrap()
            .1,
        "/opt/curated/bin:/opt/other/bin:/usr/bin"
    );
    let line = report(request.toolchains()).unwrap();
    assert!(line.contains("unpinned") && line.contains("verified"));
    assert!(line.contains(&body().content_id().unwrap().to_string()));
    assert!(!line.contains("model/injected"));
    let ordinary = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        "sh",
        ["-c", "true"],
        "/work",
        caveats,
    );
    assert!(ordinary.with_operator_toolchains(&Scope::none()).is_err());
    assert!(report(&[]).is_none());
}

#[test]
fn model_configuration_cannot_deserialize_a_toolchain_policy() {
    // --config, project and self-edit share Config, which has no such setting.
    // Unknown settings either fail parsing or are discarded; neither route
    // produces the separately signed startup Body.
    let injected = "[toolchains]\nroots = ['/model/injected']\nsig = 'forged'\n";
    if let Ok(config) = toml::from_str::<crate::Config>(injected) {
        assert!(serde_json::to_value(config)
            .unwrap()
            .get("toolchains")
            .is_none());
    }
    let request = crate::confined_exec::ExecRequest::new(
        crate::confined_exec::ExecOrigin::AgentInfluenced,
        "newt",
        ["doctor", "--sign-toolchains"],
        "/model",
        crate::Caveats::top(),
    )
    .env("NEWT_CONFIG_DIR", "/model/injected")
    .env("NEWT_CONFIG", "/model/config.toml");
    assert!(request.toolchains().is_empty());
    assert!(request.with_operator_toolchains(&Scope::All).is_err());
}

#[cfg(not(all(unix, feature = "confine")))]
#[test]
fn unsupported_ownership_checks_fail_closed_without_io() {
    assert!(trusted_source(Path::new("unopened"))
        .unwrap_err()
        .to_string()
        .contains("unavailable"));
}

#[test]
fn explicit_signing_preserves_the_ordered_body_and_never_makes_unsigned_input_authority() {
    let body = body();
    let unsigned = toml::to_string(&body).unwrap();
    let key = SigningKey::from_bytes(&[29; 32]);
    let verifier = Ed25519ApproveVerifier {
        verifying_key: key.verifying_key().to_bytes(),
    };
    assert!(verify(&unsigned, &verifier).is_err());
    assert_eq!(
        parse(&unsigned).unwrap().0.content_id().unwrap(),
        body.content_id().unwrap()
    );
    let calls = std::cell::Cell::new(0);
    let signed = signed_toml(&body, &|bytes| {
        calls.set(calls.get() + 1);
        assert_eq!(bytes, body.canonical_form().unwrap());
        key.sign(bytes).to_bytes()
    })
    .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(
        verify(&signed, &verifier).unwrap().content_id().unwrap(),
        body.content_id().unwrap()
    );
    assert!(
        verify(&unsigned, &verifier).is_err(),
        "review/sign helpers cannot install authority"
    );
    let mut invalid = body.clone();
    invalid.roots.push(body.roots[0].clone());
    assert!(signed_toml(&invalid, &|_| panic!("invalid scope reached signing key")).is_err());
}
