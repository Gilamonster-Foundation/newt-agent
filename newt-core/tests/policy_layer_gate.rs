//! The policy layer must not depend on the agent loop, the conversation store
//! or the config (#3140). These modules are the cut a policy crate takes: they
//! decide what is allowed, so they may use each other and the standard
//! library, and nothing that runs a turn, stores a conversation or reads a
//! user's settings. A new `crate::agentic`, `crate::store` or `crate::config`
//! path in one of them fails here, in the change that adds it.
//!
//! A module can name a layer's item without naming the layer: `lib.rs`
//! re-exports items from all three at the crate root (`pub use config::{Config,
//! ..}`), so `crate::Config` is the config layer too. The gate reads those
//! re-exports from `lib.rs` and forbids each name as well.
//!
//! It reads each module's production code from source: everything before the
//! first test-gated item (`#[cfg(test)]` or `#[cfg(all(test`), without comment
//! lines.

use std::collections::BTreeSet;
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
const LAYERS: &[&str] = &["agentic", "store", "config"];

/// Violations that exist today, as `(module, path)`. This is a ratchet: the
/// list must match what the scan finds, so a new violation fails and a fixed
/// one must be deleted here. It may only shrink.
///
/// `posture`: `build_posture` resolves a `/posture` name against
/// `Config.modes` and `Config.permission_presets`. That is config resolution,
/// not policy. Moving it means changing its signature to take config types,
/// which is a rewrite, so it stays until the policy crate step decides where
/// those types and that resolver live.
const KNOWN_VIOLATIONS: &[(&str, &str)] = &[("posture", "crate::Config")];

fn production_code(src: &str) -> String {
    let mut out = String::new();
    for line in src.lines() {
        if line.starts_with("#[cfg(test)]") || line.starts_with("#[cfg(all(test") {
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

/// The first identifier in `s`, if it starts one.
fn first_ident(s: &str) -> &str {
    let s = s.trim_start();
    let end = s.find(|c: char| !is_ident_char(c)).unwrap_or(s.len());
    &s[..end]
}

/// The depth-1 members of the `{..}` group that opens at `s[0]`, as their
/// first identifier each: `{a, b::{c}, d}` gives `a`, `b`, `d`.
fn group_heads(s: &str) -> Vec<String> {
    let mut heads = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '{' => {
                depth += 1;
                if depth == 1 {
                    start = i + 1;
                }
            }
            '}' | ',' if depth == 1 => {
                let head = first_ident(&s[start..i]);
                if !head.is_empty() {
                    heads.push(head.to_string());
                }
                start = i + 1;
                if c == '}' {
                    break;
                }
            }
            '}' => depth -= 1,
            _ => {}
        }
    }
    heads
}

/// The root names `lib.rs` re-exports from the forbidden layers: every item of
/// a `pub use <layer>::..;`, by its exported name. A glob re-export cannot be
/// resolved here, so it panics instead of silently passing.
fn reexported_names(lib_rs: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for layer in LAYERS {
        let prefix = format!("pub use {layer}::");
        let mut rest = lib_rs;
        while let Some(i) = rest.find(&prefix) {
            let after = &rest[i + prefix.len()..];
            let end = after.find(';').expect("a `pub use` ends in `;`");
            let stmt = &after[..end];
            assert!(
                !stmt.trim_start().starts_with('*'),
                "lib.rs has `pub use {layer}::*`: the gate cannot resolve a glob re-export"
            );
            let items: Vec<String> = if stmt.trim_start().starts_with('{') {
                // `a, b as c` exports `a` and `c`.
                group_heads_exported(stmt)
            } else {
                vec![exported(stmt)]
            };
            names.extend(items.into_iter().filter(|n| !n.is_empty() && n != "self"));
            rest = &after[end..];
        }
    }
    names
}

/// The name a `use` item exports: its alias after `as`, else its last path
/// segment.
fn exported(item: &str) -> String {
    let item = item.trim();
    let item = item.rsplit(" as ").next().unwrap_or(item);
    let last = item.rsplit("::").next().unwrap_or(item);
    first_ident(last).to_string()
}

fn group_heads_exported(group: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in group.char_indices() {
        match c {
            '{' => {
                depth += 1;
                if depth == 1 {
                    start = i + 1;
                }
            }
            '}' | ',' if depth == 1 => {
                out.push(exported(&group[start..i]));
                start = i + 1;
            }
            '}' => depth -= 1,
            _ => {}
        }
    }
    out
}

/// The forbidden things `src` names, in its production code: a layer
/// (`crate::agentic`), or an item a layer re-exports at the root
/// (`crate::Config`), directly or inside a `crate::{..}` group.
fn violations(src: &str, reexports: &BTreeSet<String>) -> BTreeSet<String> {
    let code = production_code(src);
    let forbidden: BTreeSet<&str> = LAYERS
        .iter()
        .copied()
        .chain(reexports.iter().map(String::as_str))
        .collect();
    let mut found = BTreeSet::new();
    for name in &forbidden {
        let direct = format!("crate::{name}");
        let mut rest = code.as_str();
        while let Some(i) = rest.find(&direct) {
            let after = &rest[i + direct.len()..];
            if !after.chars().next().is_some_and(is_ident_char) {
                found.insert(direct.clone());
                break;
            }
            rest = after;
        }
    }
    // `use crate::{agentic, Config, ..};` names them without the `crate::X` text.
    let mut rest = code.as_str();
    while let Some(i) = rest.find("crate::{") {
        let group = &rest[i + "crate::".len()..];
        for head in group_heads(group) {
            if forbidden.contains(head.as_str()) {
                found.insert(format!("crate::{head}"));
            }
        }
        rest = &rest[i + "crate::{".len()..];
    }
    found
}

fn src_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

#[test]
fn the_policy_layer_names_no_agent_store_or_config_item() {
    let src = src_dir();
    let lib_rs = std::fs::read_to_string(src.join("lib.rs")).expect("lib.rs");
    let reexports = reexported_names(&lib_rs);
    assert!(
        reexports.contains("Config"),
        "the re-export scan must see `Config` from `pub use config::{{..}}`; found {reexports:?}"
    );

    let mut found = BTreeSet::new();
    for module in POLICY_MODULES {
        let path = src.join(format!("{module}.rs"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("policy module {module} not found at {path:?}: {e}"));
        for v in violations(&text, &reexports) {
            found.insert((module.to_string(), v));
        }
    }
    let known: BTreeSet<(String, String)> = KNOWN_VIOLATIONS
        .iter()
        .map(|(m, v)| (m.to_string(), v.to_string()))
        .collect();

    let new: Vec<_> = found.difference(&known).collect();
    assert!(
        new.is_empty(),
        "the policy layer must not depend on agentic, store or config (nor on what lib.rs \
         re-exports from them); new: {new:?}"
    );
    let fixed: Vec<_> = known.difference(&found).collect();
    assert!(
        fixed.is_empty(),
        "KNOWN_VIOLATIONS lists violations that are gone; delete them (the ratchet only \
         shrinks): {fixed:?}"
    );
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Anti-vacuous twin: the scan flags each way of naming a layer or one of its
/// re-exported items, and ignores what it should, so a passing gate is not a
/// scan that sees nothing.
#[test]
fn the_scan_flags_a_layer_or_a_reexport_and_ignores_the_rest() {
    let re = set(&["Config", "ConfigError", "Verdict"]);

    // A layer, three ways.
    assert_eq!(
        violations("use crate::agentic::X;\n", &re),
        set(&["crate::agentic"])
    );
    assert_eq!(
        violations("let x = crate::store::open();\n", &re),
        set(&["crate::store"])
    );
    assert_eq!(
        violations("use crate::{caveats, config};\n", &re),
        set(&["crate::config"])
    );
    // An item a layer re-exports at the root.
    assert_eq!(
        violations("fn f(c: &crate::Config) {}\n", &re),
        set(&["crate::Config"])
    );
    assert_eq!(
        violations("use crate::{Caveats, Verdict, caveats::Scope};\n", &re),
        set(&["crate::Verdict"])
    );
    // A nested group does not hide a later member, and a multi-line group works.
    assert_eq!(
        violations("use crate::{caveats::{Caveats}, agentic};\n", &re),
        set(&["crate::agentic"])
    );
    assert_eq!(
        violations("use crate::{\n    caveats,\n    Config,\n};\n", &re),
        set(&["crate::Config"])
    );
    // Neighbouring names, comments and test-only code are not violations.
    assert!(violations("use crate::config_tests::x;\n", &re).is_empty());
    assert!(violations("use crate::ConfigurationBuilder;\n", &re).is_empty());
    assert!(violations("// use crate::agentic::Verdict;\nfn f() {}\n", &re).is_empty());
    assert!(violations(
        "fn f() {}\n#[cfg(test)]\nmod t { use crate::agentic::X; }\n",
        &re
    )
    .is_empty());
    assert!(violations(
        "fn f() {}\n#[cfg(all(test, unix))]\nmod t { use crate::Config; }\n",
        &re
    )
    .is_empty());
    assert!(violations("use crate::caveats::Caveats;\n", &re).is_empty());
}

/// The re-export scan reads plain, grouped, aliased and multi-line forms, and
/// ignores other layers' `pub use`.
#[test]
fn the_reexport_scan_reads_every_form_of_pub_use() {
    let lib = "pub use config::writeback_probed_backend;\n\
               pub use config::{\n    Config, ConfigError as CfgErr,\n    nested::Deep,\n};\n\
               pub use agentic::{chat_complete, Step};\n\
               pub use store::SessionId;\n\
               pub use caveats::Caveats;\n";
    assert_eq!(
        reexported_names(lib),
        set(&[
            "writeback_probed_backend",
            "Config",
            "CfgErr",
            "Deep",
            "chat_complete",
            "Step",
            "SessionId"
        ])
    );
}
