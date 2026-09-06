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
