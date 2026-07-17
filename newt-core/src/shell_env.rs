//! File-sourced shell env import — the `~/.newt/shell-env/` drop-in dir
//! (EPIC #1243 Leg 2 fix b). The deliberate import surface for tokens and
//! support vars into the brush/confined-shell boundary.
//!
//! Each regular file in `~/.newt/shell-env/` is ONE env var the confined shell
//! receives: **filename = var name, first non-empty trimmed line = value**.
//! This honors two standing laws:
//! - **tokens live in FILES, never in `config.toml`** (#1219 config-lean): the
//!   secret is the file's contents; nothing inline, no `[shell]` clutter, and
//!   `~/.newt/shell-env/` joins the house drop-in family (`backends/`, `ocap/`,
//!   model cards, nudger profiles).
//! - **values never enter newt's own process env**: they flow file → the
//!   bridle `env` seam → the confined shell, so a token cannot leak through
//!   `newt`'s environment (the exact gap `[shell] env_passthrough` — which only
//!   mirrors ambient NAMES — could not close).
//!
//! This is the *import* surface (file-sourced secrets), deliberately distinct
//! from *passthrough* (ambient-name mirror). Pure core ([`env_from_entries`]) +
//! a thin fs reader ([`from_config_dir`]).

use std::collections::BTreeMap;
use std::path::Path;

/// The drop-in dir name, resolved beside `~/.newt/config.toml`.
pub const SHELL_ENV_DIR: &str = "shell-env";

/// A token file's value: the first non-empty line, trimmed — the same
/// discipline as `Config::resolve_api_key` (a token file is usually a single
/// line, and a trailing newline must not become part of the secret). `None`
/// when the file is blank.
pub fn token_value(contents: &str) -> Option<String> {
    contents
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// A conservative POSIX-ish env-var-name check: non-empty, ASCII letters /
/// digits / underscore, not starting with a digit. Keeps stray non-var files
/// (`.gitkeep`, `README.md`, editor swap files) from becoming env vars.
pub fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Pure: fold `(filename, contents)` entries into an env map. A blank file or a
/// filename that is not a valid env-var name is skipped. Deterministic order
/// (BTreeMap) so a capture/dispatch is reproducible.
pub fn env_from_entries(entries: &[(String, String)]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (name, contents) in entries {
        if !is_env_name(name) {
            continue;
        }
        if let Some(v) = token_value(contents) {
            map.insert(name.clone(), v);
        }
    }
    map
}

/// Read `<config dir>/shell-env/` into an env map (the fs wrapper over
/// [`env_from_entries`]). `config_path` is the config FILE path
/// (`~/.newt/config.toml`); the dir lives beside it. A missing or unreadable
/// dir yields an empty map — the feature is opt-in by dropping files in.
pub fn from_config_dir(config_path: &Path) -> BTreeMap<String, String> {
    let dir = config_path.with_file_name(SHELL_ENV_DIR);
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        return BTreeMap::new();
    };
    let mut entries = Vec::new();
    for entry in read_dir.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(contents) = std::fs::read_to_string(entry.path()) {
            entries.push((name, contents));
        }
    }
    env_from_entries(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_value_takes_first_non_empty_trimmed_line() {
        assert_eq!(token_value("ghp_secret\n"), Some("ghp_secret".to_string()));
        assert_eq!(
            token_value("\n  \n  tok-en  \n more"),
            Some("tok-en".to_string()),
            "leading blanks skipped, value trimmed, later lines ignored"
        );
        assert_eq!(token_value(""), None);
        assert_eq!(token_value("   \n\t\n"), None, "all-blank → None");
    }

    #[test]
    fn is_env_name_accepts_vars_and_rejects_stray_files() {
        for ok in ["GITHUB_TOKEN", "HOME", "_x", "A1", "NEWT_VENV"] {
            assert!(is_env_name(ok), "{ok} should be a valid env name");
        }
        for bad in ["", ".gitkeep", "README.md", "1ABC", "a-b", "swap~", "a.b"] {
            assert!(!is_env_name(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn env_from_entries_maps_valid_files_and_skips_the_rest() {
        let entries = vec![
            ("GITHUB_TOKEN".to_string(), "ghp_abc\n".to_string()),
            (
                "MODULEX_STORE".to_string(),
                "/home/x/.modulex/store.db".to_string(),
            ),
            (".gitkeep".to_string(), "keep".to_string()), // not an env name
            ("README.md".to_string(), "docs".to_string()), // not an env name
            ("BLANK".to_string(), "  \n ".to_string()),   // blank → skipped
        ];
        let map = env_from_entries(&entries);
        assert_eq!(map.len(), 2);
        assert_eq!(map["GITHUB_TOKEN"], "ghp_abc");
        assert_eq!(map["MODULEX_STORE"], "/home/x/.modulex/store.db");
        assert!(!map.contains_key(".gitkeep"));
        assert!(!map.contains_key("BLANK"));
    }
}
