//! Apply existing confined-build authority before evaluating a build-bearing
//! shell invocation. Bridle keeps the original source, argv, and pipe semantics.

use super::super::content_spill::SpillStore;
use super::super::display::ToolPresentation;
use super::super::smart_harness::SmartHarness;
use super::{shell, PermissionDecision, PermissionGate, PermissionRequest};
use crate::caveats::Caveats;
use crate::confined_exec::build_tool_request;
use crate::ExecOutcome;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Identify a build tool through Bridle's structural inventory. This selects
/// an existing capability; it never interprets Cargo flags or rewrites source.
pub(super) fn build_program(source: &str) -> Option<String> {
    fn find(inspection: &agent_bridle::ShellInspection) -> Option<String> {
        for command in &inspection.commands {
            if let Some(program) = command
                .program
                .as_deref()
                .filter(|program| crate::confined_exec::is_build_tool_exec(program))
            {
                return Some(program.to_owned());
            }
            for child in &command.descendant_execs {
                if crate::confined_exec::is_build_tool_exec(&child.program) {
                    return Some(child.program.clone());
                }
            }
        }
        inspection
            .constructs
            .iter()
            .filter_map(|construct| construct.inspection.as_ref())
            .find_map(|nested| find(nested))
    }
    find(&agent_bridle::inspect_shell(source).ok()?)
}

/// #2723: select the build fence from existing filesystem authority, not a
/// second workspace allowlist. Canonical containment keeps a symlink inside a
/// granted directory from authorizing an ungranted target. Build/frame approval
/// still happens below, using this same root for every part of the request.
pub(super) fn build_directory(
    workspace: &str,
    cwd: &Path,
    caveats: &Caveats,
) -> Result<(PathBuf, PathBuf), (String, ExecOutcome)> {
    let launch = Path::new(workspace).canonicalize().map_err(|error| {
        (
            format!("error: build workspace: {error}"),
            ExecOutcome::Unavailable,
        )
    })?;
    let refusal = || {
        (
            concat!(
                "capability denied: build directory must remain inside the workspace ",
                "or an authorized write root; grant the worktree with --write <worktree> ",
                "or launch there; no command ran"
            )
            .into(),
            ExecOutcome::Denied,
        )
    };
    let cwd = cwd.canonicalize().map_err(|_| refusal())?;
    if !cwd.is_dir() {
        return Err(refusal());
    }
    if cwd.starts_with(&launch) {
        return Ok((launch, cwd));
    }
    let root = match &caveats.fs_write {
        // The operator explicitly authorized all paths; keep this build's
        // calibrated fence narrow rather than granting the filesystem root.
        crate::Scope::All => Some(cwd.clone()),
        crate::Scope::Only(roots) => roots
            .iter()
            .filter(|root| Path::new(root).is_absolute())
            .filter_map(|root| Path::new(root).canonicalize().ok())
            .filter(|root| root.is_dir() && cwd.starts_with(root))
            .max_by_key(|root| root.components().count()),
    };
    root.map(|root| (root, cwd)).ok_or_else(refusal)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute(
    source: &str,
    program: &str,
    cwd: &str,
    workspace: &str,
    caveats: &Caveats,
    filesystem_requests: &[PermissionRequest],
    gate: &mut Option<&mut dyn PermissionGate>,
    harness: Option<&SmartHarness>,
    tool_output_lines: usize,
    color: bool,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    live_output: Option<Arc<dyn crate::agentic::LiveToolOutput>>,
    presentation: &mut dyn ToolPresentation,
    command_broker: Option<Arc<dyn agent_bridle_tool_shell::CommandBroker>>,
) -> (String, ExecOutcome) {
    if let Some(refusal) = shell::same_file_redirect_refusal(source, cwd) {
        return (refusal, ExecOutcome::Denied);
    }
    let (root, cwd) = match build_directory(workspace, Path::new(cwd), caveats) {
        Ok(directory) => directory,
        Err(refusal) => return refusal,
    };

    // The existing argv request supplies the authority, environment, and
    // scratch policy. Its argv is not executed: Brush evaluates source once.
    let request = build_tool_request(
        &root,
        &cwd,
        program,
        std::iter::empty::<String>(),
        &caveats.net,
    );
    let build = request.caveats();
    if let Some(harness) = harness {
        if let Err(error) = harness.validate_tool_authority(build, &root) {
            return (
                format!("Error: frame isolation: {error}"),
                ExecOutcome::Denied,
            );
        }
    }
    if !filesystem_requests
        .iter()
        .all(|declared| shell::permits_filesystem_request(build, declared))
    {
        return (
            "capability denied: declared filesystem paths exceed the confined build fence; no command ran".into(),
            ExecOutcome::Denied,
        );
    }
    if !build.leq(caveats) {
        let permission =
            super::lifecycle_build_request(&root.to_string_lossy(), source, build, None);
        let allowed = gate.as_deref_mut().is_some_and(|gate| {
            matches!(gate.ask_with_caveats(build, &[permission]), PermissionDecision::Allow(allowed) if build.leq(&allowed))
        });
        if !allowed {
            return (
                "capability denied: command requires explicit confined build authority; no command ran".into(),
                ExecOutcome::Denied,
            );
        }
    }

    // Admission precedes even scratch creation, redirections, and pipeline
    // stages. Transfer a lease to the actual worker owner so cancellation of
    // this future cannot remove scratch before that worker has been reaped.
    let scratch: Arc<dyn Send + Sync> = match request.prepare_scratch() {
        Ok(scratch) => Arc::new(scratch),
        Err(error) => {
            return (
                format!("error: build scratch: {error}"),
                ExecOutcome::Unavailable,
            )
        }
    };
    let environment: std::collections::BTreeMap<_, _> =
        request.env_grants().iter().cloned().collect();
    let args = serde_json::json!({"cmd": source, "cwd": cwd, "env": environment});
    // Never retry this source after execution: an earlier pipeline stage may
    // already have had an effect, even if a later command was denied.
    match shell::dispatch_bridled_build_shell(args, build, live_output, scratch, command_broker)
        .await
    {
        Ok(envelope) => shell::confined_result(source, &envelope, build, color, |envelope| {
            shell::shell_envelope_output(
                envelope,
                tool_output_lines,
                color,
                tool_offload,
                spill_store,
                Some(presentation),
            )
        }),
        Err(error) => (format!("error: {error}"), ExecOutcome::Unavailable),
    }
}

#[cfg(test)]
mod tests {
    use super::build_program;

    #[test]
    fn build_selection_uses_executables_not_text_or_pipeline_shapes() {
        for source in [
            "cargo build --lib 2>&1 | grep error",
            "'cargo' fmt --all && printf formatted",
            "printf before; cargo check",
            "make all | grep result",
            "cd newt-core && echo before; timeout 120 cargo check 2>&1 | tail -30",
        ] {
            assert!(build_program(source).is_some(), "{source}");
        }
        for source in [
            "git status && grep cargo Cargo.toml",
            "printf 'cargo build | grep error'",
            "python -c 'print(\"cargo build\")'",
        ] {
            assert!(build_program(source).is_none(), "{source}");
        }
    }
}
