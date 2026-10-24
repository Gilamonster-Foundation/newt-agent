use super::*;

fn pairs(kvs: &[(&str, &str)]) -> Vec<(String, String)> {
    kvs.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
fn map(kvs: &[(&str, &str)]) -> BTreeMap<String, String> {
    kvs.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn merges_all_three_sources() {
    let got = assemble_env_grants(
        &pairs(&[("PATH", "/usr/bin")]),
        &map(&[("GITHUB_TOKEN", "tok")]),
        &map(&[("MODULEX_STORE", "/s")]),
    );
    assert_eq!(
        got,
        pairs(&[
            ("GITHUB_TOKEN", "tok"),
            ("MODULEX_STORE", "/s"),
            ("PATH", "/usr/bin"),
        ]),
        "all sources present, deterministic (BTreeMap) key order"
    );
}

#[test]
fn precedence_is_passthrough_then_shell_env_then_entry() {
    // Same key in all three: the entry wins, then shell-env, then passthrough.
    let got = assemble_env_grants(
        &pairs(&[("K", "from_passthrough"), ("P", "keep")]),
        &map(&[("K", "from_shell_env")]),
        &map(&[("K", "from_entry")]),
    );
    assert_eq!(
        got,
        pairs(&[("K", "from_entry"), ("P", "keep")]),
        "entry.env overrides shell-env overrides passthrough; unshared keys survive"
    );
}

#[test]
fn shell_env_overrides_passthrough_when_entry_absent() {
    let got = assemble_env_grants(
        &pairs(&[("K", "from_passthrough")]),
        &map(&[("K", "from_shell_env")]),
        &BTreeMap::new(),
    );
    assert_eq!(got, pairs(&[("K", "from_shell_env")]));
}

#[test]
fn empty_sources_yield_no_grants() {
    assert!(
        assemble_env_grants(&[], &BTreeMap::new(), &BTreeMap::new()).is_empty(),
        "a confined child with nothing granted starts env-EMPTY"
    );
}

#[test]
fn timezone_passthrough_preserves_empty_and_server_override() {
    assert!(newt_core::mcp_stdio_env_passthrough().contains(&"TZ"));
    for timezone in ["America/New_York", "UTC-14", ""] {
        assert_eq!(
            assemble_env_grants(
                &pairs(&[("TZ", timezone)]),
                &BTreeMap::new(),
                &BTreeMap::new(),
            ),
            pairs(&[("TZ", timezone)])
        );
    }
    assert_eq!(
        assemble_env_grants(
            &pairs(&[("TZ", "America/New_York")]),
            &map(&[("TZ", "UTC-14")]),
            &map(&[("TZ", "")]),
        ),
        pairs(&[("TZ", "")]),
        "an explicit server UTC setting must override the parent"
    );
}

fn parent_vars() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    [
        ("HOME", "/h"),
        ("PATH", "/usr/bin"),
        ("DISPLAY", ":0"),
        ("WAYLAND_DISPLAY", "wayland-0"),
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ("OPENAI_API_KEY", "sk-secret"),
        ("NEWT_AGENT_KEY", "newt-secret"),
        ("NEWT_AMBIENT", "1"),
    ]
    .map(|(k, v)| (k.into(), v.into()))
    .to_vec()
}

/// #978: outside `--ambient` the passthrough is the closed allow-list, unchanged: no
/// display variable and no secret reaches a stdio server.
#[test]
fn the_default_passthrough_is_the_closed_allow_list() {
    let got = parent_passthrough(false, parent_vars());
    assert_eq!(got, pairs(&[("HOME", "/h"), ("PATH", "/usr/bin")]));
}

/// #978: under `--ambient` a stdio server gets the full environment (so a browser launch
/// finds the display and the session bus), minus newt's own control plane.
#[test]
fn the_ambient_passthrough_is_the_full_env_minus_the_control_plane() {
    let got: BTreeMap<String, String> = parent_passthrough(true, parent_vars())
        .into_iter()
        .collect();
    for key in [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
        "HOME",
        "PATH",
    ] {
        assert!(
            got.contains_key(key),
            "{key} must reach an ambient stdio server"
        );
    }
    assert!(
        got.contains_key("OPENAI_API_KEY"),
        "the operator's own environment is the grant"
    );
    assert!(
        !got.contains_key("NEWT_AGENT_KEY"),
        "newt's secrets never are"
    );
}

/// Windows names are case-insensitive: mixed-case copies of newt's secrets are still
/// withheld under `--ambient`, and a `Path` still matches the `PATH` allow-list entry.
#[test]
fn windows_key_case_is_honored_in_both_passthroughs() {
    let vars = || -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        [
            ("Newt_Agent_Key", "s1"),
            ("newt_operator_key", "s2"),
            ("Newt_Token_Passphrase", "s3"),
            ("Newt_Ambient", "1"),
            ("Path", "C:\\bin"),
            ("DisplayThing", "x"),
        ]
        .map(|(k, v)| (k.into(), v.into()))
        .to_vec()
    };
    let ambient: BTreeMap<String, String> = parent_passthrough_with(true, vars(), true)
        .into_iter()
        .collect();
    assert_eq!(
        ambient.keys().map(String::as_str).collect::<Vec<_>>(),
        ["DisplayThing", "Path"],
        "mixed-case control-plane names are stripped"
    );
    let default: BTreeMap<String, String> = parent_passthrough_with(false, vars(), true)
        .into_iter()
        .collect();
    assert_eq!(
        default.keys().map(String::as_str).collect::<Vec<_>>(),
        ["Path"],
        "Path is PATH on Windows"
    );
    // Elsewhere names are exact: `Path` is not `PATH`, and the odd-case secrets are other variables.
    let exact = parent_passthrough_with(false, vars(), false);
    assert!(exact.is_empty(), "{exact:?}");
}

/// Windows names are case-insensitive: an inherited `Path`, a shell-env `path` and an explicit
/// server `PATH` are one variable, and the server entry wins (its spelling too), so the
/// grants the Windows spawn applies cannot let the parent's value overwrite the configured one.
#[test]
fn mixed_case_collisions_across_layers_resolve_to_the_entry() {
    let parent = pairs(&[("Path", "parent-bin"), ("HOME", "/h")]);
    let shell = BTreeMap::from([("path".to_string(), "shell-bin".to_string())]);
    let entry = BTreeMap::from([("PATH".to_string(), "entry-bin".to_string())]);
    let win = assemble_env_grants_with(&parent, &shell, &entry, true);
    assert_eq!(win, pairs(&[("HOME", "/h"), ("PATH", "entry-bin")]));
    // Without an entry value the shell-env layer beats the parent; with neither it is the parent's.
    let win = assemble_env_grants_with(&parent, &shell, &BTreeMap::new(), true);
    assert_eq!(win, pairs(&[("HOME", "/h"), ("path", "shell-bin")]));
    let win = assemble_env_grants_with(&parent, &BTreeMap::new(), &BTreeMap::new(), true);
    assert_eq!(win, pairs(&[("HOME", "/h"), ("Path", "parent-bin")]));
    // Exact names elsewhere: three different variables.
    let exact = assemble_env_grants_with(&parent, &shell, &entry, false);
    assert_eq!(exact.len(), 4, "{exact:?}");
}
