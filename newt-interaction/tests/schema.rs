//! **A published JSON Schema per record type** (A2.1, #1828).
//!
//! Generated rather than hand-written, so the schema cannot drift from the
//! serde shape it claims to describe — a hand-authored schema is a second
//! model of the same records, and the two disagree the first time a field
//! moves.
//!
//! Generation is feature-gated (`--features schema`) so `schemars` stays
//! out of the runtime closure `tests/guard.rs` pins at exactly three
//! crates. The committed files are what a non-Rust consumer reads; this
//! test is what keeps them true.
//!
//! **What a schema cannot tell you:** JSON Schema describes the JSON form.
//! Identity is minted over canonical DAG-CBOR, where a `ContentId` field
//! is a tag-42 LINK rather than the string it appears as here. That is the
//! vectors' job (`tests/data/interaction-vectors.json`, the `links`
//! field), and the two artifacts are meant to be read together.
#![cfg(feature = "schema")]

use newt_interaction::{InteractionDefinition, InteractionInstance, Response};
use std::path::{Path, PathBuf};

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("schema")
}

fn published(name: &str, generated: &str) {
    let path = schema_dir().join(format!("{name}.schema.json"));
    if std::env::var("NEWT_GOLDEN_UPDATE").is_ok() {
        std::fs::create_dir_all(schema_dir()).expect("schema dir");
        std::fs::write(&path, generated).expect("write schema");
        return;
    }
    // A missing schema FAILS: silently writing one would make whatever this
    // build happens to produce the published contract.
    let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{} is missing ({e}). Re-baseline deliberately with \
             NEWT_GOLDEN_UPDATE=1.",
            path.display()
        )
    });
    assert_eq!(
        committed, generated,
        "the published schema for `{name}` no longer matches the type. If \
         that is intended, re-baseline with NEWT_GOLDEN_UPDATE=1 and say in \
         the PR what changed — non-Rust consumers read these files."
    );
}

fn render<T: schemars::JsonSchema>() -> String {
    let schema = schemars::schema_for!(T);
    let mut text = serde_json::to_string_pretty(&schema).expect("render schema");
    text.push('\n');
    text
}

#[test]
fn every_record_type_has_a_published_schema() {
    published("definition", &render::<InteractionDefinition>());
    published("instance", &render::<InteractionInstance>());
    published("response", &render::<Response>());
}

/// The schemas describe the records they claim to: every required field of
/// each type appears in its schema. A generated file that described the
/// wrong type would still round-trip against itself.
#[test]
fn each_schema_names_the_fields_of_its_record() {
    for (name, expected) in [
        (
            "definition",
            vec![
                "schema", "kind", "revision", "markdown", "controls", "features",
            ],
        ),
        (
            "instance",
            vec![
                "schema",
                "nonce",
                "definition",
                "revision",
                "ttl_ticks",
                "scope",
                "responder_policy",
                "provenance",
            ],
        ),
        (
            "response",
            vec![
                "schema",
                "definition",
                "instance",
                "revision",
                "values",
                "idempotency_key",
                "responder_provenance",
            ],
        ),
    ] {
        let text = std::fs::read_to_string(schema_dir().join(format!("{name}.schema.json")))
            .expect("schema exists");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("schema parses");
        let required = parsed["required"]
            .as_array()
            .unwrap_or_else(|| panic!("`{name}` schema has no required list"));
        let listed: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        for field in expected {
            assert!(
                listed.contains(&field),
                "`{name}` schema does not require `{field}`: {listed:?}"
            );
        }
    }
}

/// **The published contract states its closedness.**
///
/// `deny_unknown_fields` makes schemars emit `additionalProperties: false`,
/// which is the JSON-Schema way of saying what the decoder enforces: a
/// record carrying a field this build has no name for is not a record of
/// this type. Without it a foreign consumer would reasonably infer that
/// extra fields are tolerated — and would be building exactly the record
/// our decoder refuses.
#[test]
fn every_object_schema_forbids_additional_properties() {
    for name in ["definition", "instance", "response"] {
        let text = std::fs::read_to_string(schema_dir().join(format!("{name}.schema.json")))
            .expect("schema exists");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("schema parses");

        assert_eq!(
            parsed["additionalProperties"],
            serde_json::Value::Bool(false),
            "the `{name}` schema does not forbid additional properties, but \
             the decoder does"
        );

        // ...and so does every nested object definition, or the closedness
        // stops at the top level while the decoder enforces it all the way
        // down.
        let mut open = Vec::new();
        if let Some(defs) = parsed.get("definitions").and_then(|d| d.as_object()) {
            for (def_name, def) in defs {
                if def.get("type").and_then(|t| t.as_str()) != Some("object") {
                    continue;
                }
                if def.get("additionalProperties") != Some(&serde_json::Value::Bool(false)) {
                    open.push(def_name.clone());
                }
            }
        }
        assert!(
            open.is_empty(),
            "nested objects in the `{name}` schema still allow additional \
             properties: {open:?}"
        );
    }
}

/// **A published schema pins its record's version tag.**
///
/// With `"schema": {"type": "string"}` a foreign implementor could
/// validate a DEFINITION record against `response.schema.json` and pass,
/// while our decoder classifies it unknown — so the schema described "any
/// record with some string in its schema field", not "response v1". Each
/// now carries a `const`.
#[test]
fn each_schema_pins_exactly_its_own_version_tag() {
    for (name, tag) in [
        ("definition", newt_interaction::DEFINITION_SCHEMA_V1),
        ("instance", newt_interaction::INSTANCE_SCHEMA_V1),
        ("response", newt_interaction::RESPONSE_SCHEMA_V1),
    ] {
        let text = std::fs::read_to_string(schema_dir().join(format!("{name}.schema.json")))
            .expect("schema exists");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("schema parses");

        // The tag's schema is reached through the property, which schemars
        // may express inline or via $ref into `definitions`.
        let property = &parsed["properties"]["schema"];
        // schemars expresses the tag inline, as a `$ref`, or — when the
        // field carries a doc comment — as `allOf: [{ $ref }]`.
        let reference = property.get("$ref").and_then(|r| r.as_str()).or_else(|| {
            property
                .get("allOf")
                .and_then(|a| a.as_array())
                .and_then(|a| a.first())
                .and_then(|first| first.get("$ref"))
                .and_then(|r| r.as_str())
        });
        let resolved = match reference {
            Some(reference) => {
                let key = reference.rsplit('/').next().expect("a $ref name");
                &parsed["definitions"][key]
            }
            None => property,
        };
        let pinned = resolved
            .get("const")
            .and_then(|c| c.as_str())
            .or_else(|| {
                resolved
                    .get("enum")
                    .and_then(|e| e.as_array())
                    .filter(|values| values.len() == 1)
                    .and_then(|values| values[0].as_str())
            })
            .unwrap_or_else(|| {
                panic!("the `{name}` schema does not pin its version tag: {resolved}")
            });

        // The exact v1 tag is pinned...
        assert_eq!(pinned, tag, "`{name}` pins the wrong tag");
        // ...so the v2 tag and another family's v1 tag both fail to match,
        // which is precisely what a cross-record mix-up looks like.
        assert_ne!(pinned, tag.replace("/v1", "/v2"));
        for (other, other_tag) in [
            ("definition", newt_interaction::DEFINITION_SCHEMA_V1),
            ("instance", newt_interaction::INSTANCE_SCHEMA_V1),
            ("response", newt_interaction::RESPONSE_SCHEMA_V1),
        ] {
            if other != name {
                assert_ne!(
                    pinned, other_tag,
                    "the `{name}` schema accepts the `{other}` tag"
                );
            }
        }
    }
}

/// **The published schemas state the scalar rules the decoder enforces.**
///
/// A schema permitting any string while the decoder refuses most of them
/// is the same false-guarantee defect as a doc comment claiming a
/// validation the wire does not perform — and it is worse here, because
/// the schema is what a foreign implementor builds against.
#[test]
fn validated_scalars_publish_their_constraints() {
    let definition: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(schema_dir().join("definition.schema.json")).unwrap(),
    )
    .unwrap();
    let response: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(schema_dir().join("response.schema.json")).unwrap(),
    )
    .unwrap();

    // Author-assigned names carry the charset AND non-emptiness.
    for (schema, name) in [(&definition, "ControlId"), (&definition, "OptionId")] {
        let def = &schema["definitions"][name];
        assert_eq!(def["minLength"], 1, "{name} may be empty on the wire");
        assert_eq!(
            def["pattern"], "^[A-Za-z0-9_-]+$",
            "{name} does not publish its charset"
        );
    }

    // The remaining validated scalars publish non-emptiness.
    for (schema, name) in [
        (&definition, "SurfaceFeature"),
        (&response, "SecretRef"),
        (&response, "IdempotencyKey"),
    ] {
        assert_eq!(
            schema["definitions"][name]["minLength"], 1,
            "{name} may be empty on the wire"
        );
    }
}
