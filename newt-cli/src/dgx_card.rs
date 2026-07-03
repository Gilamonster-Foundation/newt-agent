//! Pure, unit-testable logic behind `newt dgx card` (#855).
//!
//! A *card* is every setting needed to serve one model — the schema lives in
//! [`newt_core::model_card`]. Built-in cards ship compiled into the binary
//! (Ornith today); a user drops `~/.newt/models/<name>.toml|.yaml` to override a
//! built-in or to teach newt a model it doesn't know. Precedence, top wins:
//! `built-in < ~/.newt/models/<name> < --card < CLI flag`.
//!
//! This module is the **discover / inspect** half of the surface — `list`,
//! `show`, `validate`. Standing a backend up + writing the `~/.newt`
//! backend/tuning config (`card setup`; vLLM first, ollama stubbed) is the
//! focused follow-on tracked on #855.
//!
//! Everything here is pure string-rendering over already-parsed cards; the only
//! IO — reading the drop-in dir — is a thin best-effort wrapper in [`run`], so
//! the render/validate/resolve logic is fully unit-testable fs-free (the
//! `dgx_pull.rs` pattern).

use std::path::{Path, PathBuf};

use newt_core::model_card::{
    builtin_cards, load_card_file, load_dropin_dir, no_hardware_leak, resolve, ModelCard,
};

use crate::dgx::CardCmd;

/// A card paired with where it came from — drives the `list` SOURCE column.
pub struct CardEntry {
    pub card: ModelCard,
    pub source: &'static str,
}

/// `~/.newt/models` — the drop-in card dir, a sibling of `config.toml`. Honors a
/// `--config` override (its sibling `models/`) exactly as the rest of `dgx.rs`
/// threads `config_path`.
fn dropin_dir(config_path: Option<&Path>) -> Option<PathBuf> {
    match config_path {
        Some(p) => Some(p.with_file_name("models")),
        None => newt_core::config::Config::user_config_path().map(|p| p.with_file_name("models")),
    }
}

/// Merge the built-in set with drop-ins into one name-sorted catalog, tagging
/// each entry's origin. A drop-in sharing a built-in's name is shown once, on
/// the built-in row tagged `built-in (overridden)` — it *overrides* rather than
/// duplicates, matching [`resolve`]'s precedence. Pure.
#[must_use]
pub fn catalog(builtins: Vec<ModelCard>, dropins: Vec<ModelCard>) -> Vec<CardEntry> {
    use std::collections::BTreeSet;
    let dropin_names: BTreeSet<String> = dropins.iter().map(|c| c.name.clone()).collect();
    let builtin_names: BTreeSet<String> = builtins.iter().map(|c| c.name.clone()).collect();
    let mut entries: Vec<CardEntry> = Vec::new();
    for b in builtins {
        let source = if dropin_names.contains(&b.name) {
            "built-in (overridden)"
        } else {
            "built-in"
        };
        entries.push(CardEntry { source, card: b });
    }
    for d in dropins {
        if !builtin_names.contains(&d.name) {
            entries.push(CardEntry {
                source: "drop-in",
                card: d,
            });
        }
    }
    entries.sort_by(|a, b| a.card.name.cmp(&b.card.name));
    entries
}

/// Render `card list` — a fixed-width table, or a JSON array under `--json`.
/// Pure.
#[must_use]
pub fn render_list(entries: &[CardEntry], json: bool) -> String {
    if json {
        let arr: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "name": e.card.name,
                    "backend": e.card.backend.map(|b| b.as_str()),
                    "footprint_gib": e.card.footprint_gib,
                    "gated": e.card.gated.unwrap_or(false),
                    "source": e.source,
                })
            })
            .collect();
        serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_string())
    } else if entries.is_empty() {
        "no cards found".to_string()
    } else {
        let mut out = format!(
            "{:<22} {:<8} {:>6}  {:<6} {}\n",
            "NAME", "BACKEND", "GiB", "GATED", "SOURCE"
        );
        for e in entries {
            let backend = e.card.backend.map_or("-", |b| b.as_str());
            let gib = e
                .card
                .footprint_gib
                .map_or_else(|| "-".to_string(), |f| format!("{f:.0}"));
            let gated = if e.card.gated.unwrap_or(false) {
                "yes"
            } else {
                ""
            };
            out.push_str(&format!(
                "{:<22} {:<8} {:>6}  {:<6} {}\n",
                e.card.name, backend, gib, gated, e.source
            ));
        }
        out
    }
}

/// Render one resolved card as canonical TOML, or JSON under `--json`. Pure.
///
/// # Errors
/// Propagates a (never-expected) serialization failure from the card.
pub fn render_show(card: &ModelCard, json: bool) -> Result<String, String> {
    if json {
        serde_json::to_string_pretty(card).map_err(|e| e.to_string())
    } else {
        card.to_toml()
    }
}

/// Validate a card and return a one-line human report, or an `Err` describing
/// the failure. Surfaces both schema errors ([`ModelCard::validate`]) and any
/// host/hardware-identity leak ([`no_hardware_leak`]) — a card is a portable,
/// committable artifact, so an embedded LAN IP is a hard failure, not a warning.
/// Pure over the already-parsed card.
///
/// # Errors
/// Returns the schema error, or a leak report listing the offending tokens.
pub fn validate_report(card: &ModelCard) -> Result<String, String> {
    card.validate()?;
    let leaks = no_hardware_leak(card);
    if !leaks.is_empty() {
        return Err(format!(
            "card `{}` leaks host/hardware identity (keep endpoints/IPs in ~/.newt, \
             not in a portable card): {}",
            card.name,
            leaks.join(", ")
        ));
    }
    Ok(format!(
        "OK: card `{}` is valid ({} backend)",
        card.name,
        card.backend.map_or("unspecified", |b| b.as_str())
    ))
}

/// Resolve `name` from the built-in + drop-in sets via the precedence chain, or
/// an `Err` listing the known names. Pure — the caller supplies the sets.
///
/// # Errors
/// Returns a "no such card" message (with the known names) when neither a
/// built-in nor a drop-in matches `name`.
fn resolve_from(
    name: &str,
    builtins: Vec<ModelCard>,
    dropins: Vec<ModelCard>,
) -> Result<ModelCard, String> {
    let builtin = builtins.iter().find(|c| c.name == name).cloned();
    let dropin = dropins.iter().find(|c| c.name == name).cloned();
    let Some(base) = builtin.or(dropin) else {
        let known: Vec<String> = catalog(builtins, dropins)
            .into_iter()
            .map(|e| e.card.name)
            .collect();
        return Err(format!(
            "no card named `{name}`. Known cards: {}",
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        ));
    };
    Ok(resolve(base, &dropins, None))
}

/// Execute a `card` subcommand — the thin IO shell over the pure renderers.
/// Gathers built-ins (compiled in) + drop-ins (`~/.newt/models`, best-effort)
/// and prints. Read-only: nothing here mutates config or touches the network.
///
/// # Errors
/// Surfaces a bad `--card`/name (unknown card, unreadable/invalid file) as an
/// `anyhow` error for the CLI to print.
pub fn run(cmd: CardCmd, config_path: Option<&Path>) -> anyhow::Result<()> {
    let dropins = || {
        dropin_dir(config_path)
            .map(|d| load_dropin_dir(&d))
            .unwrap_or_default()
    };
    match cmd {
        CardCmd::List { json } => {
            let entries = catalog(builtin_cards(), dropins());
            println!("{}", render_list(&entries, json));
            Ok(())
        }
        CardCmd::Show { name, json } => {
            let card =
                resolve_from(&name, builtin_cards(), dropins()).map_err(|e| anyhow::anyhow!(e))?;
            println!(
                "{}",
                render_show(&card, json).map_err(|e| anyhow::anyhow!(e))?
            );
            Ok(())
        }
        CardCmd::Validate { path } => {
            let card = load_card_file(&path).map_err(|e| anyhow::anyhow!(e))?;
            let report = validate_report(&card).map_err(|e| anyhow::anyhow!(e))?;
            println!("{report}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::model_card::parse_card;

    /// Build a card from a TOML literal (pure — no fs).
    fn card(toml: &str) -> ModelCard {
        parse_card(toml, "toml").expect("valid test card")
    }

    fn vllm_card(name: &str, temp: f64) -> ModelCard {
        card(&format!(
            "name = \"{name}\"\nbackend = \"vllm\"\n\n[vllm]\nserved_name = \"{name}\"\n\n[tuning]\ntemperature = {temp}\n"
        ))
    }

    #[test]
    fn catalog_tags_builtin_dropin_and_override() {
        let builtins = vec![vllm_card("aaa", 0.6), vllm_card("bbb", 0.6)];
        let dropins = vec![vllm_card("bbb", 0.1), vllm_card("ccc", 0.2)];
        let entries = catalog(builtins, dropins);

        // Sorted by name, each name once.
        let names: Vec<&str> = entries.iter().map(|e| e.card.name.as_str()).collect();
        assert_eq!(names, ["aaa", "bbb", "ccc"]);
        assert_eq!(entries[0].source, "built-in");
        assert_eq!(entries[1].source, "built-in (overridden)"); // bbb shadowed
        assert_eq!(entries[2].source, "drop-in"); // ccc is net-new
    }

    #[test]
    fn render_list_table_has_header_and_rows() {
        let entries = catalog(vec![vllm_card("demo", 0.6)], vec![]);
        let out = render_list(&entries, false);
        assert!(out.contains("NAME") && out.contains("BACKEND") && out.contains("SOURCE"));
        assert!(out.contains("demo"));
        assert!(out.contains("vllm"));
    }

    #[test]
    fn render_list_json_is_a_valid_array() {
        let entries = catalog(vec![vllm_card("demo", 0.6)], vec![]);
        let out = render_list(&entries, true);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        let arr = v.as_array().expect("array");
        assert_eq!(arr[0]["name"], "demo");
        assert_eq!(arr[0]["backend"], "vllm");
        assert_eq!(arr[0]["source"], "built-in");
    }

    #[test]
    fn render_list_empty_is_friendly() {
        assert_eq!(render_list(&[], false), "no cards found");
        assert_eq!(render_list(&[], true), "[]");
    }

    #[test]
    fn render_show_toml_roundtrips() {
        let c = vllm_card("demo", 0.6);
        let out = render_show(&c, false).expect("toml");
        assert!(out.contains("name = \"demo\""));
        // Re-parse to prove it's canonical, loadable TOML.
        let back = parse_card(&out, "toml").expect("reparse");
        assert_eq!(back.name, "demo");
    }

    #[test]
    fn render_show_json_parses() {
        let c = vllm_card("demo", 0.6);
        let out = render_show(&c, true).expect("json");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(v["name"], "demo");
    }

    #[test]
    fn validate_report_ok_for_complete_card() {
        let msg = validate_report(&vllm_card("demo", 0.6)).expect("valid");
        assert!(msg.contains("valid"), "got: {msg}");
        assert!(msg.contains("vllm"));
    }

    #[test]
    fn validate_report_flags_missing_backend_block() {
        // backend=vllm but no [vllm] block — validate() rejects it.
        let c = card("name = \"x\"\nbackend = \"vllm\"\n");
        let err = validate_report(&c).expect_err("should reject");
        assert!(err.contains("serving block"), "got: {err}");
    }

    #[test]
    fn validate_report_flags_hardware_leak() {
        // A private LAN IP smuggled into a card field. Built from octets at
        // runtime so no literal RFC1918 address lands in committed source (the
        // pre-push network-leak guard).
        let ip = format!("{}.{}.{}.{}", 10, 0, 0, 5);
        let c = card(&format!(
            "name = \"leaky\"\nbackend = \"vllm\"\n\n[vllm]\nserved_name = \"demo\"\nextra = [\"{ip}\"]\n"
        ));
        let err = validate_report(&c).expect_err("should flag the leak");
        assert!(err.contains("leaks host/hardware"), "got: {err}");
        assert!(err.contains(&ip), "names the offending token: {err}");
    }

    #[test]
    fn resolve_from_unknown_lists_known_names() {
        let err =
            resolve_from("nope", vec![vllm_card("aaa", 0.6)], vec![]).expect_err("unknown card");
        assert!(err.contains("no card named `nope`"), "got: {err}");
        assert!(err.contains("aaa"), "lists known names: {err}");
    }

    #[test]
    fn resolve_from_dropin_overrides_builtin() {
        // Built-in temp 0.6; a drop-in of the same name sets 0.1 -> 0.1 wins.
        let resolved = resolve_from(
            "demo",
            vec![vllm_card("demo", 0.6)],
            vec![vllm_card("demo", 0.1)],
        )
        .expect("resolves");
        assert_eq!(resolved.tuning.and_then(|t| t.temperature), Some(0.1));
    }

    #[test]
    fn resolve_from_dropin_only_card_resolves() {
        // A user card for a model with no built-in resolves from the drop-in.
        let resolved =
            resolve_from("custom", vec![], vec![vllm_card("custom", 0.3)]).expect("drop-in only");
        assert_eq!(resolved.name, "custom");
    }
}
