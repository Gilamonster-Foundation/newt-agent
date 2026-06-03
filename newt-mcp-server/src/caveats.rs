//! Sourcing the session's **granted** [`Caveats`] — the leash the
//! Caveats-confined `shell_run` tool is dispatched under.
//!
//! `shell_run` used to be an UNCONFINED `sh -c` (PR #125): full ambient
//! authority, only a timeout. P1 supersedes it with agent-bridle's
//! brush-backed confined shell, which runs every command under a granted
//! [`Caveats`] leash. This module decides *what* that grant is.
//!
//! Resolution order (first hit wins):
//!
//! 1. **`~/.newt/config.toml`**, table **`[caveats]`** — the persistent
//!    per-host leash. Same field / enum serde shape `agent-bridle-mcp`
//!    uses, e.g. `exec = { only = ["git", "cargo"] }`.
//! 2. **Default [`Caveats::top()`]** — UNCONFINED. Emits a prominent stderr
//!    WARNING, because an unconfined leash means `shell_run` can still run
//!    anything (the historical behaviour) — the warning makes that visible.
//!
//! The TOML shape is identical to the Rust `Caveats` serde derive:
//! string axes are either `"all"` or `{ only = [..] }`; `max_calls` is
//! either `"unlimited"` or `{ at_most = N }`. Example:
//!
//! ```toml
//! [caveats]
//! fs_read = "all"
//! fs_write = "all"
//! exec = { only = ["git", "cargo"] }
//! net = "all"
//! max_calls = "unlimited"
//! valid_for_generation = "all"
//! ```

use std::path::{Path, PathBuf};

use agent_mesh_protocol::Caveats;

/// Where the granted leash came from — surfaced in the startup log so an
/// operator can tell, at a glance, whether `shell_run` is confined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaveatsSource {
    /// Loaded from `~/.newt/config.toml` `[caveats]`.
    ConfigFile(PathBuf),
    /// No `[caveats]` configured — defaulted to [`Caveats::top()`] (UNCONFINED).
    UnconfinedDefault,
}

/// The resolved leash plus where it came from.
#[derive(Debug, Clone)]
pub struct GrantedCaveats {
    /// The granted authority every `shell_run` dispatch is confined to.
    pub caveats: Caveats,
    /// Provenance of `caveats`, for the startup banner.
    pub source: CaveatsSource,
}

impl GrantedCaveats {
    /// Resolve the granted leash from `~/.newt/config.toml`, using the real
    /// `$HOME`. A missing file (or missing `[caveats]` table) falls through to
    /// the UNCONFINED default rather than erroring.
    ///
    /// On any failure to *parse* a present config, this logs a warning and
    /// falls back to the unconfined default — `shell_run` must remain usable
    /// even if the host config is malformed, but the operator is told.
    #[must_use]
    pub fn load() -> Self {
        let home = home_dir();
        match Self::resolve(home.as_deref()) {
            Ok(g) => g,
            Err(e) => {
                eprintln!(
                    "WARNING: newt-mcp-server could not parse ~/.newt/config.toml [caveats]: {e}; \
                     shell_run is running UNCONFINED (full ambient authority)."
                );
                Self {
                    caveats: Caveats::top(),
                    source: CaveatsSource::UnconfinedDefault,
                }
            }
        }
    }

    /// Pure resolution given the (optional) home dir. Factored out so tests
    /// drive it without touching real process state.
    ///
    /// # Errors
    /// Returns an error only when a *present* config file is malformed (an
    /// unparsable `[caveats]` table) — a missing file is not an error, it
    /// falls through to the unconfined default.
    pub fn resolve(home: Option<&Path>) -> anyhow::Result<Self> {
        if let Some(home) = home {
            let path = home.join(".newt").join("config.toml");
            if path.is_file() {
                let caveats = load_from_config(&path)?;
                return Ok(Self {
                    caveats,
                    source: CaveatsSource::ConfigFile(path),
                });
            }
        }

        Ok(Self {
            caveats: Caveats::top(),
            source: CaveatsSource::UnconfinedDefault,
        })
    }

    /// A human-readable, one-line provenance banner for stderr. When the leash
    /// is the unconfined default, the line is a prominent WARNING.
    #[must_use]
    pub fn banner(&self) -> String {
        match &self.source {
            CaveatsSource::ConfigFile(p) => {
                format!(
                    "newt-mcp-server: shell_run confined by {} [caveats]",
                    p.display()
                )
            }
            CaveatsSource::UnconfinedDefault => {
                "WARNING: newt-mcp-server shell_run is running UNCONFINED \
                 (no ~/.newt/config.toml [caveats]); shell_run can run any \
                 command with full ambient authority. Add a [caveats] table to \
                 ~/.newt/config.toml (e.g. exec = { only = [\"git\", \"cargo\"] }) \
                 to confine it."
                    .to_string()
            }
        }
    }

    /// Emit the provenance banner to stderr at startup. The unconfined default
    /// is a loud warning so an operator never confines-by-omission unknowingly.
    pub fn warn_to_stderr(&self) {
        eprintln!("{}", self.banner());
    }
}

/// The shape of `~/.newt/config.toml` we care about: a `[caveats]` table
/// deserializing straight into [`Caveats`]. Other top-level keys are ignored,
/// so the file can carry unrelated newt config too.
#[derive(serde::Deserialize)]
struct Config {
    caveats: Option<Caveats>,
}

/// Read and parse the `[caveats]` table from a config file. A file that exists
/// but has no `[caveats]` table is treated as "no grant configured" → top, to
/// keep the same fall-through semantics as a missing file.
fn load_from_config(path: &Path) -> anyhow::Result<Caveats> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    let cfg: Config = toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("cannot parse {} [caveats]: {e}", path.display()))?;
    Ok(cfg.caveats.unwrap_or_else(Caveats::top))
}

/// Resolve `$HOME` without pulling in a dirs crate (lean dep budget).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_mesh_protocol::{CountBound, Scope};

    /// A unique temp dir under the test temp root, no external crate needed.
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "newt-mcp-caveats-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn config_toml_is_parsed_with_the_mesh_shape() {
        let dir = tempdir();
        let nd = dir.join(".newt");
        std::fs::create_dir_all(&nd).unwrap();
        std::fs::write(
            nd.join("config.toml"),
            r#"
[caveats]
fs_read = "all"
fs_write = "all"
exec = { only = ["git", "cargo"] }
net = "all"
max_calls = { at_most = 5 }
valid_for_generation = "all"
"#,
        )
        .unwrap();

        let g = GrantedCaveats::resolve(Some(&dir)).unwrap();
        assert!(matches!(g.source, CaveatsSource::ConfigFile(_)));
        assert_eq!(
            g.caveats.exec,
            Scope::only(["git".to_string(), "cargo".to_string()])
        );
        assert_eq!(g.caveats.max_calls, CountBound::AtMost(5));
        assert!(!g.banner().starts_with("WARNING"));
    }

    #[test]
    fn missing_config_is_unconfined_top_with_warning_banner() {
        let dir = tempdir(); // no config file inside
        let g = GrantedCaveats::resolve(Some(&dir)).unwrap();
        assert_eq!(g.source, CaveatsSource::UnconfinedDefault);
        assert_eq!(g.caveats, Caveats::top());
        assert!(g.banner().contains("UNCONFINED"), "banner: {}", g.banner());
        assert!(g.banner().starts_with("WARNING"));
    }

    #[test]
    fn missing_home_falls_through_to_unconfined_default() {
        let g = GrantedCaveats::resolve(None).unwrap();
        assert_eq!(g.source, CaveatsSource::UnconfinedDefault);
    }

    #[test]
    fn config_without_caveats_table_falls_through_to_top() {
        let dir = tempdir();
        let nd = dir.join(".newt");
        std::fs::create_dir_all(&nd).unwrap();
        // Valid TOML, but no [caveats] table — falls through to top.
        std::fs::write(nd.join("config.toml"), "model = \"llama3.1:8b\"\n").unwrap();

        let g = GrantedCaveats::resolve(Some(&dir)).unwrap();
        assert_eq!(g.caveats, Caveats::top());
        // Source is still ConfigFile (the file existed and parsed), but the
        // grant is top because there was no [caveats] table.
        assert!(matches!(g.source, CaveatsSource::ConfigFile(_)));
    }

    #[test]
    fn malformed_config_is_an_error() {
        let dir = tempdir();
        let nd = dir.join(".newt");
        std::fs::create_dir_all(&nd).unwrap();
        std::fs::write(nd.join("config.toml"), "[caveats]\nexec = { not = valid\n").unwrap();

        let err = GrantedCaveats::resolve(Some(&dir)).unwrap_err();
        assert!(err.to_string().contains("config.toml"), "got: {err}");
    }
}
