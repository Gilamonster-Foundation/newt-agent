//! **Lifecycle phases** — a repo's build/dev commands as configurable DATA, not
//! hard-coded `match` arms (#880).
//!
//! A fixed, extensible vocabulary of phase names ([`Phase`]) — `setup`, `format`,
//! `lint`, `test`, `check`, `clean` — each mapped to a single command. The
//! mapping is resolved:
//!
//! 1. the repo's **`[lifecycle]`** table in `.newt/config.toml` (project-local,
//!    tracked — the repo declares its own `just clean` / `just test` / …),
//!    published once via [`set_lifecycle_override`];
//! 2. else the matching **tooling packs** — per-ecosystem defaults as data
//!    (built-in + `~/.newt/tooling/*.toml` drop-ins), the sibling of the
//!    language-pack model. A polyglot repo matches SEVERAL; an unknown one NONE.
//!
//! The crew consumes `format` (its normalize-before-commit step) today; the other
//! phases are declared-and-available for `newt lifecycle <phase>` and future
//! wiring. Adding/removing a phase name is a small, deliberate code change — the
//! set is intentionally hard-coded (a stable vocabulary), while the *commands*
//! are data.

use std::path::Path;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// The fixed lifecycle-phase vocabulary. Extend deliberately in a future build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Resolve deps / prepare a fresh checkout (e.g. `just install`).
    Setup,
    /// Auto-format the tree (e.g. `cargo fmt`). The crew's normalize step.
    Format,
    /// Static analysis (e.g. `cargo clippy`).
    Lint,
    /// Run the tests (e.g. `cargo test`).
    Test,
    /// The gate a change must pass (e.g. `just check`). The crew's verify.
    Check,
    /// Remove build / scratch artifacts (e.g. `just clean`).
    Clean,
}

impl Phase {
    /// Every phase, in lifecycle order.
    pub const ALL: [Self; 6] = [
        Self::Setup,
        Self::Format,
        Self::Lint,
        Self::Test,
        Self::Check,
        Self::Clean,
    ];

    /// The stable string key (the TOML field name).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Format => "format",
            Self::Lint => "lint",
            Self::Test => "test",
            Self::Check => "check",
            Self::Clean => "clean",
        }
    }
}

/// A per-phase command map — used both as a pack's default commands (`[phases]`)
/// and as the repo's `[lifecycle]` override. Every field optional (an unset phase
/// falls through to the next source).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseCommands {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean: Option<String>,
}

impl PhaseCommands {
    /// The command for `phase`, if set.
    #[must_use]
    pub fn get(&self, phase: Phase) -> Option<&str> {
        match phase {
            Phase::Setup => self.setup.as_deref(),
            Phase::Format => self.format.as_deref(),
            Phase::Lint => self.lint.as_deref(),
            Phase::Test => self.test.as_deref(),
            Phase::Check => self.check.as_deref(),
            Phase::Clean => self.clean.as_deref(),
        }
    }
}

/// A toolchain's lifecycle commands, as data. Selected for a repo when ANY of its
/// `detect` marker files exists at the repo root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolingPack {
    /// Pack name — the merge / drop-in key (`rust`, `python`, …). A drop-in with
    /// an existing name overrides the built-in; a new name adds a toolchain.
    pub name: String,
    /// Marker files whose presence at the repo root selects this toolchain.
    /// By default ANY match selects it; with `require_all` **every** marker must
    /// exist (e.g. PyO3 = `Cargo.toml` AND `pyproject.toml`).
    #[serde(default)]
    pub detect: Vec<String>,
    /// Require ALL `detect` markers (a compound toolchain like Rust+PyO3), and
    /// SUPERSEDE the component packs it subsumes (so a PyO3 repo uses the `pyo3`
    /// pack, not `rust` + `python` on top of it).
    #[serde(default)]
    pub require_all: bool,
    /// The toolchain's default command per lifecycle phase (`[phases]`).
    #[serde(default)]
    pub phases: PhaseCommands,
}

impl ToolingPack {
    /// Does this pack's detection match a repo at `repo_dir`?
    #[must_use]
    fn matches(&self, repo_dir: &Path) -> bool {
        if self.detect.is_empty() {
            return false;
        }
        let exists = |m: &String| repo_dir.join(m).exists();
        if self.require_all {
            self.detect.iter().all(exists)
        } else {
            self.detect.iter().any(exists)
        }
    }
}

/// The built-in tooling packs — worked examples a contributor copies into a
/// `~/.newt/tooling/<name>.toml` drop-in. Override any by name; add new ones.
#[must_use]
pub fn builtin_tooling_packs() -> Vec<ToolingPack> {
    let pack = |name: &str, marker: &str, phases: PhaseCommands| ToolingPack {
        name: name.into(),
        detect: vec![marker.into()],
        require_all: false,
        phases,
    };
    let some = |s: &str| Some(s.to_string());
    vec![
        // Rust + PyO3 (maturin) — a compound toolchain: BOTH Cargo.toml AND
        // pyproject.toml. `require_all` supersedes the plain `rust` + `python`
        // packs so a PyO3 repo gets one coherent lifecycle, not three stacked.
        ToolingPack {
            name: "pyo3".into(),
            detect: vec!["Cargo.toml".into(), "pyproject.toml".into()],
            require_all: true,
            phases: PhaseCommands {
                setup: some("maturin develop"),
                format: some("cargo fmt && ruff format ."),
                lint: some("cargo clippy --all-targets -- -D warnings && ruff check ."),
                test: some("maturin develop && pytest -x"),
                check: some(
                    "cargo fmt -- --check && cargo clippy --all-targets -- -D warnings \
                     && maturin develop && cargo test && pytest -x",
                ),
                clean: some("cargo clean"),
            },
        },
        pack(
            "rust",
            "Cargo.toml",
            PhaseCommands {
                setup: some("cargo fetch"),
                format: some("cargo fmt"),
                lint: some("cargo clippy --all-targets -- -D warnings"),
                test: some("cargo test"),
                check: some("cargo fmt -- --check && cargo test"),
                clean: some("cargo clean"),
            },
        ),
        pack(
            "python",
            "pyproject.toml",
            PhaseCommands {
                format: some("ruff format ."),
                lint: some("ruff check ."),
                test: some("pytest -x"),
                check: some("pytest -x"),
                ..PhaseCommands::default()
            },
        ),
        pack(
            "go",
            "go.mod",
            PhaseCommands {
                format: some("gofmt -w ."),
                lint: some("go vet ./..."),
                test: some("go test ./..."),
                check: some("go test ./..."),
                clean: some("go clean"),
                ..PhaseCommands::default()
            },
        ),
    ]
}

/// Load tooling packs from a drop-in dir (`<dir>/*.toml`, one per file). Malformed
/// files are skipped with a warning; a missing dir → `[]`. (The tolerant drop-in
/// contract shared with the language-pack / `[backends]` dirs.)
#[must_use]
pub fn load_tooling_packs_from_dir(dir: &Path) -> Vec<ToolingPack> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        match std::fs::read_to_string(&path).map(|s| toml::from_str::<ToolingPack>(&s)) {
            Ok(Ok(pack)) => out.push(pack),
            Ok(Err(e)) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping malformed tooling pack");
            }
            Err(_) => {}
        }
    }
    out
}

/// Merge pack layers **by name** — later layers win (built-ins < drop-in dir).
#[must_use]
pub fn merge_tooling_packs(layers: Vec<Vec<ToolingPack>>) -> Vec<ToolingPack> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: std::collections::HashMap<String, ToolingPack> =
        std::collections::HashMap::new();
    for layer in layers {
        for pack in layer {
            if !by_name.contains_key(&pack.name) {
                order.push(pack.name.clone());
            }
            by_name.insert(pack.name.clone(), pack);
        }
    }
    order
        .into_iter()
        .filter_map(|n| by_name.remove(&n))
        .collect()
}

/// Every `phase` command from the packs whose `detect` markers exist in
/// `repo_dir`, in pack order. **None** matching → empty; **several** → each
/// matching toolchain's command (a polyglot repo). Pure.
#[must_use]
pub fn phase_commands_from_packs(
    repo_dir: &Path,
    phase: Phase,
    packs: &[ToolingPack],
) -> Vec<String> {
    matching_packs(repo_dir, packs)
        .into_iter()
        .filter_map(|p| p.phases.get(phase).map(str::to_string))
        .collect()
}

/// The packs that select `repo_dir`, after **supersession**: a matching
/// `require_all` pack drops the matching packs whose `detect` markers are a
/// strict subset of its own — so a Rust+PyO3 repo runs the `pyo3` pack, not
/// `rust` + `python` stacked underneath it.
fn matching_packs<'a>(repo_dir: &Path, packs: &'a [ToolingPack]) -> Vec<&'a ToolingPack> {
    let matched: Vec<&ToolingPack> = packs.iter().filter(|p| p.matches(repo_dir)).collect();
    let supersets: Vec<&[String]> = matched
        .iter()
        .filter(|p| p.require_all)
        .map(|p| p.detect.as_slice())
        .collect();
    matched
        .iter()
        .filter(|p| {
            !supersets
                .iter()
                .any(|sup| sup.len() > p.detect.len() && p.detect.iter().all(|m| sup.contains(m)))
        })
        .copied()
        .collect()
}

/// The repo's `[lifecycle]` override, published once when config resolves.
static LIFECYCLE_OVERRIDE: OnceLock<PhaseCommands> = OnceLock::new();

/// Publish the repo `[lifecycle]` table (from `Config::resolve`), once.
pub fn set_lifecycle_override(cmds: PhaseCommands) {
    let _ = LIFECYCLE_OVERRIDE.set(cmds);
}

/// The resolved commands for `phase` in `repo_dir`:
/// the repo `[lifecycle].<phase>` override (a single command) wins; else every
/// matching tooling pack's default (built-in merged under `~/.newt/tooling/`).
/// Configuration over Convention; empty = nothing declared for this phase.
#[must_use]
pub fn resolved_phase_commands(repo_dir: &Path, phase: Phase) -> Vec<String> {
    if let Some(cmd) = LIFECYCLE_OVERRIDE.get().and_then(|c| c.get(phase)) {
        return vec![cmd.to_string()];
    }
    let dropins = crate::config::Config::user_config_dir()
        .map(|d| load_tooling_packs_from_dir(&d.join("tooling")))
        .unwrap_or_default();
    let packs = merge_tooling_packs(vec![builtin_tooling_packs(), dropins]);
    phase_commands_from_packs(repo_dir, phase, &packs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_keys_are_stable() {
        assert_eq!(Phase::Format.as_str(), "format");
        assert_eq!(Phase::Check.as_str(), "check");
        assert_eq!(Phase::ALL.len(), 6);
    }

    #[test]
    fn builtin_format_is_data_per_ecosystem() {
        let by = |n: &str| {
            builtin_tooling_packs()
                .into_iter()
                .find(|p| p.name == n)
                .unwrap()
        };
        assert_eq!(by("rust").phases.format.as_deref(), Some("cargo fmt"));
        assert_eq!(by("python").phases.format.as_deref(), Some("ruff format ."));
        assert_eq!(by("go").phases.check.as_deref(), Some("go test ./..."));
    }

    #[test]
    fn none_matching_yields_no_commands() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            phase_commands_from_packs(dir.path(), Phase::Format, &builtin_tooling_packs())
                .is_empty()
        );
    }

    #[test]
    fn multiple_toolchains_each_contribute_a_command() {
        // Polyglot (Rust + Go): the `format` phase runs BOTH formatters — a case a
        // hard-coded if/else chain could not express.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("go.mod"), "").unwrap();
        let cmds = phase_commands_from_packs(dir.path(), Phase::Format, &builtin_tooling_packs());
        assert!(cmds.contains(&"cargo fmt".to_string()), "{cmds:?}");
        assert!(cmds.contains(&"gofmt -w .".to_string()), "{cmds:?}");
    }

    #[test]
    fn pyo3_require_all_supersedes_rust_and_python() {
        // Rust + PyO3 (BOTH markers) → the `pyo3` pack ONLY, not rust + python
        // stacked. One coherent format + one check (with `maturin develop`).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(
            phase_commands_from_packs(dir.path(), Phase::Format, &builtin_tooling_packs()),
            vec!["cargo fmt && ruff format .".to_string()]
        );
        let check = phase_commands_from_packs(dir.path(), Phase::Check, &builtin_tooling_packs());
        assert_eq!(check.len(), 1, "one coherent check, not three: {check:?}");
        assert!(check[0].contains("maturin develop"), "{check:?}");
        // A pure-Rust repo still resolves just `rust`.
        let d2 = tempfile::tempdir().unwrap();
        std::fs::write(d2.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(
            phase_commands_from_packs(d2.path(), Phase::Format, &builtin_tooling_packs()),
            vec!["cargo fmt".to_string()]
        );
    }

    #[test]
    fn a_dropin_overrides_by_name_and_adds_new_toolchains() {
        let dropins = vec![
            ToolingPack {
                name: "rust".into(),
                detect: vec!["Cargo.toml".into()],
                require_all: false,
                phases: PhaseCommands {
                    format: Some("cargo +nightly fmt".into()),
                    ..PhaseCommands::default()
                },
            },
            ToolingPack {
                name: "zig".into(),
                detect: vec!["build.zig".into()],
                require_all: false,
                phases: PhaseCommands {
                    format: Some("zig fmt .".into()),
                    ..PhaseCommands::default()
                },
            },
        ];
        let merged = merge_tooling_packs(vec![builtin_tooling_packs(), dropins]);
        let rust = merged.iter().find(|p| p.name == "rust").unwrap();
        assert_eq!(rust.phases.format.as_deref(), Some("cargo +nightly fmt"));
        assert!(merged.iter().any(|p| p.name == "zig"));
    }

    #[test]
    fn pack_and_lifecycle_parse_from_toml() {
        let p: ToolingPack = toml::from_str(
            "name = \"zig\"\ndetect = [\"build.zig\"]\n[phases]\nformat = \"zig fmt .\"\n",
        )
        .unwrap();
        assert_eq!(p.phases.format.as_deref(), Some("zig fmt ."));
        // The repo `[lifecycle]` table is the same shape (flat, no [phases]).
        let lc: PhaseCommands = toml::from_str(
            "format = \"cargo fmt\"\ncheck = \"just check\"\nclean = \"just clean\"\n",
        )
        .unwrap();
        assert_eq!(lc.check.as_deref(), Some("just check"));
    }
}
