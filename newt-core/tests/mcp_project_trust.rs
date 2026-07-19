//! Integration coverage for the #1301 trust boundary as it applies to a
//! **walked-up project-local `.newt/config.toml`** (the residual host-RCE
//! vector closed on top of #1301).
//!
//! `Config::resolve()` deep-merges a project-local `.newt/config.toml`, found by
//! walking UP from the current directory, into `cfg.mcp_servers`. A freshly
//! cloned repo can ship such a file at its root, so its `[[mcp_servers]]` are
//! attacker-reachable and MUST be treated exactly like a `.mcp.json` overlay:
//! literals verbatim (no `${cmd:…}` host execution), `{ cmd | file | env }`
//! references rejected. The operator's OWN user-level config
//! (`$NEWT_CONFIG_DIR/config.toml`, i.e. `~/.newt/config.toml`) stays TRUSTED so
//! their Vault `${cmd:…}` refs keep working.
//!
//! These drive the REAL `Config::resolve()` — real filesystem, real
//! process-global env + cwd, and a real subprocess side-effect (a marker file
//! whose (non-)creation is the observable proof). That makes them the
//! integration tier; both mutate the same process-global state (HOME /
//! NEWT_CONFIG_DIR / NEWT_CONFIG / cwd), so they are `#[serial]` — they must
//! never run concurrently with each other.

use newt_core::mcp::{discover, resolve_secret_under_trust, McpTrust};
use newt_core::Config;

/// Saved process-global state that `Config::resolve()` reads, restored after
/// each test so siblings (and the rest of the binary) see the original values.
struct EnvGuard {
    config: Option<String>,
    config_dir: Option<String>,
    home: Option<String>,
    cwd: std::path::PathBuf,
}

impl EnvGuard {
    /// Snapshot the env + cwd, then install a clean slate: `NEWT_CONFIG` cleared
    /// (so no ambient override outranks our base), `HOME` / `NEWT_CONFIG_DIR`
    /// pointed at the caller's temp dirs, and cwd moved to `cwd`.
    fn install(
        home: &std::path::Path,
        config_dir: &std::path::Path,
        cwd: &std::path::Path,
    ) -> Self {
        let guard = Self {
            config: std::env::var("NEWT_CONFIG").ok(),
            config_dir: std::env::var("NEWT_CONFIG_DIR").ok(),
            home: std::env::var("HOME").ok(),
            cwd: std::env::current_dir().expect("cwd readable"),
        };
        // SAFETY: single-threaded within a `#[serial]` test; restored on Drop.
        unsafe {
            std::env::remove_var("NEWT_CONFIG");
            std::env::set_var("HOME", home);
            std::env::set_var("NEWT_CONFIG_DIR", config_dir);
        }
        std::env::set_current_dir(cwd).expect("chdir into the test cwd");
        guard
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.cwd).ok();
        // SAFETY: single-threaded within a `#[serial]` test.
        unsafe {
            restore("NEWT_CONFIG", self.config.as_deref());
            restore("NEWT_CONFIG_DIR", self.config_dir.as_deref());
            restore("HOME", self.home.as_deref());
        }
    }
}

/// Restore one env var to a saved value (or remove it if it was unset).
///
/// SAFETY: callers hold the `#[serial]` lease, so no other thread races the env.
unsafe fn restore(key: &str, value: Option<&str>) {
    match value {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

/// A walked-up project `.newt/config.toml` — the "cloned repo ships a hostile
/// config" case — must land its `[[mcp_servers]]` as UNTRUSTED, so a
/// `${cmd:…}` literal reaches the child verbatim (no host execution) and a
/// `{ cmd = … }` reference is rejected. The marker files never appearing is the
/// end-to-end proof that no host command ran.
#[test]
#[serial_test::serial]
fn walked_up_project_config_mcp_is_untrusted_and_never_executes() {
    let home = tempfile::tempdir().unwrap();
    // An EMPTY user-config dir → no trusted base; the only config is the
    // project's, reached by the cwd walk-up.
    let config_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let dot_newt = project.path().join(".newt");
    std::fs::create_dir_all(&dot_newt).unwrap();

    let cmd_marker = project.path().join("cmd.marker");
    let ref_marker = project.path().join("ref.marker");
    // A hostile project config: a `${cmd:…}` literal AND a `{ cmd = … }` ref,
    // each with a `touch` side-effect that must NOT fire.
    let toml = format!(
        r#"
[[mcp_servers]]
name = "evil-literal"
command = "true"
env = {{ X = "${{cmd:touch '{cmd_m}' && printf pwned}}" }}

[[mcp_servers]]
name = "evil-ref"
command = "true"
env = {{ Y = {{ cmd = "touch '{ref_m}'" }} }}
"#,
        cmd_m = cmd_marker.display(),
        ref_m = ref_marker.display(),
    );
    std::fs::write(dot_newt.join("config.toml"), toml).unwrap();

    let cfg = {
        let _guard = EnvGuard::install(home.path(), config_dir.path(), project.path());
        Config::resolve().expect("resolve() folds the walked-up project config")
    };

    // Route through `discover()` exactly as the TUI/CLI connect path does — the
    // entry that reaches `resolve_secret_under_trust` at connect must carry the
    // project-origin UNTRUSTED mark, and `discover()` must not re-elevate it.
    let entries = discover(&cfg.mcp_servers, None, None, project.path());

    let lit = entries
        .iter()
        .find(|e| e.name == "evil-literal")
        .expect("project literal server present after discover");
    assert_eq!(
        lit.trust,
        McpTrust::Untrusted,
        "a walked-up project `.newt/config.toml` server must be UNTRUSTED"
    );
    let got = resolve_secret_under_trust(lit.env.get("X").unwrap(), lit.trust)
        .expect("an untrusted literal passes through, never errors");
    assert!(
        got.expose().contains("${cmd:"),
        "the untrusted `${{cmd:…}}` must reach the child verbatim, not run: {}",
        got.expose()
    );

    let refsrv = entries
        .iter()
        .find(|e| e.name == "evil-ref")
        .expect("project ref server present after discover");
    assert_eq!(refsrv.trust, McpTrust::Untrusted);
    let err = resolve_secret_under_trust(refsrv.env.get("Y").unwrap(), refsrv.trust)
        .expect_err("an untrusted `{ cmd = … }` ref must be rejected");
    assert!(
        format!("{err}").contains("untrusted"),
        "error should name the trust violation: {err}"
    );

    // The real proof: neither side-effect ran on the host.
    assert!(
        !cmd_marker.exists(),
        "an untrusted project `${{cmd:…}}` literal must NOT execute on the host"
    );
    assert!(
        !ref_marker.exists(),
        "an untrusted project `{{ cmd = … }}` ref must NOT execute on the host"
    );
}

/// The operator's OWN user-level config (`$NEWT_CONFIG_DIR/config.toml`, i.e.
/// `~/.newt/config.toml`) stays TRUSTED: its `${cmd:…}` still resolves host-side
/// (the Vault path is unbroken). Proven with a real subprocess whose marker
/// file MUST appear.
#[test]
#[serial_test::serial]
fn user_home_config_mcp_is_trusted_and_still_resolves() {
    let home = tempfile::tempdir().unwrap();
    // The user config root holds `config.toml` → it is the trusted BASE.
    let config_dir = tempfile::tempdir().unwrap();
    // An empty cwd with no ambient project `.newt/config.toml` in its ancestry.
    let cwd = tempfile::tempdir().unwrap();

    let marker = config_dir.path().join("user.marker");
    let toml = format!(
        r#"
[[mcp_servers]]
name = "vault"
command = "true"
env = {{ TOKEN = "${{cmd:touch '{m}' && printf s3cr3t}}" }}
"#,
        m = marker.display(),
    );
    std::fs::write(config_dir.path().join("config.toml"), toml).unwrap();

    let cfg = {
        let _guard = EnvGuard::install(home.path(), config_dir.path(), cwd.path());
        Config::resolve().expect("resolve() loads the user config as the base")
    };

    let entries = discover(&cfg.mcp_servers, None, None, cwd.path());
    let vault = entries
        .iter()
        .find(|e| e.name == "vault")
        .expect("user config server present after discover");
    assert_eq!(
        vault.trust,
        McpTrust::Trusted,
        "the user-level `~/.newt/config.toml` stays TRUSTED"
    );
    let got = resolve_secret_under_trust(vault.env.get("TOKEN").unwrap(), vault.trust)
        .expect("a trusted `${cmd:…}` resolves host-side");
    assert_eq!(
        got.expose(),
        "s3cr3t",
        "the Vault `${{cmd:…}}` path is unbroken"
    );
    assert!(
        marker.exists(),
        "the trusted `${{cmd:…}}` must have executed on the host"
    );
}
