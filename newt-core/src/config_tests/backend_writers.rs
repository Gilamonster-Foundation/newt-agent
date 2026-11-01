use super::*;

// Comment-preserving operator edits to backend records and the default selector.

// ---- comment-preserving default-backend writer ----

#[test]
fn with_default_backend_updates_value_and_preserves_unrelated_content() {
    let original = "\
# hand-authored config
default_backend = \"old\" # keep this selection note

[discovery]
hosts = [\"localhost\", \"dgx1.home.arpa\"]

[custom]
operator_note = \"leave me alone\" # custom inline comment
";

    let out = Config::with_default_backend(original, "dgx1-openai-8000").unwrap();
    let parsed: toml::Value = toml::from_str(&out).unwrap();

    assert_eq!(
        parsed.get("default_backend").and_then(toml::Value::as_str),
        Some("dgx1-openai-8000")
    );
    assert!(
        out.contains("# hand-authored config"),
        "top comment lost: {out}"
    );
    assert!(
        out.contains("# keep this selection note"),
        "target inline comment lost: {out}"
    );
    assert!(
        out.contains("dgx1.home.arpa"),
        "discovery table changed: {out}"
    );
    assert!(
        out.contains("leave me alone"),
        "custom table changed: {out}"
    );
    assert!(
        out.contains("# custom inline comment"),
        "unrelated inline comment lost: {out}"
    );
}

/// #1667 review §8: the backend panel's EDIT must not destroy operator
/// content. `with_dropin_edits` touches ONLY the listed keys — comments,
/// key order, and keys `BackendConfig` does not model survive, which a
/// serde round-trip (`from_str` → mutate → `to_string`) silently deletes.
#[test]
fn with_dropin_edits_touches_only_named_keys_and_keeps_comments_and_unknowns() {
    let original = "\
# hand-authored drop-in for the lab box
endpoint = \"http://gpu-runner:11434\" # the LAN address
kind = \"anthropic\"
model = \"qwen3:30b\"
api_key_env = \"OLD_KEY\"
operator_hint = \"do not lose me\"

[serving_notes]
note = \"unmodelled table\"
";
    // Change only the model; clear the api-key env.
    let out = BackendConfig::with_dropin_edits(
        original,
        &[
            ("model", Some("llama3.1:8b".to_string())),
            ("api_key_env", None),
        ],
    )
    .unwrap();

    let parsed: BackendConfig = toml::from_str(&out).unwrap();
    assert_eq!(parsed.model.as_deref(), Some("llama3.1:8b"));
    assert_eq!(parsed.api_key_env, None, "the cleared key is gone");
    assert_eq!(
        parsed.kind,
        Some(BackendKind::Anthropic),
        "an untouched kind survives verbatim (the #1667 §1 corruption)"
    );
    assert!(
        out.contains("# hand-authored drop-in"),
        "comment lost: {out}"
    );
    assert!(
        out.contains("# the LAN address"),
        "inline comment lost: {out}"
    );
    assert!(
        out.contains("operator_hint = \"do not lose me\""),
        "unknown key lost: {out}"
    );
    assert!(out.contains("[serving_notes]"), "unknown table lost: {out}");
}

/// A key the drop-in does not have yet is created; invalid TOML is a
/// visible error, never a silent overwrite.
#[test]
fn with_dropin_edits_creates_missing_keys_and_rejects_invalid_toml() {
    let out = BackendConfig::with_dropin_edits(
        "endpoint = \"http://x:1\"\n",
        &[("kind", Some("openai".to_string()))],
    )
    .unwrap();
    let parsed: BackendConfig = toml::from_str(&out).unwrap();
    assert_eq!(parsed.kind, Some(BackendKind::Openai));
    assert!(BackendConfig::with_dropin_edits("not = = toml", &[]).is_err());
}

#[test]
fn with_default_backend_creates_key_and_is_idempotent() {
    let original = "# config without a default\n[discovery]\nhosts = [\"localhost\"]\n";
    let once = Config::with_default_backend(original, "local").unwrap();
    let twice = Config::with_default_backend(&once, "local").unwrap();

    let parsed: toml::Value = toml::from_str(&once).unwrap();
    assert_eq!(
        parsed.get("default_backend").and_then(toml::Value::as_str),
        Some("local")
    );
    assert_eq!(twice, once, "reapplying the same default changed output");
    assert_eq!(twice.matches("default_backend").count(), 1);
}

#[test]
fn with_default_backend_rejects_empty_name() {
    assert!(Config::with_default_backend("", "").is_err());
    assert!(Config::with_default_backend("", "   ").is_err());
}

#[test]
fn with_default_backend_rejects_invalid_toml() {
    assert!(Config::with_default_backend("this = = not toml", "local").is_err());
}

/// `/settings prefix` persists through this. The trap it must survive is
/// #2569's: a config with a `[tui.permissions]` subtable and no `[tui]` keys.
/// The new key lands in `[tui]`, the subtable and the comments survive, and a
/// second write replaces rather than duplicates.
#[test]
fn with_tui_key_sets_one_scalar_and_keeps_subtables_and_comments() {
    let text = "# my config\ndefault_backend = \"b\"\n\n[tui.permissions]\nnet = [\"example.org\"] # keep me\n";
    let once = Config::with_tui_key(text, "prefix_key", "ctrl+a").unwrap();
    let parsed: toml::Value = toml::from_str(&once).unwrap();
    assert_eq!(parsed["tui"]["prefix_key"].as_str(), Some("ctrl+a"));
    assert_eq!(
        parsed["tui"]["permissions"]["net"][0].as_str(),
        Some("example.org")
    );
    assert!(
        once.contains("# my config") && once.contains("# keep me"),
        "{once}"
    );
    let twice = Config::with_tui_key(&once, "prefix_key", "ctrl+space").unwrap();
    assert_eq!(twice.matches("prefix_key").count(), 1, "{twice}");
    let parsed: toml::Value = toml::from_str(&twice).unwrap();
    assert_eq!(parsed["tui"]["prefix_key"].as_str(), Some("ctrl+space"));
    // A fresh config gains a [tui] table.
    let fresh = Config::with_tui_key("", "prefix_key", "ctrl+n").unwrap();
    assert!(
        fresh.contains("[tui]") && fresh.contains("prefix_key = \"ctrl+n\""),
        "{fresh}"
    );
    assert!(Config::with_tui_key("tui = 3\n", "prefix_key", "x").is_err());
}

/// #2932: the `/settings tools` and `/settings read-only` writers. The
/// `[tool_exposure]` table is created when absent, an existing `profile` and
/// the surrounding comments survive, `None` removes the allow list, and a
/// second write replaces rather than duplicates.
#[test]
fn with_tool_exposure_writers_edit_one_key_and_keep_the_rest() {
    let text = "# my config\n[tool_exposure]\nprofile = \"auto\" # keep me\n";
    let listed =
        Config::with_tool_exposure_allow_list(text, Some(&["read_file", "run_command"])).unwrap();
    let parsed: Config = toml::from_str(&listed).unwrap();
    let exposure = parsed.tool_exposure();
    assert_eq!(exposure.profile, ExposureProfile::Auto);
    assert_eq!(
        exposure.allow_list.as_deref(),
        Some(&["read_file".to_string(), "run_command".to_string()][..])
    );
    assert!(
        listed.contains("# my config") && listed.contains("# keep me"),
        "{listed}"
    );

    let ro = Config::with_tool_exposure_read_only(&listed, true).unwrap();
    let ro_again = Config::with_tool_exposure_read_only(&ro, false).unwrap();
    assert_eq!(ro_again.matches("read_only").count(), 1, "{ro_again}");
    assert!(
        !toml::from_str::<Config>(&ro_again)
            .unwrap()
            .tool_exposure()
            .read_only
    );
    assert!(
        toml::from_str::<Config>(&ro)
            .unwrap()
            .tool_exposure()
            .read_only
    );

    let released = Config::with_tool_exposure_allow_list(&ro, None).unwrap();
    let parsed: Config = toml::from_str(&released).unwrap();
    assert_eq!(parsed.tool_exposure().allow_list, None);
    assert!(
        parsed.tool_exposure().read_only,
        "releasing the list keeps the toggle"
    );

    // A fresh config gains the table; a scalar in its place is an error.
    let fresh = Config::with_tool_exposure_read_only("", true).unwrap();
    assert!(
        fresh.contains("[tool_exposure]") && fresh.contains("read_only = true"),
        "{fresh}"
    );
    assert!(Config::with_tool_exposure_read_only("tool_exposure = 3\n", true).is_err());
}

/// #2981 review: replacing `read_only` / `allow_list` kept neither key's
/// trailing comment, and a same-value write erased it too.
#[test]
fn with_tool_exposure_writers_keep_comments_on_the_edited_keys() {
    let text = "[tool_exposure]\nread_only = false # keep this explanation\n\
                allow_list = [\"read_file\"] # keep this list note\n";
    for out in [
        Config::with_tool_exposure_read_only(text, true).unwrap(),
        Config::with_tool_exposure_read_only(text, false).unwrap(),
        Config::with_tool_exposure_allow_list(text, Some(&["read_file"])).unwrap(),
        Config::with_tool_exposure_allow_list(text, Some(&["find"])).unwrap(),
    ] {
        assert!(out.contains("# keep this explanation"), "{out}");
        assert!(out.contains("# keep this list note"), "{out}");
    }
}
