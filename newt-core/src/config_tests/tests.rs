use super::*;

/// Test seam for loader semantics: run the backend assembly over
/// `cfg.backends` plus `dirs` (in order), exactly the way
/// `resolve_runtime_unpublished` does — effective backends written
/// back, receipts AND warnings returned for inspection.
///
/// #1984: the ONE real implementation. [`merge_for_test`] and
/// [`merge_for_test_with_warnings`] are thin wrappers over this — the
/// former preserves the pre-#1984 signature so its ~20 unrelated callers
/// need no changes; the latter is for the handful of tests that assert on
/// warning TEXT, which they now read as a returned value instead of
/// scraping a global tracing subscriber (see the doc on
/// `BackendAssembly::warnings` in `config/backend.rs` for why that scrape was
/// flaky).
fn merge_for_test_inner(
    cfg: &mut Config,
    dirs: &[&Path],
) -> std::result::Result<(Vec<BackendResolutionReceipt>, Vec<String>), String> {
    let mut assembly = BackendAssembly::new(std::mem::take(&mut cfg.backends))?;
    for dir in dirs {
        assembly.merge_dir(dir)?;
    }
    if assembly.operator_configured() {
        cfg.backend_fallback = false;
    }
    let warnings = assembly.warnings().to_vec();
    let (backends, receipts) = assembly.finish();
    cfg.backends = backends;
    Ok((receipts, warnings))
}

fn merge_for_test(
    cfg: &mut Config,
    dirs: &[&Path],
) -> std::result::Result<Vec<BackendResolutionReceipt>, String> {
    merge_for_test_inner(cfg, dirs).map(|(receipts, _warnings)| receipts)
}

fn merge_for_test_with_warnings(
    cfg: &mut Config,
    dirs: &[&Path],
) -> std::result::Result<(Vec<BackendResolutionReceipt>, Vec<String>), String> {
    merge_for_test_inner(cfg, dirs)
}

/// Test seam for the CLI-request phase: assembly over `cfg.backends` +
/// `dirs` + an explicit request — the whole pipeline minus file
/// layering, receipts AND warnings returned. Same #1984 wrapper shape as
/// [`merge_for_test_inner`] above, for the same reason.
fn resolve_for_test_inner(
    cfg: &mut Config,
    dirs: &[&Path],
    over: Option<BackendOverride>,
) -> std::result::Result<(Vec<BackendResolutionReceipt>, Vec<String>), String> {
    let mut assembly = BackendAssembly::new(std::mem::take(&mut cfg.backends))?;
    for dir in dirs {
        assembly.merge_dir(dir)?;
    }
    let _slot = assembly.apply_request(over, cfg.default_backend.as_deref())?;
    let warnings = assembly.warnings().to_vec();
    let (backends, receipts) = assembly.finish();
    cfg.backends = backends;
    Ok((receipts, warnings))
}

fn resolve_for_test(
    cfg: &mut Config,
    dirs: &[&Path],
    over: Option<BackendOverride>,
) -> std::result::Result<Vec<BackendResolutionReceipt>, String> {
    resolve_for_test_inner(cfg, dirs, over).map(|(receipts, _warnings)| receipts)
}

fn resolve_for_test_with_warnings(
    cfg: &mut Config,
    dirs: &[&Path],
    over: Option<BackendOverride>,
) -> std::result::Result<(Vec<BackendResolutionReceipt>, Vec<String>), String> {
    resolve_for_test_inner(cfg, dirs, over)
}

/// Pin the FULL config-resolution environment (`NEWT_CONFIG` removed,
/// `NEWT_CONFIG_DIR` + `HOME` + cwd → `dir`) for a resolve-level test,
/// restoring everything on drop — panic-safe, unlike the manual
/// save/restore pattern. Users stay in the `real_fs` serial lane.
struct HomeSandbox {
    config: Option<std::ffi::OsString>,
    config_dir: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    cwd: PathBuf,
}
impl HomeSandbox {
    fn enter(dir: &Path) -> Self {
        let sandbox = Self {
            config: std::env::var_os("NEWT_CONFIG"),
            config_dir: std::env::var_os(NEWT_CONFIG_DIR_ENV),
            home: std::env::var_os("HOME"),
            cwd: std::env::current_dir().unwrap(),
        };
        // SAFETY: the `real_fs` serial lane serializes every test that
        // touches these; restoration runs on drop.
        unsafe {
            std::env::remove_var("NEWT_CONFIG");
            std::env::set_var(NEWT_CONFIG_DIR_ENV, dir);
            std::env::set_var("HOME", dir);
        }
        std::env::set_current_dir(dir).unwrap();
        sandbox
    }
}
impl Drop for HomeSandbox {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.cwd);
        // SAFETY: as above — serialized by the `real_fs` lane.
        unsafe {
            match self.config.take() {
                Some(v) => std::env::set_var("NEWT_CONFIG", v),
                None => std::env::remove_var("NEWT_CONFIG"),
            }
            match self.config_dir.take() {
                Some(v) => std::env::set_var(NEWT_CONFIG_DIR_ENV, v),
                None => std::env::remove_var(NEWT_CONFIG_DIR_ENV),
            }
            match self.home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

/// Pin `NEWT_CONFIG_DIR` for a test's duration and restore the prior
/// value on drop — INCLUDING through a panic or failed assertion, which
/// a manual end-of-test restore does not survive (a mid-test panic then
/// leaks the tempdir path into every later `user_config_dir()` reader).
/// Same RAII shape as the established `EnvGuard`s elsewhere in the
/// crate; env is process-global, so users stay in the
/// `#[serial_test::serial(real_fs)]` lane.
struct ConfigDirGuard {
    prev: Option<std::ffi::OsString>,
}
impl ConfigDirGuard {
    fn set(dir: &Path) -> Self {
        let prev = std::env::var_os(NEWT_CONFIG_DIR_ENV);
        // SAFETY: the `real_fs` serial lane serializes every test that
        // touches this env var; restoration runs on drop.
        unsafe { std::env::set_var(NEWT_CONFIG_DIR_ENV, dir) };
        Self { prev }
    }
}
impl Drop for ConfigDirGuard {
    fn drop(&mut self) {
        // SAFETY: as above — serialized by the `real_fs` lane.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var(NEWT_CONFIG_DIR_ENV, v),
                None => std::env::remove_var(NEWT_CONFIG_DIR_ENV),
            }
        }
    }
}

// Behavior families follow the current config owners. Backend tests are
// split further into schema, assembly, requests, selection, and persistence.
// Shared loader/environment fixtures above remain private to this test tree.

#[cfg(test)]
#[path = "backend_schema.rs"]
mod backend_schema_tests;

#[cfg(test)]
#[path = "backend_layers.rs"]
mod backend_layers_tests;

#[cfg(test)]
#[path = "backend_requests.rs"]
mod backend_requests_tests;

#[cfg(test)]
#[path = "backend_selection.rs"]
mod backend_selection_tests;

#[cfg(test)]
#[path = "dropin.rs"]
mod dropin_tests;

#[cfg(test)]
#[path = "backend_writers.rs"]
mod backend_writers_tests;

#[cfg(test)]
#[path = "context.rs"]
mod context_tests;

#[cfg(test)]
#[path = "crew.rs"]
mod crew_tests;

#[cfg(test)]
#[path = "discovery.rs"]
mod discovery_tests;

#[cfg(test)]
#[path = "layering.rs"]
mod layering_tests;

#[cfg(test)]
#[path = "loading.rs"]
mod loading_tests;

#[cfg(test)]
#[path = "loadout.rs"]
mod loadout_tests;

#[cfg(test)]
#[path = "mcp.rs"]
mod mcp_tests;

#[cfg(test)]
#[path = "memory.rs"]
mod memory_tests;

#[cfg(test)]
#[path = "permissions.rs"]
mod permissions_tests;

#[cfg(test)]
#[path = "presentation.rs"]
mod presentation_tests;

#[cfg(test)]
#[path = "profile.rs"]
mod profile_tests;

#[cfg(test)]
#[path = "redaction.rs"]
mod redaction_tests;

#[cfg(test)]
#[path = "runtime.rs"]
mod runtime_tests;

#[cfg(test)]
#[path = "semantic.rs"]
mod semantic_tests;

#[cfg(test)]
#[path = "skills.rs"]
mod skills_tests;

#[cfg(test)]
#[path = "summarizer.rs"]
mod summarizer_tests;

#[cfg(test)]
#[path = "tool_exposure.rs"]
mod tool_exposure_tests;

#[cfg(test)]
#[path = "tools.rs"]
mod tools_tests;
