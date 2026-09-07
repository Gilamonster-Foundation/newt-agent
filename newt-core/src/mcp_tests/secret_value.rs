use super::*;

// ---- SecretValue: deserialization + resolution ----
#[derive(Deserialize)]
struct Holder {
    v: SecretValue,
}

#[test]
fn secret_value_string_deserializes_to_literal() {
    // TOML (newt config) and JSON (Claude import) both map a bare string to
    // a Literal — backward-compatible with every existing config.
    let toml_h: Holder = toml::from_str(r#"v = "hello""#).unwrap();
    assert_eq!(toml_h.v, SecretValue::Literal("hello".into()));
    let json_v: SecretValue = serde_json::from_value(serde_json::json!("hi")).unwrap();
    assert_eq!(json_v, SecretValue::Literal("hi".into()));
}
#[test]
fn secret_value_table_deserializes_to_ref() {
    let cmd_h: Holder =
        toml::from_str(r#"v = { cmd = "vault kv get -field=token secret/gh" }"#).unwrap();
    assert_eq!(
        cmd_h.v,
        SecretValue::Ref(SecretRef {
            cmd: Some("vault kv get -field=token secret/gh".into()),
            ..Default::default()
        })
    );
    let env_h: Holder = toml::from_str(r#"v = { env = "TOK" }"#).unwrap();
    assert!(matches!(
        env_h.v,
        SecretValue::Ref(SecretRef { env: Some(_), .. })
    ));
    let file_h: Holder = toml::from_str(r#"v = { file = "~/.secrets/x" }"#).unwrap();
    assert!(matches!(
        file_h.v,
        SecretValue::Ref(SecretRef { file: Some(_), .. })
    ));
}
#[test]
fn secret_value_literal_resolves_verbatim_without_tokens() {
    // No `${...}` → no env/fs/subprocess touched: a pure pass-through.
    assert_eq!(
        SecretValue::literal("info").resolve().unwrap().expose(),
        "info"
    );
    assert_eq!(SecretValue::literal("").resolve().unwrap().expose(), "");
}
#[test]
fn secret_value_as_literal_and_roundtrips_through_toml() {
    assert_eq!(SecretValue::literal("x").as_literal(), Some("x"));
    assert_eq!(SecretValue::Ref(SecretRef::default()).as_literal(), None);
    // A Literal serializes as a bare string; a Ref as an inline table.
    #[derive(Serialize)]
    struct H {
        v: SecretValue,
    }
    let lit = toml::to_string(&H {
        v: SecretValue::literal("hi"),
    })
    .unwrap();
    assert!(lit.contains("v = \"hi\""), "{lit}");
    let refd = toml::to_string(&H {
        v: SecretValue::Ref(SecretRef {
            cmd: Some("vault kv get x".into()),
            ..Default::default()
        }),
    })
    .unwrap();
    assert!(refd.contains("cmd"), "{refd}");
    assert!(refd.contains("vault kv get x"), "{refd}");
}
