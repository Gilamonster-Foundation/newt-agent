//! **The cross-language contract** (A2.1, #1828).
//!
//! A schema says what shape a record has. It does not say what ID that
//! record gets, and the id is the part another language can get subtly
//! wrong — a different map ordering, an indefinite-length encoding, a
//! non-smallest integer, and the bytes differ while the JSON looks
//! identical. So each vector carries all three: the record as JSON, the
//! canonical DAG-CBOR bytes as hex, and the ContentId. A non-Rust
//! implementation can encode the JSON, compare bytes, and only then
//! compare ids — which tells it WHICH step it got wrong.
//!
//! Discipline copied from `newt-core/tests/spill_cid_vectors.rs`: a
//! missing file FAILS rather than silently regenerating, and
//! `NEWT_GOLDEN_UPDATE=1` re-baselines deliberately.

use content_addressable::{canonical, ContentAddressable, ContentId};
use newt_interaction::{
    AssertionKind, Audience, ChoiceOption, Control, ControlId, ControlKind, ControlValue,
    FeatureDemand, IdempotencyKey, InteractionDefinition, InteractionInstance, InteractionKind,
    Nonce, OptionId, Provenance, Requirement, ResponderPolicy, ResponderProvenance, Response,
    Revision, Scope, SecretRef, SemanticRole, Submission, SurfaceFeature,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

fn vectors_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/interaction-vectors.json")
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "hex must be byte-aligned");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex digit"))
        .collect()
}

/// One vector: everything a consumer needs to reproduce the id, and to
/// locate which step it got wrong if it cannot.
#[derive(Serialize, serde::Deserialize, PartialEq, Eq, Debug)]
struct Vector {
    /// What this case is for.
    name: String,
    /// Which record type.
    record: String,
    /// The record as JSON — the human-facing form.
    json: serde_json::Value,
    /// JSON paths whose values are CONTENT IDS. This is the fact a
    /// foreign implementation most needs and JSON cannot express: in the
    /// canonical DAG-CBOR these are CID LINKS (tag 42), not strings, so
    /// encoding the JSON above verbatim will NOT reproduce the bytes
    /// below. Encode these fields as links.
    links: Vec<String>,
    /// The canonical DAG-CBOR encoding, hex. Identity is minted over
    /// exactly these bytes.
    dagcbor_hex: String,
    /// The resulting ContentId, canonically rendered.
    content_id: String,
}

fn vector<T: ContentAddressable + Serialize>(
    name: &str,
    record_type: &str,
    links: &[&str],
    value: &T,
) -> Vector {
    let bytes = value.canonical_form().expect("canonical form");
    Vector {
        name: name.to_string(),
        record: record_type.to_string(),
        links: links.iter().map(|l| (*l).to_string()).collect(),
        json: serde_json::to_value(value).expect("json"),
        dagcbor_hex: to_hex(&bytes),
        content_id: value.content_id().expect("content id").to_string(),
    }
}

fn choice_field(id: &str, options: &[(&str, SemanticRole, &str)]) -> Control {
    Control {
        id: ControlId::new(id).unwrap(),
        kind: ControlKind::Choice {
            options: options
                .iter()
                .map(|(oid, role, label)| ChoiceOption {
                    id: OptionId::new(*oid).unwrap(),
                    role: *role,
                    label: (*label).to_string(),
                })
                .collect(),
        },
        label: format!("{id} field"),
        requirement: Requirement::Required,
    }
}

fn field(id: &str, kind: ControlKind, requirement: Requirement) -> Control {
    Control {
        id: ControlId::new(id).unwrap(),
        kind,
        label: format!("{id} field"),
        requirement,
    }
}

/// A definition with one field per control KIND, so every positive
/// response vector answers a control whose kind matches its value.
fn sample_definition() -> InteractionDefinition {
    let mut def = InteractionDefinition::new(
        InteractionKind::Form,
        "⊘ run_command wants to run `bash`",
        vec![
            choice_field(
                "decision",
                &[
                    ("allow-once", SemanticRole::Allow, "allow once"),
                    ("deny", SemanticRole::Deny, "deny (default)"),
                ],
            ),
            field("reason", ControlKind::Text, Requirement::Optional),
            field("remember", ControlKind::Toggle, Requirement::Optional),
            field("passphrase", ControlKind::Secret, Requirement::Optional),
        ],
    );
    def.features = vec![
        FeatureDemand {
            feature: SurfaceFeature::new(SurfaceFeature::SECRET_INPUT).unwrap(),
            requirement: Requirement::Required,
        },
        FeatureDemand {
            feature: SurfaceFeature::new(SurfaceFeature::DIAGRAMS).unwrap(),
            requirement: Requirement::Optional,
        },
    ];
    def
}

fn sample_instance(def: &InteractionDefinition) -> InteractionInstance {
    InteractionInstance {
        schema: newt_interaction::InstanceTag,
        nonce: Nonce::new("1756200000000000000-0f4c1b2e").unwrap(),
        definition: def.definition_id().unwrap(),
        revision: Revision::FIRST,
        ttl_ticks: 300,
        scope: Scope {
            workspace_key: "ws-abc".to_string(),
            conversation_id: "conv-1".to_string(),
        },
        responder_policy: ResponderPolicy {
            audiences: vec![Audience::Terminal, Audience::Web],
            requires_assertion: true,
        },
        provenance: Provenance {
            origin: "permission-gate".to_string(),
            minted_tick: 42,
        },
    }
}

fn sample_response(
    def: &InteractionDefinition,
    inst: &InteractionInstance,
    control: ControlId,
    value: ControlValue,
) -> Response {
    Response {
        schema: newt_interaction::ResponseTag,
        definition: def.definition_id().unwrap(),
        instance: inst.instance_id().unwrap(),
        revision: Revision::FIRST,
        values: vec![Submission { control, value }],
        idempotency_key: IdempotencyKey::new("first-try").unwrap(),
        responder_provenance: ResponderProvenance {
            kind: AssertionKind::SignedAssertion,
            subject: "operator:example".to_string(),
            audience: Audience::Web,
            assertion: Some("assertion-handle-1".to_string()),
        },
    }
}

/// Every record type, and every `ControlValue` variant — the variants are
/// where a consumer's tagged-enum handling is most likely to differ.
fn generate() -> Vec<Vector> {
    let def = sample_definition();
    let inst = sample_instance(&def);
    let mut out = vec![
        vector(
            "definition/choice-with-feature-demands",
            "definition",
            &[],
            &def,
        ),
        vector(
            "instance/two-audiences-assertion-required",
            "instance",
            &["definition"],
            &inst,
        ),
    ];
    // Each response answers the control whose KIND matches its value: a
    // corpus foreign implementations reproduce must be valid in context,
    // not merely enum-complete.
    for (name, control, value) in [
        (
            "choice",
            "decision",
            ControlValue::Choice {
                option: OptionId::new("deny").unwrap(),
            },
        ),
        (
            "text",
            "reason",
            ControlValue::Text {
                text: "a typed answer".to_string(),
            },
        ),
        ("toggle", "remember", ControlValue::Toggle { on: true }),
        (
            "secret-by-reference",
            "passphrase",
            ControlValue::Secret {
                reference: SecretRef::new("vault-handle-9").unwrap(),
            },
        ),
    ] {
        out.push(vector(
            &format!("response/value-{name}"),
            "response",
            &["definition", "instance"],
            &sample_response(&def, &inst, ControlId::new(control).unwrap(), value),
        ));
    }
    out
}

fn render(vectors: &[Vector]) -> String {
    let mut text = serde_json::to_string_pretty(vectors).expect("render");
    text.push('\n');
    text
}

#[test]
fn the_committed_vectors_match_regeneration() {
    let path = vectors_path();
    let generated = render(&generate());

    if std::env::var("NEWT_GOLDEN_UPDATE").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).expect("data dir");
        std::fs::write(&path, &generated).expect("write vectors");
        return;
    }

    // A missing file FAILS. Silently regenerating would make the first run
    // on any machine authoritative, which is how a golden stops being one.
    let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{} is missing ({e}). Re-baseline deliberately with \
             NEWT_GOLDEN_UPDATE=1, never by letting the test write it.",
            path.display()
        )
    });
    assert_eq!(
        committed, generated,
        "the committed vectors no longer match what this build produces. If \
         that is intended, re-baseline with NEWT_GOLDEN_UPDATE=1 and say in \
         the PR what changed on the wire — every non-Rust consumer reads \
         these bytes."
    );
}

/// **Anti-vacuous twin.** A byte-compare that cannot fail is not a golden.
#[test]
fn a_perturbed_vector_fails_the_byte_compare() {
    let mut vectors = generate();
    let original = render(&vectors);

    // One byte of one id, flipped.
    let id = &mut vectors[0].content_id;
    let last = id.pop().expect("non-empty id");
    id.push(if last == 'a' { 'b' } else { 'a' });
    assert_ne!(
        render(&vectors),
        original,
        "perturbing a vector's content id did not change the rendered file — \
         the comparison would pass over a corrupted vector"
    );
}

#[test]
fn a_vector_id_is_reproducible_from_its_canonical_bytes() {
    let committed: Vec<Vector> = serde_json::from_str(
        &std::fs::read_to_string(vectors_path()).expect("vectors file exists"),
    )
    .expect("vectors parse");
    assert!(!committed.is_empty(), "no vectors to check");

    for vector in &committed {
        // 1. The recorded bytes really do produce the recorded id. This is
        //    the step a foreign implementation can verify with nothing but
        //    a BLAKE3 and a CID library.
        let bytes = from_hex(&vector.dagcbor_hex);
        let minted = ContentId::from_canonical_bytes(&bytes);
        assert_eq!(
            minted.to_string(),
            vector.content_id,
            "vector `{}`: the recorded bytes do not mint the recorded id",
            vector.name
        );

        // 2. The bytes round-trip through the typed record unchanged. A
        //    canonical encoder that is not idempotent is not canonical, and
        //    this is the property a foreign implementation is really being
        //    asked to match.
        let reencoded = match vector.record.as_str() {
            "definition" => {
                let record: InteractionDefinition =
                    canonical::from_canonical_dagcbor(&bytes).expect("decode definition");
                record.canonical_form().expect("re-encode")
            }
            "instance" => {
                let record: InteractionInstance =
                    canonical::from_canonical_dagcbor(&bytes).expect("decode instance");
                record.canonical_form().expect("re-encode")
            }
            "response" => {
                let record: Response =
                    canonical::from_canonical_dagcbor(&bytes).expect("decode response");
                record.canonical_form().expect("re-encode")
            }
            other => panic!("vector `{}` names unknown record `{other}`", vector.name),
        };
        assert_eq!(
            to_hex(&reencoded),
            vector.dagcbor_hex,
            "vector `{}`: decoding and re-encoding did not reproduce the bytes",
            vector.name
        );

        // 3. Every path the vector declares a LINK really holds a
        //    canonically-rendered content id. This is the one thing the
        //    JSON cannot show — in the CBOR these are tag-42 links, not
        //    strings — so the declaration has to be checked, not trusted.
        for path in &vector.links {
            let raw = vector.json.get(path).unwrap_or_else(|| {
                panic!(
                    "vector `{}` declares link `{path}`, absent from its json",
                    vector.name
                )
            });
            let text = raw
                .as_str()
                .unwrap_or_else(|| panic!("link `{path}` is not a string in json"));
            let parsed: ContentId = text.parse().unwrap_or_else(|_| {
                panic!(
                    "vector `{}`: link `{path}` is not a content id",
                    vector.name
                )
            });
            assert_eq!(
                parsed.to_string(),
                text,
                "vector `{}`: link `{path}` is not canonically rendered",
                vector.name
            );
        }
    }
}

#[test]
fn the_corpus_covers_every_record_and_value_variant() {
    let committed: Vec<Vector> = serde_json::from_str(
        &std::fs::read_to_string(vectors_path()).expect("vectors file exists"),
    )
    .expect("vectors parse");
    for record in ["definition", "instance", "response"] {
        assert!(
            committed.iter().any(|v| v.record == record),
            "no vector covers the `{record}` record"
        );
    }
    for variant in ["choice", "text", "toggle", "secret-by-reference"] {
        assert!(
            committed.iter().any(|v| v.name.ends_with(variant)),
            "no vector covers the `{variant}` control value"
        );
    }
}

/// **Every committed vector survives a decode/encode round trip.**
///
/// The corpus is the cross-language contract, so it has to satisfy the
/// same rule the decoder enforces: canonical bytes decode to a record that
/// re-encodes to exactly those bytes, and the id recomputed from them is
/// the id recorded beside them. A vector that failed this would be
/// teaching foreign implementations something untrue.
#[test]
fn decoding_then_encoding_reproduces_every_vector() {
    let committed: Vec<Vector> = serde_json::from_str(
        &std::fs::read_to_string(vectors_path()).expect("vectors file exists"),
    )
    .expect("vectors parse");
    assert!(!committed.is_empty(), "no vectors to check");

    for vector in &committed {
        let bytes = from_hex(&vector.dagcbor_hex);
        let (reencoded, id) = match vector.record.as_str() {
            "definition" => match newt_interaction::decode_definition(&bytes).unwrap() {
                newt_interaction::Decoded::Known(r) => (
                    r.canonical_form().unwrap(),
                    r.content_id().unwrap().to_string(),
                ),
                newt_interaction::Decoded::Unknown(_) => {
                    panic!("vector `{}` did not decode as known", vector.name)
                }
            },
            "instance" => match newt_interaction::decode_instance(&bytes).unwrap() {
                newt_interaction::Decoded::Known(r) => (
                    r.canonical_form().unwrap(),
                    r.content_id().unwrap().to_string(),
                ),
                newt_interaction::Decoded::Unknown(_) => {
                    panic!("vector `{}` did not decode as known", vector.name)
                }
            },
            "response" => match newt_interaction::decode_response(&bytes).unwrap() {
                newt_interaction::Decoded::Known(r) => (
                    r.canonical_form().unwrap(),
                    r.content_id().unwrap().to_string(),
                ),
                newt_interaction::Decoded::Unknown(_) => {
                    panic!("vector `{}` did not decode as known", vector.name)
                }
            },
            other => panic!("vector `{}` names unknown record `{other}`", vector.name),
        };
        assert_eq!(
            to_hex(&reencoded),
            vector.dagcbor_hex,
            "vector `{}`: decode then encode did not reproduce the bytes",
            vector.name
        );
        assert_eq!(
            id, vector.content_id,
            "vector `{}`: the recomputed id differs from the recorded one",
            vector.name
        );
    }
}
