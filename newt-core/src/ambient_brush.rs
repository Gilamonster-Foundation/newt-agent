//! Windows ambient Brush child. This is an ordinary interpreter entrypoint,
//! never the authenticated confined worker. The parent strips Newt authority
//! variables and assigns a kill-on-close job before delivering the script.

/// Execute one script delivered through stdin by the supervised parent.
/// No command is evaluated until EOF: job assignment must precede delivery.
#[cfg(windows)]
pub async fn run() -> anyhow::Result<i32> {
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    // A cancelled partial pipe write must never become a partial shell script.
    // Reuse JSON's string encoding: a truncated value is refused before eval.
    let script: String = serde_json::from_str(&input)?;
    let mut shell = brush_core::Shell::builder()
        .builtins(brush_builtins::default_builtins(
            brush_builtins::BuiltinSet::BashMode,
        ))
        .interactive(false)
        .no_editing(true)
        .kill_external_commands_on_drop(true)
        .build()
        .await?;
    // Inherit only the already-sanitized child environment, including exported
    // PATH and Windows SystemRoot/TEMP. Never register private carried shims:
    // ordinary tools are adjacent executables supplied by the command pack.
    let params = shell.default_exec_params();
    let result = shell
        .run_string(&script, &brush_core::SourceInfo::default(), &params)
        .await?;
    Ok(u8::from(result.exit_code) as i32)
}

/// Native integration seam: inject the built CLI path without an environment
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
