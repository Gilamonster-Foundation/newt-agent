use super::*;

// MCP configuration writers and the stdio environment boundary (owned by shell.rs).

#[test]
fn mcp_stdio_env_allowlist_excludes_secrets_and_is_closed() {
    // #1155: the stdio-MCP env allow-list must NOT be a passthrough of the
    // whole environment — secret-bearing vars are absent, and it stays a
    // superset of the shell default (a subprocess needs PATH to exec).
    let allow = mcp_stdio_env_passthrough();
    for secret in [
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "AWS_SECRET_ACCESS_KEY",
        "GITHUB_TOKEN",
        "DGX_API_KEY",
        "NVIDIA_API_KEY",
        // The encrypted-token-store unlock channel (crate::secrets):
        // a child process must never inherit the vault passphrase.
        crate::secrets::PASSPHRASE_ENV,
    ] {
        assert!(!allow.contains(&secret), "{secret} must never be inherited");
    }
    assert!(allow.contains(&"PATH"), "a child needs PATH to exec");
    for base in shell_env_passthrough_default() {
        assert!(
            allow.contains(&base.as_str()),
            "{base} (shell default) should be covered"
        );
    }
}

// ---- #1149: /mcp enable|disable config writer ----

#[test]
fn with_mcp_enabled_toggles_and_preserves_comments() {
    let text = "# my config\n[[mcp_servers]]\nname = \"modulex\"\ncommand = \"modulex-mcp\"\n";
    // disable → enabled = false written, comment preserved
    let off = Config::with_mcp_enabled(text, "modulex", false).unwrap();
    assert!(off.contains("enabled = false"));
    assert!(off.contains("# my config"));
    // re-enable → written in place, so a comment on the line would stay
    let on = Config::with_mcp_enabled(&off, "modulex", true).unwrap();
    assert!(on.contains("enabled = true"), "{on}");
    // unknown name errors loudly
    assert!(Config::with_mcp_enabled(text, "nope", false).is_err());
    // entry parses with default enabled=true; explicit false honored
    let e: crate::mcp::McpServerEntry = toml::from_str("name = \"x\"\ncommand = \"x\"\n").unwrap();
    assert!(e.enabled);
    let d: crate::mcp::McpServerEntry =
        toml::from_str("name = \"x\"\ncommand = \"x\"\nenabled = false\n").unwrap();
    assert!(!d.enabled);
}

#[test]
fn with_mcp_enabled_keeps_comments_and_leaves_a_settled_entry_alone() {
    // #3062 review (F9): a toggle to the state the entry already has changes
    // nothing, byte for byte; a real toggle writes the value in place, so the
    // comments on and above the line stay, and so do CRLF line endings.
    let head = "[[mcp_servers]]\nname = \"fs\"\ncommand = \"fs-mcp\"\n";
    for (text, enabled) in [
        (
            format!("{head}enabled = false # disabled: upstream leaks tokens\n"),
            false,
        ),
        (
            format!("{head}# pinned on for the review bot\nenabled = true # ask ops first\n"),
            true,
        ),
        (
            "# servers\r\n[[mcp_servers]]\r\nname = \"fs\"\r\ncommand = \"fs-mcp\"\r\n".to_owned(),
            true,
        ),
    ] {
        assert_eq!(
            Config::with_mcp_enabled(&text, "fs", enabled).unwrap(),
            text
        );
    }
    let toggle = |line: &str, enabled: bool| {
        Config::with_mcp_enabled(&format!("{head}# owner note\n{line}\n"), "fs", enabled).unwrap()
    };
    assert_eq!(
        toggle("enabled = true # owner: platform team", false),
        format!("{head}# owner note\nenabled = false # owner: platform team\n")
    );
    assert_eq!(
        toggle("enabled = false # paused", true),
        format!("{head}# owner note\nenabled = true # paused\n")
    );
    let crlf = "[[mcp_servers]]\r\nname = \"fs\"\r\ncommand = \"fs-mcp\"\r\n";
    assert_eq!(
        Config::with_mcp_enabled(crlf, "fs", false).unwrap(),
        format!("{crlf}enabled = false\r\n")
    );
}

#[test]
fn with_mcp_server_merged_updates_imported_fields_and_keeps_local_fields() {
    let text = "# local MCP settings\n[[mcp_servers]]\nname = \"review\"\ntype = \"http\"\nurl = \"https://old.example/mcp\"\nenabled = false\nlogin_argv = [\"client\", \"login\"]\nrequest_timeout_secs = 90\n";
    let incoming = crate::mcp::McpServerEntry {
        name: "review".into(),
        enabled: true,
        transport: crate::mcp::TransportKind::Http,
        command: None,
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        url: Some("https://new.example/mcp/".into()),
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: crate::mcp::McpTrust::Untrusted,
        origin: None,
    };

    let updated = Config::with_mcp_server_merged(text, &incoming, &Default::default()).unwrap();
    assert!(updated.contains("# local MCP settings"));
    assert!(updated.contains("url = \"https://new.example/mcp/\""));
    assert!(updated.contains("enabled = false"));
    assert!(updated.contains("login_argv = [\"client\", \"login\"]"));
    assert!(updated.contains("request_timeout_secs = 90"));
    let parsed: Config = toml::from_str(&updated).unwrap();
    assert_eq!(
        parsed.mcp_servers[0].url.as_deref(),
        Some("https://new.example/mcp/")
    );
    assert!(!parsed.mcp_servers[0].enabled);
    assert_eq!(parsed.mcp_servers[0].login_argv, vec!["client", "login"]);
    assert_eq!(parsed.mcp_servers[0].request_timeout_secs, Some(90));
}

#[test]
fn with_mcp_server_merged_is_a_noop_when_source_fields_match() {
    let text = "[[mcp_servers]]\nname = \"review\"\ncommand = \"review-mcp\"\n";
    let incoming: crate::mcp::McpServerEntry =
        toml::from_str("name = \"review\"\ncommand = \"review-mcp\"\n").unwrap();
    assert_eq!(
        Config::with_mcp_server_merged(text, &incoming, &Default::default()).unwrap(),
        text
    );
}

#[test]
fn mcp_writers_keep_the_file_style() {
    // #3062 review (F3): a merge that changes no setting hands back the text
    // byte for byte, whatever toml_edit would re-render; a real change keeps
    // the file's missing final newline, CRLF line endings and BOM.
    let server = |name: &str, command: &str| -> crate::mcp::McpServerEntry {
        toml::from_str(&format!("name = \"{name}\"\ncommand = \"{command}\"\n")).unwrap()
    };
    let merge = |text: &str, command: &str| {
        let provided = ["command"].into_iter().collect();
        Config::with_mcp_server_merged(text, &server("fs", command), &provided).unwrap()
    };
    for text in [
        "[[mcp_servers]]\nname = \"fs\"\ncommand = \"fs-mcp\"",
        "# servers\r\n[[mcp_servers]]\r\nname = \"fs\"\r\ncommand = \"fs-mcp\"\r\n\r\n[[mcp_servers]]\r\nname = \"other\"\r\ncommand = \"other-mcp\"\r\n",
        "\u{feff}[[mcp_servers]]\nname = \"fs\"\ncommand = \"fs-mcp\"\n",
    ] {
        assert_eq!(merge(text, "fs-mcp"), text, "unchanged");
        assert_eq!(
            merge(text, "fs-v2"),
            text.replacen("\"fs-mcp\"", "\"fs-v2\"", 1),
            "changed"
        );
    }
    // Layouts toml_edit respaces or reorders stay as written on a no-op.
    for text in [
        "[[ mcp_servers ]]\nname = \"fs\"\ncommand = \"fs-mcp\"\n\n[[mcp_servers]]\nname = \"other\"\ncommand = \"other-mcp\"\n",
        "[[mcp_servers]]\nname = \"fs\"\nenv.A = \"a\"\ncommand = \"fs-mcp\"\nenv.B = \"b\"\n",
    ] {
        assert_eq!(merge(text, "fs-mcp"), text);
    }
    // Adding and removing a server keep the style too.
    let styled =
        "\u{feff}# servers\r\n[[mcp_servers]]\r\nname = \"fs\"\r\ncommand = \"fs-mcp\"\r\n";
    let added = Config::with_mcp_server_added(styled, &server("other", "other-mcp")).unwrap();
    assert!(added.starts_with('\u{feff}'), "{added:?}");
    assert_eq!(added.matches('\n').count(), added.matches("\r\n").count());
    assert_eq!(
        Config::with_mcp_server_removed(&added, "other").unwrap(),
        styled
    );
}

#[test]
fn with_mcp_server_merged_resets_a_changed_transport_to_default_stdio() {
    let text = "[[mcp_servers]]\nname = \"review\"\ntype = \"http\"\nurl = \"https://old.example/mcp\"\nheaders = { Authorization = \"Bearer old\" }\n";
    let incoming: crate::mcp::McpServerEntry =
        toml::from_str("name = \"review\"\ncommand = \"review-mcp\"\n").unwrap();

    let updated = Config::with_mcp_server_merged(text, &incoming, &Default::default()).unwrap();
    let parsed: Config = toml::from_str(&updated).unwrap();
    assert_eq!(
        parsed.mcp_servers[0].transport,
        crate::mcp::TransportKind::Stdio
    );
    assert_eq!(parsed.mcp_servers[0].command.as_deref(), Some("review-mcp"));
    assert!(parsed.mcp_servers[0].url.is_none());
    assert!(parsed.mcp_servers[0].headers.is_empty());
    // #3062 review: the reset is written in place so a comment on it survives.
    assert!(updated.contains("type = \"stdio\""), "{updated}");
}

#[test]
fn with_mcp_server_merged_removes_stdio_fields_when_switching_to_http() {
    let text = "[[mcp_servers]]\nname = \"review\"\ncommand = \"old-mcp\"\nargs = [\"serve\"]\nenv = { TOKEN = \"secret\" }\n";
    let incoming: crate::mcp::McpServerEntry =
        toml::from_str("name = \"review\"\ntype = \"http\"\nurl = \"https://new.example/mcp\"\n")
            .unwrap();

    let updated = Config::with_mcp_server_merged(text, &incoming, &Default::default()).unwrap();
    let parsed: Config = toml::from_str(&updated).unwrap();
    let server = &parsed.mcp_servers[0];
    assert_eq!(server.transport, crate::mcp::TransportKind::Http);
    assert_eq!(server.url.as_deref(), Some("https://new.example/mcp"));
    assert!(server.command.is_none());
    assert!(server.args.is_empty());
    assert!(server.env.is_empty());
}

#[test]
fn with_mcp_server_merged_applies_explicit_empties_and_keeps_omitted_fields() {
    // #2915 review: an explicit empty/default source value is applied; only a
    // key the source omits keeps its local value.
    let text = "[[mcp_servers]]\nname = \"review\"\ncommand = \"review-mcp\"\nargs = [\"--old-target\"]\nenv = { RUST_LOG = \"debug\" }\nenabled = false\nrequest_timeout_secs = 90\n";
    let incoming: crate::mcp::McpServerEntry =
        toml::from_str("name = \"review\"\ncommand = \"review-mcp\"\n").unwrap();

    let provided = ["command", "args", "env", "enabled"].into_iter().collect();
    let updated = Config::with_mcp_server_merged(text, &incoming, &provided).unwrap();
    let server = &toml::from_str::<Config>(&updated).unwrap().mcp_servers[0];
    assert!(server.args.is_empty(), "{updated}");
    assert!(server.env.is_empty(), "{updated}");
    assert!(server.enabled, "{updated}");
    assert_eq!(
        server.request_timeout_secs,
        Some(90),
        "omitted by the source"
    );

    let omitted = Config::with_mcp_server_merged(text, &incoming, &Default::default()).unwrap();
    assert_eq!(omitted, text, "nothing provided, nothing changed");
}

#[test]
fn with_mcp_server_merged_keeps_equal_items_and_comments() {
    // #2915 review: a same-value import is a no-op, even when the local entry
    // restates a default; a changed value keeps the comment on its line.
    let text = "[[mcp_servers]]\nname = \"review\" # identity note\ntype = \"stdio\"\ncommand = 'review-mcp' # operator note\nargs = [ \"--a\" ]\n";
    let same: crate::mcp::McpServerEntry =
        toml::from_str("name = \"review\"\ncommand = \"review-mcp\"\nargs = [\"--a\"]\n").unwrap();
    let provided = ["command", "args"].into_iter().collect();
    assert_eq!(
        Config::with_mcp_server_merged(text, &same, &provided).unwrap(),
        text
    );

    let changed: crate::mcp::McpServerEntry =
        toml::from_str("name = \"review\"\ncommand = \"review-v2\"\nargs = [\"--a\"]\n").unwrap();
    let updated = Config::with_mcp_server_merged(text, &changed, &provided).unwrap();
    assert!(
        updated.contains("name = \"review\" # identity note"),
        "{updated}"
    );
    assert!(
        updated.contains("command = \"review-v2\" # operator note"),
        "{updated}"
    );
    assert!(
        updated.contains("args = [ \"--a\" ]"),
        "equal item untouched: {updated}"
    );
}

#[test]
fn with_mcp_server_merged_updates_a_map_member_by_member() {
    // #3062 review (F10): changing one `env` or `headers` member keeps the
    // map's layout (a sub-table, dotted keys, a multi-line inline table) and
    // every member's comments, instead of rewriting it as one inline table.
    let merge = |text: &str, source: &str, provided: &[&'static str]| {
        let entry: crate::mcp::McpServerEntry =
            toml::from_str(&format!("name = \"fs\"\n{source}")).unwrap();
        let provided = provided.iter().copied().collect();
        Config::with_mcp_server_merged(text, &entry, &provided).unwrap()
    };
    let stdio = "[[mcp_servers]]\nname = \"fs\"\ncommand = \"fs-mcp\"\n";
    let env = "command = \"fs-mcp\"\nenv = { A = \"${SYNTHETIC_A}\", B = \"${SYNTHETIC_B2}\" }\n";
    for map in [
        "\n[mcp_servers.env]\n# staging token\nA = \"${SYNTHETIC_A}\" # keep A\nB = \"${SYNTHETIC_B}\" # b note\n",
        "env.A = \"${SYNTHETIC_A}\" # keep A\nenv.B = \"${SYNTHETIC_B}\" # b note\n",
        "env = {\n  A = \"${SYNTHETIC_A}\", # keep A\n  B = \"${SYNTHETIC_B}\", # b note\n}\n",
    ] {
        let text = format!("{stdio}{map}");
        assert_eq!(
            merge(&text, env, &["command", "env"]),
            text.replace("SYNTHETIC_B}", "SYNTHETIC_B2}")
        );
    }

    // A member the source drops goes, a new one is added, the rest stay.
    let text = format!("{stdio}\n[mcp_servers.env]\n# staging token\nA = \"${{SYNTHETIC_A}}\" # keep A\nB = \"${{SYNTHETIC_B}}\"\n");
    let reshaped = merge(
        &text,
        "command = \"fs-mcp\"\nenv = { A = \"${SYNTHETIC_A}\", C = \"${SYNTHETIC_C}\" }\n",
        &["command", "env"],
    );
    assert!(
        reshaped.contains("[mcp_servers.env]\n# staging token\nA = \"${SYNTHETIC_A}\" # keep A\n"),
        "{reshaped}"
    );
    assert_eq!(
        entry_of(&reshaped).env.keys().collect::<Vec<_>>(),
        ["A", "C"]
    );

    let http = "[[mcp_servers]]\nname = \"fs\"\ntype = \"http\"\nurl = \"https://mcp.example.test/mcp\"\n\n[mcp_servers.headers]\nX-Tenant = \"tenant-a\" # tenant note\nAuthorization = \"Bearer ${SYNTHETIC_TOKEN}\"\n";
    assert_eq!(
        merge(
            http,
            "type = \"http\"\nurl = \"https://mcp.example.test/mcp\"\nheaders = { X-Tenant = \"tenant-a\", Authorization = \"Bearer ${SYNTHETIC_TOKEN2}\" }\n",
            &["type", "url", "headers"],
        ),
        http.replace("SYNTHETIC_TOKEN}", "SYNTHETIC_TOKEN2}")
    );
}

#[test]
fn with_mcp_server_merged_refuses_to_carry_headers_to_a_moved_endpoint() {
    // #2915 review (P1): a new URL must not inherit the credential configured
    // for the old destination, and the refusal must not quote either.
    let text = "[[mcp_servers]]\nname = \"review\"\ntype = \"http\"\nurl = \"https://old.example/mcp\"\nheaders = { Authorization = \"Bearer ${PRIVATE_MARKER}\" }\n";
    let at = |url: &str| -> crate::mcp::McpServerEntry {
        toml::from_str(&format!(
            "name = \"review\"\ntype = \"http\"\nurl = \"{url}\"\n"
        ))
        .unwrap()
    };
    let provided: std::collections::BTreeSet<&str> = ["type", "url"].into_iter().collect();

    let error = Config::with_mcp_server_merged(text, &at("https://new.example/mcp"), &provided)
        .unwrap_err()
        .to_string();
    assert!(error.contains("refusing to send them"), "{error}");
    assert!(
        !error.contains("PRIVATE_MARKER") && !error.contains("example"),
        "{error}"
    );

    // The same endpoint, respelled, keeps its headers.
    let respelled =
        Config::with_mcp_server_merged(text, &at("https://OLD.example/mcp"), &provided).unwrap();
    assert!(respelled.contains("PRIVATE_MARKER"), "{respelled}");

    // Headers the source spells out (here: none) replace the local ones.
    let with_headers = provided.iter().copied().chain(["headers"]).collect();
    let moved = Config::with_mcp_server_merged(text, &at("https://new.example/mcp"), &with_headers)
        .unwrap();
    assert!(!moved.contains("PRIVATE_MARKER"), "{moved}");
}

#[test]
fn with_mcp_server_merged_keeps_the_operators_spelling_of_one_endpoint() {
    // #3062 review (F6): `https://h` and `https://h/` are one request target,
    // but a saved login is bound to the exact spelling. An import that only
    // canonicalises the URL leaves the operator's spelling, and the login.
    let text =
        "[[mcp_servers]]\nname = \"fs\"\ntype = \"http\"\nurl = \"https://api.example.test\"\n";
    let at = |url: &str| -> crate::mcp::McpServerEntry {
        toml::from_str(&format!(
            "name = \"fs\"\ntype = \"http\"\nurl = \"{url}\"\n"
        ))
        .unwrap()
    };
    let provided = ["type", "url"].into_iter().collect();
    assert_eq!(
        Config::with_mcp_server_merged(text, &at("https://api.example.test/"), &provided).unwrap(),
        text
    );
    // A real resource change is still written.
    let moved =
        Config::with_mcp_server_merged(text, &at("https://api.example.test/mcp"), &provided)
            .unwrap();
    assert!(
        moved.contains("url = \"https://api.example.test/mcp\""),
        "{moved}"
    );
}

#[test]
fn with_mcp_server_merged_refuses_to_carry_env_to_a_different_program() {
    // #2915: the stdio twin of the header transfer. The program is the argv,
    // so a launcher running other args counts as a different program too.
    let text = "[[mcp_servers]]\nname = \"review\"\ncommand = \"npx\"\nargs = [\"old-mcp\"]\nenv = { API_TOKEN = { env = \"PRIVATE_MARKER\" } }\n";
    let runs = |argv: &str| -> crate::mcp::McpServerEntry {
        toml::from_str(&format!("name = \"review\"\n{argv}\n")).unwrap()
    };
    let with = |keys: &[&'static str]| {
        keys.iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
    };

    for (argv, provided) in [
        ("command = \"new-mcp\"", with(&["command"])),
        (
            "command = \"npx\"\nargs = [\"new-mcp\"]",
            with(&["command", "args"]),
        ),
    ] {
        let error = Config::with_mcp_server_merged(text, &runs(argv), &provided)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("refusing to send them to the new command"),
            "{error}"
        );
        assert!(
            !error.contains("PRIVATE_MARKER") && !error.contains("API_TOKEN"),
            "{error}"
        );
    }

    // The same program keeps its env; an env the source supplies replaces it.
    // (The source supplies args too: the old program's argv is not carried
    // either, see the args twin below.)
    let same =
        Config::with_mcp_server_merged(text, &runs("command = \"npx\""), &with(&["command"]))
            .unwrap();
    assert_eq!(same, text);
    let supplied = runs("command = \"new-mcp\"");
    let moved = Config::with_mcp_server_merged(text, &supplied, &with(&["command", "args", "env"]))
        .unwrap();
    assert!(!moved.contains("PRIVATE_MARKER"), "{moved}");
}

#[test]
fn with_mcp_server_merged_refuses_to_carry_args_to_a_different_program() {
    // #3062 review (F1): argv written for one program, a credential flag
    // included, must not follow a changed command the source names without
    // args. Any kept argument refuses: newt cannot tell a credential flag it
    // does not know from any other.
    let text = "[[mcp_servers]]\nname = \"review\"\ncommand = \"npx\"\nargs = [\"--api-key\", \"SYNTHETIC_SECRET_MARKER\"]\n";
    let runs = |argv: &str| -> crate::mcp::McpServerEntry {
        toml::from_str(&format!("name = \"review\"\n{argv}\n")).unwrap()
    };
    let with = |keys: &[&'static str]| keys.iter().copied().collect();

    let error = Config::with_mcp_server_merged(
        text,
        &runs("command = \"other-vendor-mcp\""),
        &with(&["command"]),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("2 local argument(s)"), "{error}");
    assert!(
        error.contains("refusing to send them to the new command"),
        "{error}"
    );
    assert!(
        !error.contains("SYNTHETIC_SECRET_MARKER") && !error.contains("--api-key"),
        "{error}"
    );

    // Args the source supplies are not a transfer, and the same program keeps
    // its own.
    let supplied = Config::with_mcp_server_merged(
        text,
        &runs("command = \"other-vendor-mcp\"\nargs = [\"--serve\"]"),
        &with(&["command", "args"]),
    )
    .unwrap();
    assert_eq!(entry_of(&supplied).args, ["--serve"]);
    let same =
        Config::with_mcp_server_merged(text, &runs("command = \"npx\""), &with(&["command"]))
            .unwrap();
    assert_eq!(same, text);
}

#[test]
fn with_mcp_server_merged_keeps_comments_on_defaults_and_resets() {
    // #3062 review: a value reset to its default keeps the comment on its
    // line, and an unchanged default-valued key is untouched even when another
    // key in the same entry changes.
    let merge = |text: &str, source: &str, provided: &[&'static str]| {
        let entry: crate::mcp::McpServerEntry =
            toml::from_str(&format!("name = \"review\"\n{source}")).unwrap();
        let provided = provided.iter().copied().collect();
        Config::with_mcp_server_merged(text, &entry, &provided).unwrap()
    };
    let head = "[[mcp_servers]]\nname = \"review\"\ncommand = \"review-mcp\"\n";

    let cleared = merge(
        &format!("{head}args = [\"--old-target\"] # argument policy\n"),
        "command = \"review-mcp\"\n",
        &["command", "args"],
    );
    assert!(cleared.contains("args = [] # argument policy"), "{cleared}");

    let enabled = merge(
        &format!("{head}enabled = false # operator policy\n"),
        "command = \"review-mcp\"\n",
        &["command", "enabled"],
    );
    assert!(
        enabled.contains("enabled = true # operator policy"),
        "{enabled}"
    );

    let mixed = merge(
        &format!("{head}enabled = true # enabled policy\ntype = \"stdio\" # transport policy\nrequest_timeout_secs = 90\n"),
        "command = \"review-mcp\"\nrequest_timeout_secs = 30\n",
        &["command", "enabled", "request_timeout_secs"],
    );
    assert!(mixed.contains("enabled = true # enabled policy"), "{mixed}");
    assert!(
        mixed.contains("type = \"stdio\" # transport policy"),
        "{mixed}"
    );
    assert!(mixed.contains("request_timeout_secs = 30"), "{mixed}");
}

/// Every name serde accepts for an `McpServerEntry` field, aliases included,
/// read from the derive itself so a new alias is covered without a list.
fn accepted_mcp_field_names() -> &'static [&'static str] {
    use serde::de::{Error, Visitor};
    struct Names(std::cell::Cell<&'static [&'static str]>);
    impl<'de> serde::Deserializer<'de> for &Names {
        type Error = serde::de::value::Error;
        fn deserialize_any<V: Visitor<'de>>(
            self,
            _: V,
        ) -> std::result::Result<V::Value, Self::Error> {
            Err(Error::custom("field names only"))
        }
        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            _: V,
        ) -> std::result::Result<V::Value, Self::Error> {
            self.0.set(fields);
            Err(Error::custom("field names only"))
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map enum identifier ignored_any
        }
    }
    let names = Names(std::cell::Cell::new(&[]));
    let _ = <crate::mcp::McpServerEntry as serde::Deserialize>::deserialize(&names);
    names.0.get()
}

#[test]
fn with_mcp_server_merged_updates_an_alias_spelled_field_in_place() {
    // #3062 review: `requestTimeoutSecs` is the same field as
    // `request_timeout_secs`. A force import must update it where the
    // operator wrote it, never add a second spelling the reader rejects.
    let names = accepted_mcp_field_names();
    assert!(names.contains(&"requestTimeoutSecs"), "{names:?}");
    let samples = [
        "30",
        "90",
        "'a'",
        "'b'",
        "false",
        "['a']",
        "['b']",
        "{ A = 'a' }",
        "{ B = 'b' }",
    ];
    let entry = |body: &str| -> Option<crate::mcp::McpServerEntry> {
        toml::from_str(&format!("name = 'review'\ncommand = 'review-mcp'\n{body}")).ok()
    };
    let base = entry("").unwrap();
    let mut aliases = 0;
    for name in names {
        // Values this spelling accepts, and the key newt itself would write.
        let accepted: Vec<(&str, String)> = samples
            .iter()
            .filter_map(|value| {
                let parsed = entry(&format!("{name} = {value}")).filter(|e| *e != base)?;
                let rendered = crate::mcp::entry_to_toml_table(&parsed, None).ok()?;
                let mut keys = rendered
                    .iter()
                    .map(|(key, _)| key)
                    .filter(|key| !["name", "command"].contains(key));
                Some((*value, keys.next()?.to_owned()))
            })
            .collect();
        let [(old, canonical), (new, _), ..] = accepted.as_slice() else {
            continue;
        };
        if canonical == name {
            continue;
        }
        aliases += 1;
        let text = format!(
            "[[mcp_servers]]\nname = 'review'\ncommand = 'review-mcp'\n{name} = {old} # note\n"
        );
        let provided = [canonical.as_str()].into_iter().collect();
        let merge = |incoming: &crate::mcp::McpServerEntry| {
            let merged = Config::with_mcp_server_merged(&text, incoming, &provided).unwrap();
            assert!(
                crate::mcp::parse_newt_mcp_toml_checked(&merged).is_ok(),
                "{name}: {merged}"
            );
            assert!(
                !merged.contains(&format!("{canonical} =")),
                "{name}: {merged}"
            );
            merged
        };
        let changed = merge(&entry(&format!("{name} = {new}")).unwrap());
        assert!(
            changed.contains(&format!("{name} = {new} # note")),
            "{changed}"
        );
        let same = merge(&entry(&format!("{name} = {old}")).unwrap());
        assert_eq!(same, text, "{name}: an unchanged value keeps its line");
        let reset = merge(&base);
        assert_eq!(entry_of(&reset), base, "{name}: reset to the default");
    }
    assert!(aliases > 0, "no alias found among {names:?}");
}

fn entry_of(text: &str) -> crate::mcp::McpServerEntry {
    crate::mcp::parse_newt_mcp_toml_checked(text)
        .unwrap()
        .remove(0)
}

#[test]
fn with_mcp_server_removed_does_not_echo_malformed_config_contents() {
    let text = "[[mcp_servers]]\nname = \"private-marker\"\ncommand = =\n";
    let error = Config::with_mcp_server_removed(text, "private-marker").unwrap_err();
    assert!(error.to_string().contains("invalid MCP TOML syntax"));
    assert!(!error.to_string().contains("private-marker"));
}

#[test]
#[serial_test::serial(real_fs)]
fn set_mcp_server_enabled_in_file_uses_atomic_writeback() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    let dir = tempfile::tempdir().unwrap();
    let _sandbox = HomeSandbox::enter(dir.path());
    let path = dir.path().join("mcp.toml");
    let original =
        "# keep this comment\n[[mcp_servers]]\nname = \"docs\"\ncommand = \"docs-mcp\"\n";
    std::fs::write(&path, original).unwrap();

    Config::set_mcp_server_enabled_in_file(&path, "docs", false).unwrap();

    let updated = std::fs::read_to_string(&path).unwrap();
    assert!(updated.contains("# keep this comment"));
    assert!(updated.contains("enabled = false"));
    assert!(!newt_core_lock_path(&path).exists());
}

/// Grounds the owner rule on real files (#3062 review, F11): the owner is
/// the file that supplies the effective entry, so a same-name entry that
/// discovery drops as invalid owns the name only when no valid one exists.
#[test]
#[ignore = "real-resource: weekly/release tier; touches the filesystem"]
#[serial_test::serial(real_fs)]
fn mcp_server_owner_path_skips_an_entry_discovery_drops() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    let dir = tempfile::tempdir().unwrap();
    let _sandbox = HomeSandbox::enter(dir.path());
    let config = dir.path().join("config.toml");
    let mcp = dir.path().join("mcp.toml");
    // An http entry with its url commented out: discovery drops it.
    let dead = "[[mcp_servers]]\nname = \"docs\"\ntype = \"http\"\n# url = \"https://docs.example.test/mcp\"\n";
    std::fs::write(&config, dead).unwrap();
    std::fs::write(
        &mcp,
        "[[mcp_servers]]\nname = \"docs\"\ncommand = \"docs-mcp\"\n",
    )
    .unwrap();

    assert_eq!(
        Config::mcp_server_owner_path("docs").unwrap().as_ref(),
        Some(&mcp)
    );
    Config::set_mcp_server_enabled_in_file(&mcp, "docs", false).unwrap();
    assert!(std::fs::read_to_string(&mcp)
        .unwrap()
        .contains("enabled = false"));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), dead);

    // With no valid `docs` left, the broken one still owns the name, so
    // `newt mcp remove` can delete it.
    std::fs::write(&mcp, "").unwrap();
    assert_eq!(Config::mcp_server_owner_path("docs").unwrap(), Some(config));
}

fn newt_core_lock_path(path: &std::path::Path) -> std::path::PathBuf {
    crate::atomic_fs::ResolvedPath::resolve(path)
        .unwrap()
        .lock_path()
}

// ---- `newt mcp add|remove` comment-preserving config writers ----

#[test]
fn with_mcp_server_added_appends_and_preserves_comments() {
    let text = "\
# hand-authored config
default_backend = \"local\" # keep me

[[mcp_servers]]
name = \"modulex\"
command = \"modulex-mcp\"
";
    let entry = crate::mcp::McpServerEntry {
        name: "scrybe".into(),
        enabled: true,
        transport: crate::mcp::TransportKind::Stdio,
        command: Some("scrybe-mcp-server".into()),
        args: vec!["stdio".into()],
        env: std::collections::BTreeMap::from([(
            "SCRYBE_LOG".to_string(),
            crate::mcp::SecretValue::literal("info"),
        )]),
        url: None,
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: Some(120),
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: crate::mcp::McpTrust::Trusted,
        origin: None,
    };
    let out = Config::with_mcp_server_added(text, &entry).unwrap();
    assert!(
        out.contains("# hand-authored config"),
        "comment lost: {out}"
    );
    assert!(out.contains("# keep me"), "inline comment lost: {out}");
    assert!(out.contains("modulex-mcp"), "existing entry lost: {out}");
    // Round-trips through the typed config with both entries intact.
    let cfg: Config = toml::from_str(&out).unwrap();
    assert_eq!(cfg.mcp_servers.len(), 2);
    let added = cfg.mcp_servers.iter().find(|s| s.name == "scrybe").unwrap();
    assert_eq!(added.command.as_deref(), Some("scrybe-mcp-server"));
    assert_eq!(added.args, vec!["stdio"]);
    assert_eq!(
        added
            .env
            .get("SCRYBE_LOG")
            .and_then(crate::mcp::SecretValue::as_literal),
        Some("info")
    );
    assert_eq!(added.request_timeout_secs, Some(120));
    assert!(added.enabled);
    // Defaults stay implicit — the file stays minimal.
    assert!(!out.contains("enabled"), "default enabled written: {out}");
    assert!(!out.contains("type"), "default transport written: {out}");
}

#[test]
fn with_mcp_server_added_creates_section_in_empty_text() {
    let entry = crate::mcp::McpServerEntry {
        name: "fs".into(),
        enabled: true,
        transport: crate::mcp::TransportKind::Stdio,
        command: Some("mcp-fs".into()),
        args: vec![],
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: crate::mcp::McpTrust::Trusted,
        origin: None,
    };
    let out = Config::with_mcp_server_added("", &entry).unwrap();
    let cfg: Config = toml::from_str(&out).unwrap();
    assert_eq!(cfg.mcp_servers.len(), 1);
    assert_eq!(cfg.mcp_servers[0].name, "fs");
    assert_eq!(cfg.mcp_servers[0].command.as_deref(), Some("mcp-fs"));
}

#[test]
fn with_mcp_server_added_writes_sse_transport_and_url() {
    let entry = crate::mcp::McpServerEntry {
        name: "remote".into(),
        enabled: true,
        transport: crate::mcp::TransportKind::Sse,
        command: None,
        args: vec![],
        env: std::collections::BTreeMap::new(),
        url: Some("https://mcp.example/sse".into()),
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: crate::mcp::McpTrust::Trusted,
        origin: None,
    };
    let out = Config::with_mcp_server_added("", &entry).unwrap();
    let cfg: Config = toml::from_str(&out).unwrap();
    assert_eq!(cfg.mcp_servers[0].transport, crate::mcp::TransportKind::Sse);
    assert_eq!(
        cfg.mcp_servers[0].url.as_deref(),
        Some("https://mcp.example/sse")
    );
}

#[test]
fn with_mcp_server_added_rejects_duplicates_and_invalid_entries() {
    let text = "[[mcp_servers]]\nname = \"scrybe\"\ncommand = \"scrybe-mcp-server\"\n";
    let dup = crate::mcp::McpServerEntry {
        name: "scrybe".into(),
        enabled: true,
        transport: crate::mcp::TransportKind::Stdio,
        command: Some("other".into()),
        args: vec![],
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: crate::mcp::McpTrust::Trusted,
        origin: None,
    };
    let err = Config::with_mcp_server_added(text, &dup).unwrap_err();
    assert!(err.to_string().contains("scrybe"), "names the dup: {err}");

    // A stdio entry with no command / an sse entry with no url never lands
    // in the file — it could never connect (mcp::McpServerEntry::is_valid).
    let no_cmd = crate::mcp::McpServerEntry {
        name: "ghost".into(),
        command: None,
        ..dup.clone()
    };
    assert!(Config::with_mcp_server_added("", &no_cmd).is_err());
    let no_url = crate::mcp::McpServerEntry {
        name: "ghost".into(),
        transport: crate::mcp::TransportKind::Http,
        command: None,
        ..dup.clone()
    };
    assert!(Config::with_mcp_server_added("", &no_url).is_err());
    // An empty name can never be addressed again — reject it.
    let unnamed = crate::mcp::McpServerEntry {
        name: "  ".into(),
        ..dup.clone()
    };
    assert!(Config::with_mcp_server_added("", &unnamed).is_err());
}

#[test]
fn with_mcp_server_removed_deletes_only_the_named_entry() {
    let text = "\
# my config

[[mcp_servers]]
name = \"keep\"
command = \"keep-mcp\" # keep note

[[mcp_servers]]
name = \"drop\"
command = \"drop-mcp\"
";
    let out = Config::with_mcp_server_removed(text, "drop").unwrap();
    assert!(out.contains("# my config"), "comment lost: {out}");
    assert!(out.contains("# keep note"), "inline comment lost: {out}");
    let cfg: Config = toml::from_str(&out).unwrap();
    assert_eq!(cfg.mcp_servers.len(), 1);
    assert_eq!(cfg.mcp_servers[0].name, "keep");
    assert!(!out.contains("drop-mcp"));
}

#[test]
fn with_mcp_server_removed_reports_a_non_array_section_accurately() {
    // The inline-array form is valid TOML the serde reader accepts; the
    // writer must say it cannot edit that shape, not falsely claim the
    // entry is absent.
    let text = "mcp_servers = [ { name = \"x\", command = \"y\" } ]\n";
    let err = Config::with_mcp_server_removed(text, "x").unwrap_err();
    assert!(
        err.to_string().contains("not an array of tables"),
        "wrong-shape section misreported: {err}"
    );
    let err = Config::with_mcp_server_removed("mcp_servers = 3\n", "x").unwrap_err();
    assert!(
        err.to_string().contains("not an array of tables"),
        "scalar section misreported: {err}"
    );
}

#[test]
fn mcp_writer_error_branches_are_loud() {
    let entry = crate::mcp::McpServerEntry {
        name: "x".into(),
        enabled: true,
        transport: crate::mcp::TransportKind::Stdio,
        command: Some("x-mcp".into()),
        args: vec![],
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: crate::mcp::McpTrust::Trusted,
        origin: None,
    };
    // Invalid TOML input text.
    let err = Config::with_mcp_server_added("not toml [", &entry).unwrap_err();
    assert!(err.to_string().contains("invalid MCP TOML syntax"), "{err}");
    let err = Config::with_mcp_server_removed("not toml [", "x").unwrap_err();
    assert!(err.to_string().contains("invalid MCP TOML syntax"), "{err}");
    // A section that is not an array of tables.
    let err = Config::with_mcp_server_added("mcp_servers = 3\n", &entry).unwrap_err();
    assert!(err.to_string().contains("not an array of tables"), "{err}");
    // A timeout that does not fit TOML's i64 integers.
    let oversized = crate::mcp::McpServerEntry {
        login_argv: Vec::new(),
        request_timeout_secs: Some(u64::MAX),
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        ..entry
    };
    let err = Config::with_mcp_server_added("", &oversized).unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
}

#[test]
fn with_mcp_server_removed_errors_when_absent() {
    let text = "[[mcp_servers]]\nname = \"present\"\ncommand = \"x\"\n";
    let err = Config::with_mcp_server_removed(text, "ghost").unwrap_err();
    assert!(err.to_string().contains("ghost"), "names the miss: {err}");
    // No section at all errors the same way, not a panic.
    assert!(Config::with_mcp_server_removed("", "ghost").is_err());
}
