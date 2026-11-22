//! #3160: `ocap_policy` is a copy of agent-bridle-core's durable-policy contract, so newt-core
//! needs no bridle crate to load and evaluate it. While bridle is a dependency the two must
//! agree on everything a store writes or reads: the parse and its refusals, the serialized form,
//! the signing payload (a signature minted by one must verify under the other), which entries
//! survive verification and why the rest are dropped, and the precedence law.

use crate::ocap_policy as ours;
use agent_bridle::policy as theirs;
use ed25519_dalek::{Signer, SigningKey};

/// Every string literal in the copied tests.
const CORPUS: &[&str] = &[
    "deny.toml",
    "approve.toml",
    "[[exec]]\ntarget = \"cargo\"\nnote = \"build tooling\"\ngranted = \"2026-07-14\"\n\n\
                 [[fs]]\npath = \"~/workspaces\"\nwrite = true\n\n[[net]]\nhost = \"crates.io\"\n",
    "cargo",
    "crates.io",
    "by =",
    "unset provenance skipped: {out}",
    "",
    "[[exec]]\ntarget=\"x\"\nnotee=\"typo\"\n",
    "[[exec]]\ntarget=\"rm\"\n",
    "rm",
    "[[net]]\nhost=\"api.example\"\n",
    "api.example",
    "[[exec]]\ntarget=\"cargo\"\n",
    "python3",
    "class-scoped",
    "bash",
    "anything",
    "agent-bridle:ocap-approve:v1\nexec\ncargo",
    "/ws",
    "write is authority-bearing",
    "bad signature",
    "[[exec]]\ntarget=\"cargo\"\nsig=\"0a0b\"\n\
                 [[exec]]\ntarget=\"git\"\n\
                 [[net]]\nhost=\"crates.io\"\nsig=\"ff\"\n",
    "only the valid-sig entry survives",
    "bad sig dropped",
    "{warnings:?}",
    "git",
    "unsigned",
    "sig = \"0a0b\"",
    "{toml}",
    "00ff1a",
    "00FF1A",
    "abc",
    "zz",
    "replayed sig must not carry to a new target",
];

/// Documents the copied tests do not use: every entry shape, and each way the schema refuses.
const EXTRAS: &[&str] = &[
    "",
    "# only a comment\n",
    "[[exec]]\ntarget = \"cargo\"\nnote = \"n\"\ngranted = \"2026-07-14\"\nby = \"op\"\nsig = \"00ff\"\n",
    "[[fs]]\npath = \"/tmp\"\nwrite = true\n[[fs]]\npath = \"/etc\"\n",
    "[[net]]\nhost = \"crates.io\"\n[[net]]\nhost = \"crates.io\"\n",
    "[[exec]]\ntarget = \"é ü\"\n[[fs]]\npath = \"~/a b\"\n[[net]]\nhost = \"[::1]\"\n",
    "[[exe]]\ntarget = \"x\"\n",
    "[[exec]]\ntarget = \"x\"\nbogus = 1\n",
    "[[exec]]\nnote = \"no target\"\n",
    "[[exec]]\ntarget = 3\n",
    "[[fs]]\npath = \"/x\"\nwrite = \"yes\"\n",
    "[[exec\ntarget = \"x\"\n",
    "exec = 1\n",
    "[[net]]\nhost = \"a\"\nsig = 5\n",
];

fn documents() -> impl Iterator<Item = &'static str> {
    CORPUS.iter().chain(EXTRAS).copied()
}

fn signer() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn sign(payload: &[u8]) -> String {
    ours::hex_encode(&signer().sign(payload).to_bytes())
}

fn targets() -> Vec<String> {
    let mut all: Vec<String> = [
        "",
        "cargo",
        "/usr/bin/git",
        "rm -rf",
        "a\nfs\n/etc\nwrite=true",
        "é ü",
        "\0",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    all.push("x".repeat(4096));
    all
}

#[test]
fn the_copy_parses_and_serializes_every_document_as_bridle_does() {
    let (mut accepted, mut refused) = (0, 0);
    for doc in documents() {
        match (ours::PolicyFile::parse(doc), theirs::PolicyFile::parse(doc)) {
            (Ok(a), Ok(b)) => {
                assert_eq!(format!("{a:?}"), format!("{b:?}"), "{doc:?}");
                assert_eq!(a.to_toml(), b.to_toml(), "{doc:?}");
                accepted += 1;
            }
            (Err(a), Err(b)) => {
                // Everything after the prefix is the `toml` crate's own text, position
                // included, and the two crates link different `toml` versions (newt-core 1.1,
                // bridle-core 0.8), so only the refusal and the prefix this code adds must agree.
                let prefix = "ocap policy TOML: ";
                assert!(
                    a.starts_with(prefix) && b.starts_with(prefix),
                    "{doc:?}: {a:?} vs {b:?}"
                );
                refused += 1;
            }
            (a, b) => panic!("one parse accepted {doc:?} and the other refused it: {a:?} vs {b:?}"),
        }
    }
    // Anti-vacuous: the corpus reaches both outcomes in numbers.
    assert!(
        accepted >= 8 && refused >= 8,
        "accepted {accepted}, refused {refused}"
    );
}

#[test]
fn the_signing_payload_is_bridles_byte_for_byte() {
    assert_eq!(ours::SIGNING_DOMAIN, theirs::SIGNING_DOMAIN);
    for t in targets() {
        assert_eq!(
            ours::ExecEntry {
                target: t.clone(),
                ..Default::default()
            }
            .signing_payload(),
            theirs::ExecEntry {
                target: t.clone(),
                ..Default::default()
            }
            .signing_payload(),
            "exec {t:?}"
        );
        assert_eq!(
            ours::NetEntry {
                host: t.clone(),
                ..Default::default()
            }
            .signing_payload(),
            theirs::NetEntry {
                host: t.clone(),
                ..Default::default()
            }
            .signing_payload(),
            "net {t:?}"
        );
        for write in [false, true] {
            assert_eq!(
                ours::FsEntry {
                    path: t.clone(),
                    write,
                    ..Default::default()
                }
                .signing_payload(),
                theirs::FsEntry {
                    path: t.clone(),
                    write,
                    ..Default::default()
                }
                .signing_payload(),
                "fs {t:?} write={write}"
            );
        }
    }
}

#[test]
fn a_signature_minted_by_either_side_verifies_under_the_other() {
    let key = signer().verifying_key().to_bytes();
    let (ours_verifier, theirs_verifier) = (
        ours::Ed25519ApproveVerifier { verifying_key: key },
        theirs::Ed25519ApproveVerifier { verifying_key: key },
    );
    let mut o = ours::ExecEntry {
        target: "cargo".into(),
        ..Default::default()
    };
    o.sig = Some(sign(&o.signing_payload()));
    let mut b = theirs::ExecEntry {
        target: "cargo".into(),
        ..Default::default()
    };
    b.sig = Some(sign(&b.signing_payload()));
    // Ed25519 is deterministic: the same key over the same payload is the same signature.
    assert_eq!(o.sig, b.sig);

    let ours_file = ours::PolicyFile {
        exec: vec![o],
        ..Default::default()
    }
    .to_toml()
    .unwrap();
    let theirs_file = theirs::PolicyFile {
        exec: vec![b],
        ..Default::default()
    }
    .to_toml()
    .unwrap();
    let (kept, warnings) = theirs::PolicyFile::parse(&ours_file)
        .unwrap()
        .verified_approves(&theirs_verifier);
    assert_eq!(
        (kept.exec.len(), warnings.len()),
        (1, 0),
        "bridle accepts our signature: {warnings:?}"
    );
    let (kept, warnings) = ours::PolicyFile::parse(&theirs_file)
        .unwrap()
        .verified_approves(&ours_verifier);
    assert_eq!(
        (kept.exec.len(), warnings.len()),
        (1, 0),
        "we accept bridle's signature: {warnings:?}"
    );
}

/// Three entries that verify, and five that must not: unsigned, a signature replayed onto
/// another target, one replayed across the `write` flag, non-hex, and odd-length hex.
fn mixed_document() -> String {
    let entry = |target: &str, sig: Option<String>| ours::ExecEntry {
        target: target.into(),
        sig,
        ..Default::default()
    };
    let cargo = sign(&entry("cargo", None).signing_payload());
    let tmp = |write: bool, sig: Option<String>| ours::FsEntry {
        path: "/tmp".into(),
        write,
        sig,
        ..Default::default()
    };
    let tmp_write = sign(&tmp(true, None).signing_payload());
    let net = |host: &str, sig: Option<String>| ours::NetEntry {
        host: host.into(),
        sig,
        ..Default::default()
    };
    ours::PolicyFile {
        exec: vec![
            entry("cargo", Some(cargo.clone())),
            entry("git", None),
            entry("rm", Some(cargo)),
        ],
        fs: vec![
            tmp(true, Some(tmp_write.clone())),
            tmp(false, Some(tmp_write)),
        ],
        net: vec![
            net("crates.io", Some("zz".into())),
            net("docs.rs", Some("abc".into())),
            net("a.b", Some(sign(&net("a.b", None).signing_payload()))),
        ],
    }
    .to_toml()
    .unwrap()
}

#[test]
fn verification_keeps_and_drops_the_same_entries_with_the_same_reasons() {
    let key = signer().verifying_key().to_bytes();
    let document = mixed_document();
    let (ours_kept, ours_warnings) = ours::PolicyFile::parse(&document)
        .unwrap()
        .verified_approves(&ours::Ed25519ApproveVerifier { verifying_key: key });
    let (theirs_kept, theirs_warnings) = theirs::PolicyFile::parse(&document)
        .unwrap()
        .verified_approves(&theirs::Ed25519ApproveVerifier { verifying_key: key });
    assert_eq!(ours_kept.to_toml(), theirs_kept.to_toml());
    assert_eq!(ours_warnings, theirs_warnings);
    // Anti-vacuous: both sides kept the three signed entries and dropped the five others.
    assert_eq!(
        ours_kept.exec.len() + ours_kept.fs.len() + ours_kept.net.len(),
        3
    );
    assert_eq!(ours_warnings.len(), 5, "{ours_warnings:?}");
}

#[test]
fn the_precedence_law_and_lookup_agree() {
    let verdicts = [
        (
            ours::Verdict::Deny,
            theirs::Verdict::Deny,
            "[[exec]]\ntarget = \"rm\"\n[[net]]\nhost = \"evil.example\"\n",
        ),
        (
            ours::Verdict::Passkey,
            theirs::Verdict::Passkey,
            "[[exec]]\ntarget = \"sudo\"\n",
        ),
        (
            ours::Verdict::Ask,
            theirs::Verdict::Ask,
            "[[exec]]\ntarget = \"git\"\n[[fs]]\npath = \"/etc\"\n",
        ),
        (
            ours::Verdict::Approve,
            theirs::Verdict::Approve,
            "[[exec]]\ntarget = \"rm\"\n[[exec]]\ntarget = \"git\"\n[[exec]]\ntarget = \"cargo\"\n\
             [[fs]]\npath = \"/tmp\"\n[[net]]\nhost = \"crates.io\"\n",
        ),
    ];
    let (mut ours_set, mut theirs_set) = (ours::PolicySet::default(), theirs::PolicySet::default());
    for (o, t, doc) in verdicts {
        assert_eq!(
            (o.filename(), o.precedence()),
            (t.filename(), t.precedence()),
            "{o:?}"
        );
        ours_set
            .files
            .insert(o, ours::PolicyFile::parse(doc).unwrap());
        theirs_set
            .files
            .insert(t, theirs::PolicyFile::parse(doc).unwrap());
    }
    let classes = [
        (ours::CapabilityClass::Exec, theirs::CapabilityClass::Exec),
        (ours::CapabilityClass::Fs, theirs::CapabilityClass::Fs),
        (ours::CapabilityClass::Net, theirs::CapabilityClass::Net),
    ];
    let mut outcomes = std::collections::BTreeSet::new();
    for (oc, tc) in classes {
        for target in [
            "rm",
            "sudo",
            "git",
            "cargo",
            "/etc",
            "/tmp",
            "crates.io",
            "evil.example",
            "nothing",
            "",
        ] {
            let (a, b) = (
                ours_set.evaluate(oc, target),
                theirs_set.evaluate(tc, target),
            );
            assert_eq!(format!("{a:?}"), format!("{b:?}"), "{oc:?} {target:?}");
            let ours_danger = |_: ours::CapabilityClass, t: &str| t == "rm";
            let theirs_danger = |_: theirs::CapabilityClass, t: &str| t == "rm";
            assert_eq!(
                ours::PolicySet::validate_approve(oc, target, ours_danger),
                theirs::PolicySet::validate_approve(tc, target, theirs_danger),
                "{oc:?} {target:?}"
            );
            outcomes.insert(format!("{a:?}"));
        }
    }
    // Anti-vacuous: the grid hits every verdict and a miss.
    assert_eq!(outcomes.len(), 5, "{outcomes:?}");
}

#[test]
fn hex_is_bridles_and_round_trips() {
    let all: Vec<u8> = (0..=255u8).collect();
    for bytes in [&[][..], &[0][..], &[0xff][..], &all[..]] {
        assert_eq!(ours::hex_encode(bytes), theirs::hex_encode(bytes));
        assert_eq!(
            ours::hex_decode(&ours::hex_encode(bytes)),
            Ok(bytes.to_vec())
        );
    }
}
