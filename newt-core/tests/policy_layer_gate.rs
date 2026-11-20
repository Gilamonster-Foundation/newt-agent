//! The policy layer must not depend on the agent loop, the conversation store
//! or the config (#3140). These modules are the cut a policy crate takes: they
//! decide what is allowed, so they may use each other and the standard
//! library, and nothing that runs a turn, stores a conversation or reads a
//! user's settings. A new `crate::agentic`, `crate::store` or `crate::config`
//! path in one of them fails here, in the change that adds it.
//!
//! It reads each module's production code from source: everything before the
//! first top-level `#[cfg(test)]`, without comment lines.

use std::path::Path;

/// The policy modules, by file name under `src/`. A module missing from the
/// disk fails the test, so a rename cannot quietly drop it from the gate.
const POLICY_MODULES: &[&str] = &[
    "caveats",
    "danger",
    "fs_aliases",
    "git_caveats",
    "netguard",
    "paths",
    "permission_challenge",
    "permissions",
    "posture",
    "redact",
];

/// The layers a policy module must not name.
const FORBIDDEN: &[&str] = &["agentic", "store", "config"];

fn production_code(src: &str) -> String {
    let mut out = String::new();
    for line in src.lines() {
        if line.starts_with("#[cfg(test)]") {
            break;
        }
        if line.trim_start().starts_with("//") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The forbidden layers `src` names through `crate::<layer>` or
/// `crate::{<layer>, ..}`, in its production code.
fn violations(src: &str) -> Vec<String> {
    let code = production_code(src);
    let mut found = Vec::new();
    for layer in FORBIDDEN {
        let direct = format!("crate::{layer}");
        let mut rest = code.as_str();
        while let Some(i) = rest.find(&direct) {
            let after = &rest[i + direct.len()..];
            if !after.chars().next().is_some_and(is_ident_char) {
                found.push(direct.clone());
                break;
            }
            rest = after;
        }
        // `use crate::{agentic, ..};` names the layer without the `crate::agentic` text.
        let mut rest = code.as_str();
        while let Some(i) = rest.find("crate::{") {
            let after = &rest[i + "crate::{".len()..];
            let group = after.split('}').next().unwrap_or("");
            if group
                .split(|c: char| !is_ident_char(c))
                .any(|word| word == *layer)
            {
                found.push(format!("crate::{{{layer}}}"));
                break;
            }
            rest = after;
        }
    }
    found
}

#[test]
fn the_policy_layer_names_no_agent_store_or_config_path() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut bad = Vec::new();
    for module in POLICY_MODULES {
        let path = src_dir.join(format!("{module}.rs"));
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("policy module {module} not found at {path:?}: {e}"));
        for v in violations(&src) {
            bad.push(format!("{module}.rs names {v}"));
        }
    }
    assert!(
        bad.is_empty(),
        "the policy layer must not depend on agentic, store or config:\n{}",
        bad.join("\n")
    );
}

/// Anti-vacuous twin: the scan flags each way of naming a layer and ignores
/// what it should, so a passing gate is not a scan that sees nothing.
#[test]
fn the_scan_flags_a_layer_and_ignores_tests_comments_and_neighbours() {
    assert_eq!(
        violations("use crate::agentic::Verdict;\n"),
        ["crate::agentic"]
    );
    assert_eq!(
        violations("let x = crate::store::open();\n"),
        ["crate::store"]
    );
    assert_eq!(
        violations("use crate::{caveats, config};\n"),
        ["crate::{config}"]
    );
    // A neighbouring name, a comment and test-only code are not violations.
    assert!(violations("use crate::config_tests::x;\n").is_empty());
    assert!(violations("// use crate::agentic::Verdict;\nfn f() {}\n").is_empty());
    assert!(violations("fn f() {}\n#[cfg(test)]\nmod t { use crate::agentic::X; }\n").is_empty());
    assert!(violations("use crate::caveats::Caveats;\n").is_empty());
}
