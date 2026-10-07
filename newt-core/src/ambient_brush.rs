//! Location contract for the ordinary ambient runner, independent of the host
//! binary's entrypoints. Never search cwd, PATH, or workspace configuration.

/// The sole installation location for the carried ambient interpreter.
pub fn runner_path(host_executable: &std::path::Path) -> std::path::PathBuf {
    dunce::simplified(host_executable)
        .with_file_name("tools")
        .join(format!(
            "newt-ambient-brush{}",
            std::env::consts::EXE_SUFFIX
        ))
}

/// Whether this embedding host has the separately installed runner.
pub fn runner_available(host_executable: &std::path::Path) -> bool {
    runner_path(host_executable).is_file()
}

pub(crate) fn installed() -> bool {
    std::env::current_exe().is_ok_and(|path| runner_available(&path))
}

/// Native integration seam: inject a non-CLI host executable path without an environment
/// override in production. Exercises the same selector, child and supervisor.
#[cfg(all(windows, feature = "test-util"))]
pub async fn test_dispatch(
    executable: &std::path::Path,
    script: &str,
    cwd: &str,
    windows_cmd: bool,
    timeout: std::time::Duration,
) -> std::io::Result<serde_json::Value> {
    crate::agentic::tools::test_windows_ambient_dispatch(
        executable,
        script,
        cwd,
        windows_cmd,
        timeout,
    )
    .await
}
