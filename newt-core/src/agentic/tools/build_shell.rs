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
    find_build_program(source, |program, _| {
        crate::confined_exec::is_build_tool_exec(program)
    })
}

/// Select the specialized build fence independently of build-time budgeting.
pub(super) fn confined_build_program(source: &str, ocap_disabled: bool) -> Option<String> {
    // The ordinary shell path owns host execution and explicit exec floors.
    // Do not create a confined build request or scratch lease before it runs.
    (!ocap_disabled).then(|| build_program(source)).flatten()
}

/// Preserve ordinary shell source instead of translating builds to build_exec.
pub(super) fn route_for_ocap(
    decision: crate::agentic::routing::RouteDecision,
    ocap_disabled: bool,
) -> crate::agentic::routing::RouteDecision {
    use crate::agentic::routing::RouteDecision;
    match decision {
        RouteDecision::Route {
            tool: "build_exec", ..
        } if ocap_disabled => RouteDecision::Exec,
        decision => decision,
    }
}

/// Time budgets distinguish actual build work from version/help probes.
pub(super) fn has_build_work(source: &str) -> bool {
    find_build_program(source, crate::agentic::routing::is_build_work).is_some()
}

fn find_build_program(source: &str, predicate: fn(&str, &[String]) -> bool) -> Option<String> {
    fn find(
        inspection: &agent_bridle::ShellInspection,
        predicate: fn(&str, &[String]) -> bool,
    ) -> Option<String> {
        for command in &inspection.commands {
            if let Some(program) = command
                .program
                .as_deref()
                .filter(|program| predicate(program, &command.argv))
            {
                return Some(program.to_owned());
            }
            for child in &command.descendant_execs {
                if predicate(&child.program, &child.argv) {
                    return Some(child.program.clone());
                }
            }
        }
        inspection
            .constructs
            .iter()
            .filter_map(|construct| construct.inspection.as_ref())
            .find_map(|nested| find(nested, predicate))
    }
    find(&agent_bridle::inspect_shell(source).ok()?, predicate)
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
    let cwd = shell::existing_exec_cwd(cwd)?;
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
    timeout_secs: Option<u64>,
    command_budget: crate::RunCommandBudget,
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
    let args = serde_json::json!({"cmd": source, "cwd": cwd, "env": environment, "timeout_secs": timeout_secs});
    // Never retry this source after execution: an earlier pipeline stage may
    // already have had an effect, even if a later command was denied.
    match shell::dispatch_bridled_build_shell(
        args,
        build,
        live_output,
        scratch,
        command_broker,
        request.build_held_read_roots(),
        command_budget,
    )
    .await
    {
        Ok(envelope) => shell::confined_result(
            source,
            &envelope,
            build,
            color,
            |envelope| {
                shell::shell_envelope_output(
                    envelope,
                    tool_output_lines,
                    color,
                    tool_offload,
                    spill_store,
                    Some(presentation),
                )
            },
            command_budget,
        ),
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
    /// #2784: classified builds must reach the ordinary host route when OCAP
    /// is disabled, on Windows as well as Unix; confined builds retain their fence.
    #[test]
    fn disabled_ocap_builds_reach_the_portable_host_route() {
        use super::super::shell::{select_shell_route, ShellRoute};
        for source in [
            "cargo check",
            "cargo check --quiet 2>&1",
            "cargo check 2>&1 | head",
            "cd task && cargo check",
            "printf before; cargo check",
        ] {
            assert!(
                super::has_build_work(source),
                "budget classification: {source}"
            );
            assert!(
                super::confined_build_program(source, false).is_some(),
                "confined: {source}"
            );
            assert!(
                super::confined_build_program(source, true).is_none(),
                "host: {source}"
            );
            for windows in [false, true] {
                assert_eq!(
                    select_shell_route(
                        true,
                        false,
                        false,
                        true,
                        windows,
                        false,
                        true,
                        crate::ShellEngine::SafeSubset
                    ),
                    if windows {
                        ShellRoute::AmbientBrush
                    } else {
                        ShellRoute::BashSh
                    }
                );
                assert!(
                    matches!(
                        select_shell_route(
                            true,
                            false,
                            false,
                            false,
                            windows,
                            false,
                            true,
                            crate::ShellEngine::SafeSubset
                        ),
                        ShellRoute::Bridled(_)
                    ),
                    "an explicit exec floor still wins"
                );
            }
        }
    }
    /// #2784: the earlier argv build route must not bypass the host decision.
    #[test]
    fn disabled_ocap_builds_keep_original_source_at_l2() {
        use crate::agentic::routing::{RouteDecision, RouteTable};
        for source in [
            "cargo check",
            "cargo check 2>&1 | tail -5",
            "timeout 10 cargo check",
        ] {
            let decision = RouteTable::builtin().classify(
                source,
                std::path::Path::new("."),
                &crate::Scope::All,
            );
            assert!(
                matches!(
                    decision,
                    RouteDecision::Route {
                        tool: "build_exec",
                        ..
                    }
                ),
                "{source}: {decision:?}"
            );
            assert_eq!(super::route_for_ocap(decision.clone(), false), decision);
            assert_eq!(
                super::route_for_ocap(decision, true),
                RouteDecision::Exec,
                "{source}"
            );
        }
        let read = RouteDecision::Route {
            tool: "read_file",
            args: serde_json::json!({"path": "src/lib.rs"}),
        };
        assert_eq!(
            super::route_for_ocap(read.clone(), true),
            read,
            "read routing stays governed"
        );
    }
}
