//! Standalone ambient interpreter. No CLI or server composition is re-entered.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    let code = std::thread::Builder::new()
        .name("ambient-brush".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(run())
        })?
        .join()
        .map_err(|_| anyhow::anyhow!("ambient Brush worker panicked"))??;
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("newt-ambient-brush is supported on Windows only");
    std::process::exit(1);
}

/// Execute one script delivered through stdin by the supervised parent.
/// No command is evaluated until EOF: job assignment must precede delivery.
#[cfg(windows)]
async fn run() -> anyhow::Result<i32> {
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
