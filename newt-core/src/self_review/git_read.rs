//! Bounded stdout for already-authorized hardened Git metadata. No shell,
//! global authority resolver, new process scheduler or persistent storage.
use std::process::Stdio;
use tokio::io::AsyncReadExt;

use super::CaptureFailure;
use crate::caveats::Scope;

pub(super) async fn output(
    directory: &crate::fs_cap::WorkspaceDir,
    scope: &Scope<String>,
    args: &[&str],
    max_bytes: usize,
) -> Result<Vec<u8>, CaptureFailure> {
    let result = command_output(directory, scope, args, max_bytes).await?;
    if !result.status.success() {
        return Err(failure(&result));
    }
    Ok(result.stdout)
}

pub(super) fn failure(result: &std::process::Output) -> CaptureFailure {
    CaptureFailure::Incomplete(format!(
        "Git subject read failed: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    ))
}

/// Retain exit status only for the narrow discovery/ref-existence decisions.
/// Spawn, pipe, bound and wait failures remain errors, never absence.
pub(super) async fn command_output(
    directory: &crate::fs_cap::WorkspaceDir,
    scope: &Scope<String>,
    args: &[&str],
    max_bytes: usize,
) -> Result<std::process::Output, CaptureFailure> {
    // Preserve authority before resolving or launching any executable.
    crate::agentic::check_git_read_scope("metadata", scope).map_err(|_| CaptureFailure::Denied)?;
    let cwd = directory
        .command_directory()
        .map_err(|error| CaptureFailure::Incomplete(error.to_string()))?;
    let mut command = crate::git_hardening::metadata_git(&cwd, args, scope).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            CaptureFailure::Denied
        } else {
            CaptureFailure::Incomplete(error.to_string())
        }
    })?;
    // Git 2.43 may lazily hydrate promised objects even for cat-file -s.
    // Disallow every fetch transport AFTER hardening's environment reset.
    command.env("GIT_ALLOW_PROTOCOL", "");
    let mut command = tokio::process::Command::from(command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| CaptureFailure::Incomplete(error.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CaptureFailure::Incomplete("Git stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CaptureFailure::Incomplete("Git stderr unavailable".into()))?;
    let completed = tokio::try_join!(
        async {
            child
                .wait()
                .await
                .map_err(|error| CaptureFailure::Incomplete(error.to_string()))
        },
        bounded(stdout, max_bytes),
        bounded(stderr, 4096),
    );
    let (status, stdout, stderr) = match completed {
        Ok(result) => result,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error);
        }
    };
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

async fn bounded(
    pipe: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, CaptureFailure> {
    let mut bytes = Vec::new();
    let bound = u64::try_from(limit)
        .map_err(|_| CaptureFailure::OverLimit)?
        .saturating_add(1);
    pipe.take(bound)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| CaptureFailure::Incomplete(error.to_string()))?;
    if bytes.len() > limit {
        return Err(CaptureFailure::OverLimit);
    }
    Ok(bytes)
}
