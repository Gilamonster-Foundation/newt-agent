use super::*;

#[test]
fn imported_names_are_portable_token_store_components() {
    for invalid in [
        "../other",
        "a/b",
        r"a\b",
        ".",
        "..",
        "bad:name",
        "bad name",
        "CON",
        "lpt1.remote",
        "review.",
        "review__source",
        "review--source",
        "review-_source",
    ] {
        assert!(validate_import_server_name(invalid).is_err(), "{invalid}");
    }
    for valid in ["review", "Case.Sensitive-name", "review_source"] {
        validate_import_server_name(valid).unwrap();
    }
}

#[test]
fn imported_names_are_unique_under_effective_tool_namespacing() {
    reject_namespace_collisions(["review-source", "calendar"], true).unwrap();
    let error = reject_namespace_collisions(["review-source", "review_source"], true).unwrap_err();
    assert!(error.to_string().contains("effective tool namespace"));
    reject_namespace_collisions(["review-source", "review_source"], false).unwrap();
}
