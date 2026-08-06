//! Process-level coverage for `newt providers` — the preset roster and the
//! Hermes Agent importer.
//!
//! Real-fs tier: builds a fake `$HERMES_HOME` (a declarative plugin, a
//! hook-bearing subclass plugin, an oauth-auth declarative plugin, and a
//! legacy list-shape `config.yaml` carrying an inline api_key) and drives
//! the actual binary with `NEWT_CONFIG_DIR` pinned to a tempdir. Serialized
//! onto the `real_fs` lane — parallel tempdir churn intermittently fails
//! under load (see CLAUDE.md's testing tiers).

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

const SECRET: &str = "sk-boxcorp-XYZZY-secret";

/// The canonical declarative plugin (the Nous docs example, verbatim).
const ACME_PLUGIN: &str = r#"from providers import register_provider
from providers.base import ProviderProfile

acme = ProviderProfile(
    name="acme-inference",
    aliases=("acme",),
    display_name="Acme Inference",
    signup_url="https://acme.example.com/keys",
    env_vars=("ACME_API_KEY", "ACME_BASE_URL"),
    base_url="https://api.acme.example.com/v1",
    auth_type="api_key",
    default_aux_model="acme-small-fast",
    fallback_models=("acme-large-v3", "acme-small-fast"),
)
register_provider(acme)
"#;

/// A hook-bearing plugin (subclasses ProviderProfile) — must skip.
const FANCY_PLUGIN: &str = r#""""Fancy provider with hooks."""

from typing import Any

from providers import register_provider
from providers.base import ProviderProfile


class FancyProvider(ProviderProfile):
    def build_extra_body(self, **context: Any):
        return {}


register_provider(FancyProvider(name="fancy", base_url="https://f.example.com/v1"))
"#;

/// Declarative but oauth-auth — parses fine, must skip with the auth reason.
const OAUTHY_PLUGIN: &str = r#"from providers import register_provider
from providers.base import ProviderProfile

oauthy = ProviderProfile(
    name="oauthy",
    base_url="https://api.oauthy.example.com/v1",
    auth_type="oauth_device_code",
)
register_provider(oauthy)
"#;

/// Build the fake HERMES_HOME. `config.yaml` uses the REAL legacy shape:
/// `custom_providers:` is a LIST of entries with inline names.
fn write_hermes_home(home: &Path) {
    let plugins = home.join("plugins").join("model-providers");
    for (dir, body) in [
        ("acme", ACME_PLUGIN),
        ("fancy", FANCY_PLUGIN),
        ("oauthy", OAUTHY_PLUGIN),
    ] {
        let d = plugins.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("__init__.py"), body).unwrap();
    }
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "model:\n  default: box-large\ncustom_providers:\n  - name: boxcorp\n    base_url: \"http://boxes.example:8000/v1\"\n    api_key: \"{SECRET}\"\n    models:\n      - box-large\n"
        ),
    )
    .unwrap();
}

/// Recursive scan: does any file under `dir` contain `needle`?
fn tree_contains(dir: &Path, needle: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if tree_contains(&path, needle) {
                return true;
            }
        } else if std::fs::read_to_string(&path)
            .map(|body| body.contains(needle))
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn newt() -> Command {
    Command::cargo_bin("newt").unwrap()
}

#[serial_test::serial(real_fs)]
#[test]
fn import_hermes_writes_presets_skips_honestly_and_never_stores_keys() {
    let hermes = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    write_hermes_home(hermes.path());

    let assert = newt()
        .env("NEWT_CONFIG_DIR", config.path())
        .args(["providers", "import-hermes", "--hermes-home"])
        .arg(hermes.path())
        .assert()
        .success()
        // One honest line per skip: the hook-bearing subclass, the oauth
        // auth flow, and the inline-api-key instruction.
        .stdout(predicate::str::contains("subclasses ProviderProfile"))
        .stdout(predicate::str::contains("FancyProvider"))
        .stdout(predicate::str::contains("oauth_device_code"))
        .stdout(predicate::str::contains(
            "found an api_key for boxcorp in Hermes config.yaml",
        ))
        .stdout(predicate::str::contains("export BOXCORP_API_KEY"))
        .stdout(predicate::str::contains(
            "imported 2, skipped 2 (see reasons above)",
        ));
    // The key VALUE never surfaces on stdout/stderr either.
    let output = assert.get_output();
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));

    // Exactly the two importable presets were written.
    let providers_dir = config.path().join("providers");
    let mut names: Vec<String> = std::fs::read_dir(&providers_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["acme-inference.toml", "boxcorp.toml"]);

    // The acme body transposed field-for-field, carrying the unmirrored
    // kwarg as a comment.
    let acme = std::fs::read_to_string(providers_dir.join("acme-inference.toml")).unwrap();
    assert!(acme.contains("name = \"acme-inference\""));
    assert!(acme.contains("base_url = \"https://api.acme.example.com/v1\""));
    assert!(acme.contains("# hermes: default_aux_model = \"acme-small-fast\" (not used by newt)"));

    // No byte of the api_key value anywhere under the config dir.
    assert!(
        !tree_contains(config.path(), SECRET),
        "inline api_key leaked into the newt config tree"
    );
    // The boxcorp preset names the env var instead.
    let boxcorp = std::fs::read_to_string(providers_dir.join("boxcorp.toml")).unwrap();
    assert!(boxcorp.contains("BOXCORP_API_KEY"));

    // Re-run: both targets exist — skipped, nothing imported.
    newt()
        .env("NEWT_CONFIG_DIR", config.path())
        .args(["providers", "import-hermes", "--hermes-home"])
        .arg(hermes.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "already exists (use --force to overwrite)",
        ))
        .stdout(predicate::str::contains(
            "imported 0, skipped 4 (see reasons above)",
        ));

    // --force overwrites: clobber a file, re-import, content is restored.
    std::fs::write(providers_dir.join("acme-inference.toml"), "clobbered").unwrap();
    newt()
        .env("NEWT_CONFIG_DIR", config.path())
        .args(["providers", "import-hermes", "--force", "--hermes-home"])
        .arg(hermes.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "imported 2, skipped 2 (see reasons above)",
        ));
    let restored = std::fs::read_to_string(providers_dir.join("acme-inference.toml")).unwrap();
    assert!(restored.contains("base_url = \"https://api.acme.example.com/v1\""));
}

#[serial_test::serial(real_fs)]
#[test]
fn import_hermes_dry_run_writes_nothing() {
    let hermes = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    write_hermes_home(hermes.path());

    newt()
        .env("NEWT_CONFIG_DIR", config.path())
        .args(["providers", "import-hermes", "--dry-run", "--hermes-home"])
        .arg(hermes.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("would-write acme-inference"))
        .stdout(predicate::str::contains("would-write boxcorp"))
        .stdout(predicate::str::contains(
            "imported 2, skipped 2 (see reasons above)",
        ));

    assert!(
        !config.path().join("providers").exists(),
        "--dry-run must write nothing"
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn providers_list_smoke_contains_builtin_rows() {
    let config = tempfile::tempdir().unwrap();
    newt()
        .env("NEWT_CONFIG_DIR", config.path())
        .args(["providers", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("openai"))
        .stdout(predicate::str::contains("anthropic"));
}
