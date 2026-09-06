use super::*;

#[test]
fn parse_row_cap_defaults_and_clamps() {
    assert_eq!(parse_row_cap(None), DEFAULT_ROW_CAP);
    assert_eq!(
        parse_row_cap(Some(&serde_json::json!(null))),
        DEFAULT_ROW_CAP
    );
    assert_eq!(parse_row_cap(Some(&serde_json::json!(0))), 0);
    assert_eq!(parse_row_cap(Some(&serde_json::json!(50))), 50);
    // Negative → treated as the default (a negative cap is meaningless).
    assert_eq!(parse_row_cap(Some(&serde_json::json!(-5))), DEFAULT_ROW_CAP);
    // Mistyped caps a model occasionally emits — a float, a stringified
    // number, or a bool — must fall back to the safe default, NOT to an
    // unbounded read. This is the load-bearing case: returning usize::MAX
    // here would silently defeat the honest `truncated` contract.
    assert_eq!(
        parse_row_cap(Some(&serde_json::json!(2.5))),
        DEFAULT_ROW_CAP
    );
    assert_eq!(
        parse_row_cap(Some(&serde_json::json!("100"))),
        DEFAULT_ROW_CAP
    );
    assert_eq!(
        parse_row_cap(Some(&serde_json::json!(true))),
        DEFAULT_ROW_CAP
    );
    // A huge u64 beyond i64 range is still accepted as-is (saturates to
    // usize::MAX on a 64-bit target); the engine caps the actual read and
    // sets the honest `truncated` flag.
    assert_eq!(
        parse_row_cap(Some(&serde_json::json!(u64::MAX))),
        usize::MAX
    );
}
#[test]
fn required_str_present_and_absent() {
    let args = serde_json::json!({ "a": "x" });
    assert_eq!(required_str(&args, "a").unwrap(), "x");
    let err = required_str(&args, "b").unwrap_err();
    assert_eq!(err["isError"], true);
    assert!(err["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("missing required argument: b"));
}
/// `parse_head` defaults to 5 and folds every mistyped value to the default,
/// taking a genuine non-negative integer as-is (mirrors `parse_row_cap`).
#[test]
fn parse_head_defaults_and_folds_mistypes() {
    assert_eq!(parse_head(None), DEFAULT_HEAD);
    assert_eq!(parse_head(Some(&serde_json::json!(null))), DEFAULT_HEAD);
    assert_eq!(parse_head(Some(&serde_json::json!(0))), 0);
    assert_eq!(parse_head(Some(&serde_json::json!(10))), 10);
    assert_eq!(parse_head(Some(&serde_json::json!(-3))), DEFAULT_HEAD);
    assert_eq!(parse_head(Some(&serde_json::json!(2.5))), DEFAULT_HEAD);
    assert_eq!(parse_head(Some(&serde_json::json!("7"))), DEFAULT_HEAD);
    assert_eq!(parse_head(Some(&serde_json::json!(true))), DEFAULT_HEAD);
}
