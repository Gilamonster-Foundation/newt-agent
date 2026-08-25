//! The canonical **model-card catalog** — ONE owner of "what does this card
//! name mean". The runtime capability sidecar
//! ([`crate::model_card::ResolvedCapabilities`]) and `newt dgx card`
//! `show`/`setup` resolve through [`ModelCardCatalog::resolve_exact`];
//! nothing else answers a card-name lookup.
//!
//! Identity is the card's **`name` field, exactly** — case-sensitive, no
//! substring, no normalization. Filenames are *source metadata*: every
//! built-in declares its override source key explicitly AS DATA beside the
//! embedded card (e.g. `ornith-1.0-35b` beside `Ornith-1.0-35B`, in
//! [`builtin_card_entries`]), and [`ModelCardCatalog::resolve_exact`] preflights the
//! source keys RELEVANT to a request, so a malformed or name-mismatched
//! file sitting in an override slot errors instead of being silently
//! resolved past — while an unrelated malformed file cannot poison a good
//! card.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::model_card::{apply_family_defaults, builtin_card_entries, parse_card, ModelCard};

/// One drop-in source file: its source key (the file stem, verbatim), its
/// path, and its parse outcome.
#[derive(Debug, Clone)]
pub struct CardSource {
    /// The file stem exactly as on disk — metadata, never an identity.
    pub key: String,
    pub path: PathBuf,
    pub parsed: Result<ModelCard, String>,
}

/// A built-in card with its EXPLICIT override source key — declared as data
/// beside the embedded card file ([`builtin_card_entries`]), consulted
/// literally, never derived by normalizing a name at runtime.
#[derive(Debug, Clone)]
struct BuiltinEntry {
    source_key: String,
    card: ModelCard,
}

/// The built-in cards plus one drop-in directory's sources, held with their
/// diagnostics. Construction is the only IO ([`Self::load`]); resolution is
/// pure over the held data.
#[derive(Debug, Clone)]
pub struct ModelCardCatalog {
    builtins: Vec<BuiltinEntry>,
    /// Drop-in sources in sorted `(key, path)` order.
    sources: Vec<CardSource>,
    /// The searched drop-in dir, for diagnostics. `None` = built-ins only.
    dir: Option<PathBuf>,
}

/// Why a card name did not resolve — stable, typed, and actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// More than one drop-in file contends for the card — by shared source
    /// key or by shared logical identity. Paths are in stable sorted order.
    Duplicate { name: String, paths: Vec<PathBuf> },
    /// A file in one of the card's source slots does not parse.
    Malformed { path: PathBuf, error: String },
    /// A file in one of the card's source slots declares a different
    /// identity.
    NameMismatch {
        path: PathBuf,
        requested: String,
        declared: String,
    },
    /// The name resolved, but the fully-merged card fails validation.
    Invalid { name: String, error: String },
    /// Nothing carries the name.
    NotFound {
        name: String,
        searched: Option<PathBuf>,
    },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate { name, paths } => {
                let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                write!(
                    f,
                    "card `{name}`: {} drop-in files contend for it ({}) — keep exactly one",
                    paths.len(),
                    paths.join(", ")
                )
            }
            Self::Malformed { path, error } => write!(
                f,
                "card file {} EXISTS but did not parse as a card — fix the file: {error}",
                path.display()
            ),
            Self::NameMismatch {
                path,
                requested,
                declared,
            } => write!(
                f,
                "card file {} declares `{declared}`, not `{requested}` — a card's identity \
                 is its `name` field (exact); fix the name or rename the file out of this \
                 card's override slot",
                path.display()
            ),
            Self::Invalid { name, error } => write!(f, "card `{name}` is invalid: {error}"),
            Self::NotFound { name, searched } => match searched {
                Some(dir) => write!(
                    f,
                    "no card named `{name}` (searched built-ins, then {})",
                    dir.display()
                ),
                None => write!(
                    f,
                    "no card named `{name}` (searched built-ins; no card dir)"
                ),
            },
        }
    }
}

impl std::error::Error for CatalogError {}

impl ModelCardCatalog {
    /// Pure constructor over already-loaded sources (the mocked test tier).
    /// Each built-in arrives with its EXPLICIT override source key. Sources
    /// are held in sorted `(key, path)` order so every diagnostic is
    /// deterministic whatever the filesystem enumeration order was.
    ///
    /// # Panics
    /// When two built-ins share a logical name or an override source key —
    /// a compiled-in data defect that must never ship.
    #[must_use]
    pub fn new(
        builtins: Vec<(String, ModelCard)>,
        mut sources: Vec<CardSource>,
        dir: Option<PathBuf>,
    ) -> Self {
        let builtins: Vec<BuiltinEntry> = builtins
            .into_iter()
            .map(|(source_key, card)| BuiltinEntry { source_key, card })
            .collect();
        for (i, b) in builtins.iter().enumerate() {
            assert!(
                !builtins[..i]
                    .iter()
                    .any(|o| o.card.name == b.card.name || o.source_key == b.source_key),
                "built-in cards must have unique names and override keys: `{}`",
                b.card.name
            );
        }
        sources.sort_by(|a, b| a.key.cmp(&b.key).then_with(|| a.path.cmp(&b.path)));
        Self {
            builtins,
            sources,
            dir,
        }
    }

    /// The shipped built-ins plus `dir`'s `*.toml|yaml|yml` drop-ins, each
    /// file's parse outcome retained. The only IO in this module.
    #[must_use]
    pub fn load(dir: Option<&Path>) -> Self {
        let mut sources = Vec::new();
        if let Some(dir) = dir {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.flatten() {
                    let path = entry.path();
                    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                    if !matches!(ext, "toml" | "yaml" | "yml") {
                        continue;
                    }
                    let Some(key) = path.file_stem().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    let parsed = std::fs::read_to_string(&path)
                        .map_err(|e| format!("read: {e}"))
                        .and_then(|text| parse_card(&text, ext));
                    sources.push(CardSource {
                        key: key.to_string(),
                        path,
                        parsed,
                    });
                }
            }
        }
        Self::new(builtin_card_entries(), sources, dir.map(Path::to_path_buf))
    }

    /// The held sources, in sorted `(key, path)` order.
    #[must_use]
    pub fn sources(&self) -> &[CardSource] {
        &self.sources
    }

    /// Every resolvable card name — built-ins plus parsed drop-ins, sorted,
    /// deduplicated. For decorated "known cards" errors and menus.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .builtins
            .iter()
            .map(|b| b.card.name.clone())
            .chain(
                self.sources
                    .iter()
                    .filter_map(|s| s.parsed.as_ref().ok())
                    .map(|c| c.name.clone()),
            )
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// THE lookup. Exact, case-sensitive logical identity; one built-in plus
    /// at most one drop-in override; the raw base/overlay merge happens
    /// first, family defaults apply once to the FINAL family, and the fully
    /// resolved card is validated. A drop-in with no built-in base takes the
    /// identical path.
    ///
    /// # Errors
    /// A typed [`CatalogError`]. A malformed or name-mismatched file in one
    /// of THIS card's source slots (the requested name, or the built-in's
    /// explicit override key) errors even when the built-in alone could have
    /// answered — a failed override must never be silently skipped.
    pub fn resolve_exact(&self, name: &str) -> Result<ModelCard, CatalogError> {
        let builtin = self.builtins.iter().find(|b| b.card.name == name);
        // Preflight the source slots this request depends on.
        let mut relevant_keys: Vec<&str> = vec![name];
        if let Some(b) = builtin {
            if b.source_key != name {
                relevant_keys.push(&b.source_key);
            }
        }
        for key in relevant_keys {
            let slot: Vec<&CardSource> = self.sources.iter().filter(|s| s.key == key).collect();
            if slot.len() > 1 {
                // e.g. `foo.toml` beside `foo.yaml` — ambiguous even when
                // one of them is malformed.
                return Err(CatalogError::Duplicate {
                    name: name.to_string(),
                    paths: slot.iter().map(|s| s.path.clone()).collect(),
                });
            }
            if let Some(s) = slot.first() {
                match &s.parsed {
                    Err(error) => {
                        return Err(CatalogError::Malformed {
                            path: s.path.clone(),
                            error: error.clone(),
                        })
                    }
                    Ok(card) if card.name != name => {
                        return Err(CatalogError::NameMismatch {
                            path: s.path.clone(),
                            requested: name.to_string(),
                            declared: card.name.clone(),
                        })
                    }
                    Ok(_) => {}
                }
            }
        }
        // Overrides by exact logical identity, wherever the file lives.
        let overrides: Vec<&CardSource> = self
            .sources
            .iter()
            .filter(|s| s.parsed.as_ref().is_ok_and(|c| c.name == name))
            .collect();
        if overrides.len() > 1 {
            return Err(CatalogError::Duplicate {
                name: name.to_string(),
                paths: overrides.iter().map(|s| s.path.clone()).collect(),
            });
        }
        // The selected override's OWN key must be unambiguous too — a second
        // file under the same key contends for it even when that key is
        // neither the requested name nor the built-in's override slot.
        if let Some(winner) = overrides.first() {
            let slot: Vec<&CardSource> = self
                .sources
                .iter()
                .filter(|s| s.key == winner.key)
                .collect();
            if slot.len() > 1 {
                return Err(CatalogError::Duplicate {
                    name: name.to_string(),
                    paths: slot.iter().map(|s| s.path.clone()).collect(),
                });
            }
        }
        let overlay = overrides
            .first()
            .and_then(|s| s.parsed.as_ref().ok())
            .cloned();
        let raw = match (builtin.map(|b| b.card.clone()), overlay) {
            (Some(base), Some(over)) => base.merge(over),
            (Some(base), None) => base,
            (None, Some(over)) => over,
            (None, None) => {
                return Err(CatalogError::NotFound {
                    name: name.to_string(),
                    searched: self.dir.clone(),
                })
            }
        };
        finalize(raw).map_err(|error| CatalogError::Invalid {
            name: name.to_string(),
            error,
        })
    }
}

/// Merge order made law: the caller merged raw base/overlay already; here
/// family defaults fill gaps for the FINAL card's family (so an overlay that
/// changes `family` gets the family it ends up in, and a drop-in-only card
/// gets its family layer at all), then the fully resolved card is validated.
///
/// # Errors
/// The card's own [`ModelCard::validate`] reason.
pub fn finalize(mut card: ModelCard) -> Result<ModelCard, String> {
    apply_family_defaults(&mut card);
    card.validate()?;
    Ok(card)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(toml: &str) -> ModelCard {
        parse_card(toml, "toml").expect("valid test card")
    }

    fn vllm_card(name: &str) -> ModelCard {
        card(&format!(
            "name = \"{name}\"\nbackend = \"vllm\"\n\n[vllm]\nserved_name = \"{name}\"\n"
        ))
    }

    fn source(key: &str, parsed: Result<ModelCard, String>) -> CardSource {
        CardSource {
            key: key.to_string(),
            path: PathBuf::from(format!("/cards/{key}.toml")),
            parsed,
        }
    }

    /// A built-in with its explicitly declared override key.
    fn keyed(key: &str, card: ModelCard) -> (String, ModelCard) {
        (key.to_string(), card)
    }

    #[test]
    fn resolve_exact_is_case_sensitive_for_builtins_and_dropins() {
        let catalog = ModelCardCatalog::new(
            vec![keyed("ornith-test", vllm_card("Ornith-Test"))],
            vec![],
            None,
        );
        assert!(catalog.resolve_exact("Ornith-Test").is_ok());
        assert!(matches!(
            catalog.resolve_exact("Ornith-TEST"),
            Err(CatalogError::NotFound { .. })
        ));
    }

    #[test]
    fn a_lowercase_filename_still_overrides_the_builtin_its_body_names() {
        // The file key is the built-in's EXPLICIT override key (lowercase);
        // the body declares the built-in's exact name. Identity comes from
        // the body — the override applies with no fuzzy matching anywhere.
        let mut over = vllm_card("Ornith-Test");
        over.footprint_gib = Some(9.0);
        let catalog = ModelCardCatalog::new(
            vec![keyed("ornith-test", vllm_card("Ornith-Test"))],
            vec![source("ornith-test", Ok(over))],
            None,
        );
        let resolved = catalog
            .resolve_exact("Ornith-Test")
            .expect("override applies");
        assert_eq!(resolved.footprint_gib, Some(9.0));
    }

    #[test]
    fn a_malformed_file_in_the_builtin_override_slot_errors_not_falls_through() {
        // The operator wrote an override and broke it: resolving the
        // built-in must say so — silently answering from the built-in would
        // hide that their override is not applying.
        let catalog = ModelCardCatalog::new(
            vec![keyed("ornith-test", vllm_card("Ornith-Test"))],
            vec![source("ornith-test", Err("TOML parse error".to_string()))],
            None,
        );
        assert!(matches!(
            catalog.resolve_exact("Ornith-Test"),
            Err(CatalogError::Malformed { .. })
        ));
    }

    #[test]
    fn a_mismatched_name_in_the_builtin_override_slot_errors() {
        // A file at the built-in's override key declaring another identity
        // is almost certainly a typo'd override — error with both names.
        let catalog = ModelCardCatalog::new(
            vec![keyed("ornith-test", vllm_card("Ornith-Test"))],
            vec![source("ornith-test", Ok(vllm_card("ornith-test")))],
            None,
        );
        let err = catalog.resolve_exact("Ornith-Test").unwrap_err();
        assert!(
            matches!(&err, CatalogError::NameMismatch { declared, .. } if declared == "ornith-test"),
            "got {err}"
        );
    }

    #[test]
    fn duplicate_source_keys_error_even_when_one_is_malformed() {
        // `foo.toml` beside `foo.yaml`: which one is the override is
        // ambiguous — reject, even though only one parses.
        let mut a = source("foo", Ok(vllm_card("foo")));
        a.path = PathBuf::from("/cards/foo.toml");
        let mut b = source("foo", Err("broken".to_string()));
        b.path = PathBuf::from("/cards/foo.yaml");
        let catalog = ModelCardCatalog::new(vec![], vec![b, a], None);
        let err = catalog.resolve_exact("foo").unwrap_err();
        let CatalogError::Duplicate { paths, .. } = &err else {
            panic!("expected Duplicate, got {err}");
        };
        assert_eq!(
            paths,
            &[
                PathBuf::from("/cards/foo.toml"),
                PathBuf::from("/cards/foo.yaml")
            ],
            "paths are in stable sorted order"
        );
    }

    #[test]
    fn two_files_claiming_one_logical_identity_is_a_duplicate_error() {
        let catalog = ModelCardCatalog::new(
            vec![],
            vec![
                source("b", Ok(vllm_card("model-x"))),
                source("a", Ok(vllm_card("model-x"))),
            ],
            None,
        );
        let err = catalog.resolve_exact("model-x").unwrap_err();
        let CatalogError::Duplicate { paths, .. } = &err else {
            panic!("expected Duplicate, got {err}");
        };
        assert_eq!(
            paths,
            &[
                PathBuf::from("/cards/a.toml"),
                PathBuf::from("/cards/b.toml")
            ],
            "stable sorted paths"
        );
    }

    #[test]
    fn a_duplicate_under_the_selected_overrides_own_key_is_rejected() {
        // The winning override lives at key `alias` (neither the requested
        // name nor a built-in slot); a second file shares that key. The slot
        // is contended — reject rather than trust which file "won".
        let mut a = source("alias", Ok(vllm_card("model-x")));
        a.path = PathBuf::from("/cards/alias.toml");
        let mut b = source("alias", Err("broken".to_string()));
        b.path = PathBuf::from("/cards/alias.yaml");
        let catalog = ModelCardCatalog::new(vec![], vec![a, b], None);
        assert!(matches!(
            catalog.resolve_exact("model-x"),
            Err(CatalogError::Duplicate { .. })
        ));
    }

    #[test]
    fn an_unrelated_malformed_file_does_not_poison_a_good_card() {
        let catalog = ModelCardCatalog::new(
            vec![],
            vec![
                source("broken", Err("TOML parse error".to_string())),
                source("good", Ok(vllm_card("good"))),
            ],
            None,
        );
        assert!(catalog.resolve_exact("good").is_ok());
        assert!(matches!(
            catalog.resolve_exact("broken"),
            Err(CatalogError::Malformed { .. })
        ));
    }

    #[test]
    fn a_file_keyed_by_the_name_but_declaring_another_is_a_name_mismatch() {
        let catalog = ModelCardCatalog::new(
            vec![],
            vec![source("team-reasoner", Ok(vllm_card("Team-Reasoner")))],
            None,
        );
        let err = catalog.resolve_exact("team-reasoner").unwrap_err();
        assert!(
            matches!(&err, CatalogError::NameMismatch { declared, .. } if declared == "Team-Reasoner"),
            "got {err}"
        );
    }

    #[test]
    fn an_invalid_resolved_card_reports_invalid_not_notfound() {
        // Parses fine, but has no backend — the merged result fails
        // validation, and the error says so instead of pretending absence.
        let catalog = ModelCardCatalog::new(
            vec![],
            vec![source("caps-only", Ok(card("name = \"caps-only\"\n")))],
            None,
        );
        assert!(matches!(
            catalog.resolve_exact("caps-only"),
            Err(CatalogError::Invalid { .. })
        ));
    }

    #[test]
    fn family_defaults_apply_to_the_final_family_after_the_merge() {
        // The built-in declares no family; the override joins qwen3. The
        // family layer must fill from the FINAL (merged) family — under the
        // old pre-merge order the override's family got no defaults at all.
        let catalog = ModelCardCatalog::new(
            vec![keyed("fam", vllm_card("fam"))],
            vec![source(
                "fam",
                Ok(card("name = \"fam\"\nfamily = \"qwen3\"\n")),
            )],
            None,
        );
        let resolved = catalog.resolve_exact("fam").expect("valid");
        let v = resolved.vllm.expect("vllm block");
        assert_eq!(v.reasoning_parser.as_deref(), Some("qwen3"));
        assert_eq!(
            v.served_name.as_deref(),
            Some("fam"),
            "the card's own declarations still win over the family layer"
        );
    }

    #[test]
    fn a_dropin_only_card_takes_the_identical_path() {
        let dropin = card(
            "name = \"solo\"\nbackend = \"vllm\"\nfamily = \"qwen3\"\n\
             [vllm]\nserved_name = \"solo\"\n",
        );
        let catalog = ModelCardCatalog::new(vec![], vec![source("solo", Ok(dropin))], None);
        let resolved = catalog.resolve_exact("solo").expect("valid");
        assert_eq!(
            resolved.vllm.unwrap().reasoning_parser.as_deref(),
            Some("qwen3"),
            "family defaults apply to drop-in-only cards too"
        );
    }

    #[test]
    fn construction_orders_sources_by_key_then_path_whatever_the_input_order() {
        // `load` reads the dir in filesystem order; `new` (which it
        // delegates to) owns the deterministic ordering.
        let mut y = source("m", Err("nope".to_string()));
        y.path = PathBuf::from("/cards/m.yaml");
        let mut t = source("m", Ok(vllm_card("m")));
        t.path = PathBuf::from("/cards/m.toml");
        let catalog =
            ModelCardCatalog::new(vec![], vec![source("z", Ok(vllm_card("z"))), y, t], None);
        let order: Vec<(String, PathBuf)> = catalog
            .sources()
            .iter()
            .map(|s| (s.key.clone(), s.path.clone()))
            .collect();
        assert_eq!(
            order,
            [
                ("m".to_string(), PathBuf::from("/cards/m.toml")),
                ("m".to_string(), PathBuf::from("/cards/m.yaml")),
                ("z".to_string(), PathBuf::from("/cards/z.toml")),
            ]
        );
    }

    #[test]
    fn names_lists_builtins_and_parsed_dropins_sorted() {
        let catalog = ModelCardCatalog::new(
            vec![keyed("bbb", vllm_card("bbb"))],
            vec![
                source("aaa", Ok(vllm_card("aaa"))),
                source("zzz", Err("broken".to_string())),
            ],
            None,
        );
        assert_eq!(catalog.names(), ["aaa", "bbb"]);
    }
}
