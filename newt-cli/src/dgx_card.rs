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

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use newt_core::model_card::{load_card_file, no_hardware_leak, Backend, ModelCard, VllmProfile};

use crate::dgx::{CardCmd, VllmPlanArgs};

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
/// The card catalog as LIST/PICK rows, through THE canonical typed catalog
/// ([`newt_core::card_catalog::ModelCardCatalog::entries`]) — every row is
/// the same finalized/validated resolution `card show`, `card setup`, and
/// the runtime use, so a listing can never disagree with them; files that
/// fail to parse (or resolve) come back as `problems`, visibly, instead of
/// being silently skipped the way the old `load_dropin_dir` walk did.
pub fn catalog_rows(dir: Option<&Path>) -> (Vec<CardEntry>, Vec<String>) {
    use newt_core::card_catalog::{CatalogOrigin, ModelCardCatalog};
    let catalog = ModelCardCatalog::load(dir);
    let mut rows = Vec::new();
    let mut problems = Vec::new();
    for entry in catalog.entries() {
        match entry.resolved {
            Ok(card) => rows.push(CardEntry {
                source: match entry.origin {
                    CatalogOrigin::Builtin => "built-in",
                    CatalogOrigin::BuiltinOverridden => "built-in (overridden)",
                    CatalogOrigin::Dropin => "drop-in",
                },
                card,
            }),
            Err(e) => problems.push(format!("{}: {e}", entry.name)),
        }
    }
    (rows, problems)
}

/// Render `card list` — a fixed-width table, or a JSON array under `--json`.
/// Pure.
#[must_use]
pub fn render_list(entries: &[CardEntry], problems: &[String], json: bool) -> String {
    if json {
        let mut arr: Vec<serde_json::Value> = entries
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
        // Unresolvable names ride the same array, visibly, as error rows.
        arr.extend(problems.iter().map(|p| serde_json::json!({ "error": p })));
        serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_string())
    } else if entries.is_empty() && problems.is_empty() {
        "no cards found".to_string()
    } else {
        use newt_core::markup::table::{render_table, Align, Column};
        let columns = [
            Column::new("NAME"),
            Column::new("BACKEND"),
            Column::new("GiB").align(Align::Right),
            Column::new("GATED"),
            Column::new("SOURCE"),
        ];
        let data: Vec<Vec<String>> = entries
            .iter()
            .map(|e| {
                vec![
                    e.card.name.clone(),
                    e.card.backend.map_or("-", |b| b.as_str()).to_string(),
                    e.card
                        .footprint_gib
                        .map_or_else(|| "-".to_string(), |f| format!("{f:.0}")),
                    if e.card.gated.unwrap_or(false) {
                        "yes"
                    } else {
                        ""
                    }
                    .to_string(),
                    e.source.to_string(),
                ]
            })
            .collect();
        let mut out = render_table(&columns, &data);
        // Problems stay OUTSIDE the table, exactly as before. They are
        // diagnostics about files that produced no row — an unresolvable card
        // has no name, backend or footprint to put in one — so folding them in
        // would mean a table of mostly-empty cells claiming to be entries.
        // They also come after every row rather than between them, which is
        // why this table can be GFM at all where `tuning_cmd`'s cannot.
        for problem in problems {
            out.push_str(&format!("!  {problem}\n"));
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

/// Resolve `name` through THE canonical catalog
/// ([`newt_core::card_catalog::ModelCardCatalog::resolve_exact`]) — the
/// same exact, case-sensitive lookup the runtime's capability sidecar
/// uses, so `dgx card show` and a running session can never disagree about
/// what a name means (nor about the typed Duplicate / Malformed /
/// NameMismatch / Invalid diagnostics). The CLI's added value is only the
/// decorated NotFound: the names that DO exist.
fn resolve_from(name: &str, dir: Option<&Path>) -> Result<ModelCard, String> {
    let catalog = newt_core::card_catalog::ModelCardCatalog::load(dir);
    catalog.resolve_exact(name).map_err(|e| match e {
        newt_core::card_catalog::CatalogError::NotFound { .. } => {
            let known = catalog.names();
            format!(
                "no card named `{name}`. Known cards: {}",
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known.join(", ")
                }
            )
        }
        other => other.to_string(),
    })
}

/// Map a card's vLLM profile onto the `vllm up` plan args, folding the structured
/// parser flags (reasoning / tool-call / auto-tool-choice) into the verbatim
/// `extra` argv so they ride the `vllm serve` command line ahead of the card's
/// own raw `extra`. Pure — the seam `card setup` shares with `vllm up`.
#[must_use]
pub fn card_to_vllm_plan_args(vllm: &VllmProfile) -> VllmPlanArgs {
    let mut extra: Vec<String> = Vec::new();
    if let Some(p) = &vllm.reasoning_parser {
        extra.push("--reasoning-parser".to_string());
        extra.push(p.clone());
    }
    if let Some(p) = &vllm.tool_call_parser {
        extra.push("--tool-call-parser".to_string());
        extra.push(p.clone());
    }
    if vllm.enable_auto_tool_choice == Some(true) {
        extra.push("--enable-auto-tool-choice".to_string());
    }
    extra.extend(vllm.extra.iter().cloned());
    VllmPlanArgs {
        served_name: vllm.served_name.clone(),
        dtype: None,
        tensor_parallel: vllm.tensor_parallel.unwrap_or(1),
        max_model_len: vllm.max_model_len,
        gpu_mem_util: vllm.gpu_mem.unwrap_or(0.90),
        port: 8000,
        docker: false,
        extra,
    }
}

/// Execute a `card` subcommand — the thin IO shell over the pure renderers.
/// `list` / `show` / `validate` are read-only; `setup` stands a backend up.
///
/// # Errors
/// Surfaces a bad name/card (unknown, unreadable, invalid, or leaky) or a
/// stand-up failure as an `anyhow` error for the CLI to print.
pub async fn run(cmd: CardCmd, config_path: Option<&Path>) -> anyhow::Result<()> {
    match cmd {
        CardCmd::List { json } => {
            let (entries, problems) = catalog_rows(dropin_dir(config_path).as_deref());
            println!("{}", render_list(&entries, &problems, json));
            Ok(())
        }
        CardCmd::Show { name, json } => {
            let card = resolve_from(&name, dropin_dir(config_path).as_deref())
                .map_err(|e| anyhow::anyhow!(e))?;
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
        CardCmd::Setup {
            name,
            model,
            node,
            backend,
            dry_run,
            force,
        } => setup_card(config_path, &name, model, node, backend, dry_run, force).await,
        CardCmd::Pick {
            model,
            node,
            backend,
            dry_run,
            force,
        } => {
            let (entries, problems) = catalog_rows(dropin_dir(config_path).as_deref());
            for problem in &problems {
                eprintln!("card problem (not pickable): {problem}");
            }
            if entries.is_empty() {
                anyhow::bail!("no cards found — nothing to pick from");
            }
            if !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "`card pick` is interactive and needs a TTY — use `card setup <name>` \
                     for a non-interactive stand-up (see `card list`)"
                );
            }
            // D3d found what this actually is: the numbers are SELECTORS the
            // operator types, not a table's row labels — so it belongs to the
            // interaction lane, and F0a is where it arrives. `render_menu`
            // and `parse_selection` are gone with it: the entries are options
            // and `resolve_typed` resolves them.
            let window = newt_core::tty::Terminal::suspend_for_prompt();
            let rows: Vec<(String, String)> = entries
                .iter()
                .enumerate()
                .map(|(i, e)| ((i + 1).to_string(), menu_label(e)))
                .collect();
            let choices: Vec<(&str, &str)> =
                rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let picked = newt_core::interaction_terminal::resolve_on_terminal(
                &window,
                &newt_core::interaction_form::menu("\nAvailable model cards:", "", &choices),
            )
            // EOF, a cancel, and an out-of-range number all land here. The
            // message says the operator did not choose, which is true of all
            // three and is a better answer than "malformed choice" for a
            // closed stream.
            .ok_or_else(|| anyhow::anyhow!("no card chosen"))?;
            let idx = picked.as_str().parse::<usize>().unwrap_or(1) - 1;
            let name = entries[idx].card.name.clone();
            setup_card(config_path, &name, model, node, backend, dry_run, force).await
        }
    }
}

/// Render the card catalog as a numbered interactive menu — the `card pick`
/// front-end for `card list`. Pure; the TTY read lives in [`run`].
#[must_use]
/// One catalog row's label, for the menu's option text.
///
/// The `{:>2}. {:<22} {:<8} {:>7}` hand-laid columns this replaces were the
/// `dgx_card` row D3d left in the ad-hoc-width category, and they went the
/// way D3d predicted: not by becoming a pipe table, but by becoming OPTIONS.
/// The leading number is the option's key now, so it is not in the label.
fn menu_label(e: &CardEntry) -> String {
    let backend = e.card.backend.map_or("-", |b| b.as_str());
    let gib = e
        .card
        .footprint_gib
        .map_or_else(|| "-".to_string(), |f| format!("{f:.0}GiB"));
    format!("{} {} {} {}", e.card.name, backend, gib, e.source)
}

/// Resolve a card by name, validate it, and stand its backend up. Shared by
/// `card setup <name>` and the interactive `card pick`.
///
/// # Errors
/// Surfaces an unknown/invalid/leaky card or a stand-up failure as `anyhow`.
async fn setup_card(
    config_path: Option<&Path>,
    name: &str,
    model: Option<String>,
    node: Option<String>,
    backend: Option<String>,
    dry_run: bool,
    force: bool,
) -> anyhow::Result<()> {
    let card =
        resolve_from(name, dropin_dir(config_path).as_deref()).map_err(|e| anyhow::anyhow!(e))?;
    // Validate (schema + host/LAN-IP leak) BEFORE standing anything up.
    validate_report(&card).map_err(|e| anyhow::anyhow!(e))?;
    let backend = match backend.as_deref() {
        Some(b) => b.parse::<Backend>().map_err(|e| anyhow::anyhow!(e))?,
        None => card.backend.ok_or_else(|| {
            anyhow::anyhow!("card `{}` has no backend; pass --backend", card.name)
        })?,
    };
    match backend {
        Backend::Vllm => setup_vllm(config_path, &card, model, node, force, dry_run).await,
        Backend::Ollama => {
            setup_ollama_stub(&card);
            Ok(())
        }
        Backend::LlamaCpp => {
            anyhow::bail!("card setup: the llama_cpp backend is not supported yet")
        }
    }
}

/// vLLM `card setup`: map the card onto the `vllm up` plan args and reuse the
/// stand-up (fit pre-flight → `vllm serve` over SSH → poll → activate endpoint).
/// The card's sampling tuning (temperature/top_p/top_k) is recorded on the card
/// but newt has no per-model sampling override yet, so it is reported, not wired.
async fn setup_vllm(
    config_path: Option<&Path>,
    card: &ModelCard,
    model: Option<String>,
    node: Option<String>,
    force: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let vllm = card
        .vllm
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("card `{}`: backend=vllm but no [vllm] block", card.name))?;
    let plan_args = card_to_vllm_plan_args(vllm);
    let served = vllm
        .served_name
        .clone()
        .unwrap_or_else(|| card.name.clone());
    let checkpoint = model.unwrap_or_else(|| served.clone());
    println!(
        "card setup `{}`: vLLM, serving `{checkpoint}` as `{served}`{}",
        card.name,
        if dry_run { " (dry run)" } else { "" }
    );
    crate::dgx::vllm_up(
        config_path,
        &checkpoint,
        node.as_deref(),
        &plan_args,
        force,
        false,
        dry_run,
    )
    .await?;
    if let Some(t) = &card.tuning {
        if t.temperature.is_some() || t.top_p.is_some() || t.top_k.is_some() {
            println!(
                "note: the card's sampling tuning (temperature/top_p/top_k) is recorded on the \
                 card but newt does not yet auto-apply per-model sampling — a follow-up."
            );
        }
    }
    Ok(())
}

/// ollama `card setup`: intentionally NOT implemented (vLLM is the first
/// fully-supported backend). The card still resolves + validates; this prints
/// how to point newt at the tag today and writes nothing.
fn setup_ollama_stub(card: &ModelCard) {
    println!(
        "card setup `{}`: the ollama backend is not yet implemented — vLLM is the first \
         fully-supported backend.",
        card.name
    );
    let tag = card
        .ollama
        .as_ref()
        .and_then(|o| o.tag.clone())
        .unwrap_or_else(|| "<tag>".to_string());
    println!("  The card resolves + validates fine. To point newt at the tag today:");
    println!("    newt dgx switch ollama {tag}");
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

    /// Write `(<file-stem>, <body>)` pairs into a temp catalog dir.
    fn catalog_dir_raw(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (stem, body) in files {
            std::fs::write(dir.path().join(format!("{stem}.toml")), body).expect("write card");
        }
        dir
    }

    fn valid_body(name: &str, temp: f64) -> String {
        format!(
            "name = \"{name}\"\nbackend = \"vllm\"\n\n[vllm]\nserved_name = \"{name}\"\n\n[tuning]\ntemperature = {temp}\n"
        )
    }

    #[test]
    fn catalog_rows_tag_origin_and_surface_problems() {
        // A valid drop-in, a builtin override at the declared key, a
        // MALFORMED file, and a NAME-MISMATCHED file: the listing shows the
        // same canonical resolution show/setup/runtime use — the broken
        // files surface as problems instead of silently vanishing.
        let dir = catalog_dir_raw(&[
            ("ccc", &valid_body("ccc", 0.2)),
            (
                "ornith-1.0-35b",
                "name = \"Ornith-1.0-35B\"\n\n[tuning]\ntemperature = 0.1\n",
            ),
            ("broken", "name = \"broken\"\nbackend = [not toml"),
            ("mismatch", &valid_body("elsewhere", 0.3)),
        ]);
        let (rows, problems) = catalog_rows(Some(dir.path()));
        let names: Vec<&str> = rows.iter().map(|e| e.card.name.as_str()).collect();
        assert!(names.contains(&"ccc"), "{names:?}");
        assert!(names.contains(&"Ornith-1.0-35B"), "{names:?}");
        let ornith = rows
            .iter()
            .find(|e| e.card.name == "Ornith-1.0-35B")
            .expect("listed");
        assert_eq!(ornith.source, "built-in (overridden)");
        assert_eq!(
            ornith.card.tuning.as_ref().and_then(|t| t.temperature),
            Some(0.1),
            "the LISTED row is the canonical resolved card, override applied"
        );
        assert!(
            rows.iter()
                .any(|e| e.card.name == "ccc" && e.source == "drop-in"),
            "{names:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("broken")),
            "malformed files SHOW: {problems:?}"
        );
        // The name-mismatched file lists under its PARSED (true) name — the
        // body name is the identity; the filename-keyed lookup errors with
        // the typed NameMismatch through the same canonical resolver.
        assert!(
            rows.iter().any(|e| e.card.name == "elsewhere"),
            "the card lists under its declared name: {names:?}"
        );
        let err = resolve_from("mismatch", Some(dir.path()))
            .expect_err("the filename key is not the identity");
        assert!(err.contains("elsewhere"), "{err}");
    }

    #[test]
    fn catalog_rows_surface_duplicates_and_invalid_overrides() {
        // Two files claiming one logical name → the row is the typed
        // Duplicate problem; an invalid resolved override → Invalid.
        let dir = catalog_dir_raw(&[
            ("dup-a", &valid_body("twin", 0.1)),
            ("dup-b", &valid_body("twin", 0.2)),
            (
                "nobackend",
                "name = \"nobackend\"\n\n[tuning]\ntemperature = 0.5\n",
            ),
        ]);
        let (rows, problems) = catalog_rows(Some(dir.path()));
        assert!(
            !rows.iter().any(|e| e.card.name == "twin"),
            "a duplicate never lists as a clean row"
        );
        assert!(
            problems.iter().any(|p| p.contains("twin")),
            "duplicate surfaces: {problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("nobackend")),
            "invalid override surfaces: {problems:?}"
        );
    }

    #[test]
    fn render_list_table_has_header_rows_and_problems() {
        let dir = catalog_dir_raw(&[("demo", &valid_body("demo", 0.6))]);
        let (entries, problems) = catalog_rows(Some(dir.path()));
        let out = render_list(&entries, &problems, false);
        assert!(out.contains("NAME") && out.contains("BACKEND") && out.contains("SOURCE"));
        assert!(out.contains("demo"));
        assert!(out.contains("vllm"));
        // Problems render visibly in the table form.
        let out = render_list(&entries, &["bad: malformed".to_string()], false);
        assert!(out.contains("!  bad: malformed"), "{out}");
    }

    /// **The byte golden for `newt dgx card list` as it ships today** (#1916).
    ///
    /// Captured from the shipping renderer — the method D3c used, and the
    /// reason is the same: F0's "unlisted golden diffs are bugs" only bites if
    /// there is something to diff against. The source labels are `built-in` /
    /// `drop-in` rather than paths, so this is stable across machines.
    #[test]
    fn the_card_catalog_is_byte_exact() {
        let dir = catalog_dir_raw(&[("demo", &valid_body("demo", 0.6))]);
        let (entries, problems) = catalog_rows(Some(dir.path()));
        assert_eq!(
            render_list(&entries, &problems, false),
            concat!(
                "| NAME            | BACKEND | GiB | GATED | SOURCE   |\n",
                "| --------------- | ------- | --: | ----- | -------- |\n",
                "| Ornith-1.0-35B  | vllm    |  35 |       | built-in |\n",
                "| Ornith-1.0-397B | vllm    | 400 | yes   | built-in |\n",
                "| demo            | vllm    |   - |       | drop-in  |\n",
            )
        );
    }

    /// The `--json` arm must NOT move: it is a machine contract, and this
    /// slice changes only the human table.
    #[test]
    fn the_card_catalog_json_is_unchanged_by_the_table_migration() {
        let dir = catalog_dir_raw(&[("demo", &valid_body("demo", 0.6))]);
        let (entries, problems) = catalog_rows(Some(dir.path()));
        let json = render_list(&entries, &problems, true);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let arr = parsed.as_array().expect("an array");
        assert_eq!(arr.len(), 3, "{json}");
        assert_eq!(arr[2]["name"], "demo", "{json}");
    }

    #[test]
    fn render_list_json_is_a_valid_array_with_error_rows() {
        let dir = catalog_dir_raw(&[("demo", &valid_body("demo", 0.6))]);
        let (entries, _) = catalog_rows(Some(dir.path()));
        let out = render_list(&entries, &["bad: malformed".to_string()], true);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        let arr = v.as_array().expect("array");
        assert!(arr.iter().any(|r| r["name"] == "demo"));
        assert!(arr.iter().any(|r| r["error"] == "bad: malformed"));
    }

    #[test]
    fn render_list_empty_is_friendly() {
        assert_eq!(render_list(&[], &[], false), "no cards found");
        assert_eq!(render_list(&[], &[], true), "[]");
    }

    #[test]
    fn the_card_menu_offers_every_card_as_a_numbered_option() {
        let dir = catalog_dir_raw(&[
            ("aaa", &valid_body("aaa", 0.6)),
            ("bbb", &valid_body("bbb", 0.6)),
        ]);
        let (entries, _) = catalog_rows(Some(dir.path()));
        let rows: Vec<(String, String)> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| ((i + 1).to_string(), menu_label(e)))
            .collect();
        let choices: Vec<(&str, &str)> =
            rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let def = newt_core::interaction_form::menu("cards", "", &choices);
        let shown = newt_core::markup::plain::render(&def);

        // One option per card, numbered from 1, each on its own line with its
        // key bracketed. The catalog carries BUILT-INS as well as the two
        // drop-ins this test writes, so the drop-ins are NOT at positions 1
        // and 2 — the count comes from the catalog, not from the fixtures.
        // Asserting `[1] 1 ` was wrong twice over: the number is the option's
        // KEY, not part of its label, and entry 1 is a built-in.
        let n = entries.len();
        assert!(n > 2, "built-ins are in the catalog too: {n}");
        assert_eq!(shown.lines().count(), n + 1, "body plus one line per card");
        assert!(shown.contains("[1] "), "{shown}");
        assert!(shown.contains(&format!("[{n}] ")), "{shown}");
        assert!(shown.contains(" aaa "), "{shown}");
        assert!(shown.contains(" bbb "), "{shown}");
        assert!(shown.contains("vllm"), "{shown}");
        for i in 1..=n {
            let key = i.to_string();
            assert_eq!(
                newt_core::interaction_form::resolve(&def, &key).map(|o| o.as_str().to_string()),
                Some(key.clone()),
                "{key}"
            );
        }
        // Out of range and garbage resolve to nothing, which the call site
        // turns into "no card chosen" — the arm `parse_selection`'s
        // malformed-choice error used to occupy.
        let past_end = (n + 1).to_string();
        for miss in ["0", &past_end, "abc", ""] {
            assert!(
                newt_core::interaction_form::resolve(&def, miss).is_none(),
                "{miss}"
            );
        }
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

    /// Write `cards` into a temp catalog dir (one `<name>.toml` each).
    fn catalog_dir(cards: &[(&str, f64)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, temp) in cards {
            std::fs::write(
                dir.path().join(format!("{name}.toml")),
                format!(
                    "name = \"{name}\"\nbackend = \"vllm\"\n\n[vllm]\nserved_name = \"{name}\"\n\n[tuning]\ntemperature = {temp}\n"
                ),
            )
            .expect("write card");
        }
        dir
    }

    #[test]
    fn resolve_from_unknown_lists_known_names() {
        let dir = catalog_dir(&[("aaa", 0.6)]);
        let err = resolve_from("nope", Some(dir.path())).expect_err("unknown card");
        assert!(err.contains("no card named `nope`"), "got: {err}");
        assert!(err.contains("aaa"), "lists known names: {err}");
    }

    #[test]
    fn resolve_from_dropin_overrides_builtin() {
        // The built-in Ornith card overridden by a drop-in at its DECLARED
        // source key, carrying the exact logical name: the drop-in's tuning
        // wins through the canonical catalog merge (identity stays exact —
        // see `resolve_from_is_exact_never_case_folded`).
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("ornith-1.0-35b.toml"),
            "name = \"Ornith-1.0-35B\"\n\n[tuning]\ntemperature = 0.1\n",
        )
        .expect("write override");
        let resolved = resolve_from("Ornith-1.0-35B", Some(dir.path())).expect("resolves");
        assert_eq!(resolved.tuning.and_then(|t| t.temperature), Some(0.1));
    }

    #[test]
    fn resolve_from_dropin_only_card_resolves() {
        // A user card for a model with no built-in resolves from the drop-in.
        let dir = catalog_dir(&[("custom", 0.3)]);
        let resolved = resolve_from("custom", Some(dir.path())).expect("drop-in only");
        assert_eq!(resolved.name, "custom");
    }

    #[test]
    fn resolve_from_is_exact_never_case_folded() {
        // The catalog's identity contract rides through the CLI: a case
        // near-collision is NotFound (listing the exact names), never a
        // silent bind.
        let dir = catalog_dir(&[("custom", 0.3)]);
        let err = resolve_from("Custom", Some(dir.path())).expect_err("exact identity");
        assert!(err.contains("no card named `Custom`"), "got: {err}");
        assert!(err.contains("custom"), "lists the exact name: {err}");
    }

    #[test]
    fn card_to_plan_args_folds_parser_flags_into_extra() {
        // The Ornith built-in exercises the full mapping. Goes through the
        // canonical catalog (exact identity), not the raw builtin card
        // directly: the parser fields come from the qwen3 family defaults
        // (the card's own [vllm] table no longer redeclares them), so
        // reading the unresolved card would see them as absent.
        let vllm = resolve_from("Ornith-1.0-35B", None)
            .expect("builtin resolves")
            .vllm
            .unwrap();
        let args = card_to_vllm_plan_args(&vllm);
        assert_eq!(args.served_name.as_deref(), Some("Ornith-1.0-35B"));
        assert_eq!(args.max_model_len, Some(262144));
        assert_eq!(args.tensor_parallel, 1);
        // The structured parser flags are folded into the serve argv...
        let e = args.extra.join(" ");
        assert!(e.contains("--reasoning-parser qwen3"), "got: {e}");
        assert!(e.contains("--tool-call-parser qwen3_xml"), "got: {e}");
        assert!(e.contains("--enable-auto-tool-choice"), "got: {e}");
        // ...ahead of the card's own raw extra (the escape hatch).
        assert!(e.contains("--enable-prefix-caching"), "got: {e}");
        let (rp, pc) = (
            e.find("--reasoning-parser").unwrap(),
            e.find("--enable-prefix-caching").unwrap(),
        );
        assert!(rp < pc, "structured flags precede raw extra: {e}");
    }

    #[test]
    fn card_to_plan_args_omits_absent_flags_and_defaults() {
        // A minimal vLLM profile: no parsers, no extra.
        let c = card("name = \"m\"\nbackend = \"vllm\"\n\n[vllm]\nserved_name = \"m\"\n");
        let args = card_to_vllm_plan_args(c.vllm.as_ref().unwrap());
        assert!(
            args.extra.is_empty(),
            "no flags -> empty extra: {:?}",
            args.extra
        );
        assert_eq!(args.tensor_parallel, 1);
        assert_eq!((args.gpu_mem_util * 100.0).round(), 90.0); // default 0.90
        assert_eq!(args.port, 8000);
        assert!(!args.docker);
    }
}
