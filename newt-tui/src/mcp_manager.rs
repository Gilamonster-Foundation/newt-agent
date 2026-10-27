//! The MCP panel's action boundary: terminal leases end before login or networking.

use crate::chat::InputSurface;
use crate::mcp::{Mcp, McpStatus};
use crate::mcp_panel::{ActionKind, McpPanel, ServerView};
use crate::permissions::{PromptChoice, PromptPermissionGate};
use newt_core::interaction_surface::SurfaceInteraction;
use newt_core::mcp::{McpServerEntry, TransportKind};
use newt_core::tty::PromptWindow;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub(crate) struct Context<'a> {
    pub cfg: &'a newt_core::Config,
    pub workspace: &'a str,
    pub runtime: &'a tokio::runtime::Handle,
    pub cancel: Arc<AtomicBool>,
    pub terminal_owns_turn: bool,
}

fn discovery(context: &Context<'_>) -> newt_core::mcp::McpDiscoveryReport {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let file = newt_core::Config::user_config_dir().map(|dir| dir.join("mcp.toml"));
    newt_core::mcp::discover_report_with_namespace_mode(
        &context.cfg.mcp_servers,
        file.as_deref(),
        home.as_deref(),
        std::path::Path::new(context.workspace),
        context
            .cfg
            .tui
            .as_ref()
            .is_none_or(|tui| tui.sanitize_mcp_server_names),
    )
}

fn authentication(entry: &McpServerEntry) -> String {
    if newt_core::mcp::admit(entry).is_err() {
        return "not inspected; configuration not admitted".into();
    }
    if entry.transport == TransportKind::Stdio {
        return "server-managed; connection does not verify service login".into();
    }
    if crate::mcp::has_plaintext_authorization_header(entry) {
        return "unsafe plaintext Authorization; use a credential reference".into();
    }
    if crate::mcp::has_configured_authorization_header(entry) {
        return "configured credential reference (not verified)".into();
    }
    let Some(url) = entry.url.as_ref() else {
        return "no endpoint".into();
    };
    let status = crate::mcp_token::auth_status(&[(entry.name.clone(), url.clone())]);
    use crate::mcp_token::AuthState;
    match status.first().map(|status| &status.state) {
        Some(AuthState::Valid) => "saved login valid; server acceptance checked on connection",
        Some(AuthState::Expired) => "saved login expired",
        Some(AuthState::NeedsMigration) => "fresh login required",
        _ => "no saved OAuth login (server may allow anonymous access)",
    }
    .into()
}

fn views(mcp: &Mcp, report: &newt_core::mcp::McpDiscoveryReport) -> Vec<ServerView> {
    let mut views = report
        .servers
        .iter()
        .map(|server| ServerView {
            entry: server.entry.clone(),
            source: server.source,
            status: mcp
                .statuses
                .iter()
                .find(|(name, _)| name == &server.entry.name)
                .map(|(_, status)| status.clone()),
            auth: authentication(&server.entry),
            tools: mcp.server_tools(&server.entry.name),
            muted: mcp.is_muted(&server.entry.name),
            conflict: None,
        })
        .collect::<Vec<_>>();
    for conflict in &report.conflicts {
        // The hidden row never acquires a connection or auth state. Its winner
        // supplies no executable authority: the panel refuses every action.
        if let Some(winner) = report
            .servers
            .iter()
            .find(|server| server.entry.name == conflict.winner)
        {
            let mut entry = winner.entry.clone();
            entry.name = conflict.name.clone();
            entry.command = None;
            entry.url = None;
            entry.login_argv.clear();
            entry.enabled = false;
            views.push(ServerView {
                entry,
                source: conflict.source,
                status: Some(McpStatus::Disabled),
                auth: "not inspected; shadowed configuration".into(),
                tools: vec![],
                muted: false,
                conflict: Some(conflict.winner.clone()),
            });
        }
    }
    views
}

fn failure_message(error: &anyhow::Error) -> String {
    // No arbitrary transport body, credential-bearing URL, or child stderr is
    // copied into the panel. Typed protocol/network errors remain actionable.
    if let Some(status) = newt_mcp_client::http_error_status(error) {
        return format!(
            "HTTP {status}{}",
            if status == 401 {
                format!(
                    " · login required or credential rejected · {}",
                    crate::mcp::LOGIN_REMEDIATION
                )
            } else {
                " · connection failed".into()
            }
        );
    }
    if let Some(required) = error.downcast_ref::<newt_mcp_client::HttpNetGrantRequired>() {
        return format!("OCAP network approval needed for {}", required.host);
    }
    // A dead refresh grant: the token endpoint answered `invalid_grant`.
    if error
        .downcast_ref::<crate::mcp_token::OAuthGrantRejected>()
        .is_some()
    {
        return format!(
            "Saved login was rejected (invalid_grant) · {}",
            crate::mcp::LOGIN_REMEDIATION
        );
    }
    let reason = error.to_string();
    if reason.contains("cancelled") {
        return "Cancelled".into();
    }
    if reason.contains("closed the connection during initialize") {
        return "Server exited during initialization; check its runtime and filesystem grants."
            .into();
    }
    if reason.contains("disabled") {
        return "Server is disabled in configuration.".into();
    }
    if reason.contains("not admitted") || reason.contains("untrusted") {
        return "Configuration is not admitted; review and import it before connecting.".into();
    }
    if reason.contains("Authorization reference") {
        return "Login is managed by the configured Authorization reference.".into();
    }
    if reason.contains("disconnected") {
        return "Server is disconnected; choose Reconnect first.".into();
    }
    "Operation failed. Check server configuration, authentication, and OCAP grants.".into()
}

fn confirm_login(
    surface: &mut dyn InputSurface,
    entry: &McpServerEntry,
) -> newt_core::HumanQuestionOutcome {
    let argv = entry.operator_login_argv().unwrap_or_default();
    let definition = newt_core::interaction_form::confirm(
        format!("Run the configured login for `{}`?\nProgram and literal arguments: {argv:?}", entry.name),
        "This is an operator host command. The MCP connection will be recreated only after successful login.",
        "Run login", "Cancel");
    surface.present_interaction(&SurfaceInteraction::blocking(definition))
}

/// The single exit for an operation outcome (#3013): the panel footer shows it
/// until the next page change, and `emit` commits it to scrollback so a
/// finished operation is distinguishable from a hung one once the panel closes.
/// Every outcome goes through here; nothing sets `panel.message` directly.
fn outcome(panel: &mut McpPanel, message: String, emit: &mut dyn FnMut(&str)) {
    emit(&message);
    panel.message = message;
}

pub(crate) fn run<F>(
    mcp: &mut Mcp,
    surface: &mut dyn InputSurface,
    gate: &mut PromptPermissionGate<'_, F>,
    context: Context<'_>,
) -> anyhow::Result<()>
where
    F: FnMut(&PromptWindow, &SurfaceInteraction) -> PromptChoice,
{
    let mut panel = McpPanel::new(
        views(mcp, &discovery(&context)),
        context
            .cfg
            .tui
            .as_ref()
            .is_none_or(|tui| tui.sanitize_mcp_server_names),
    );
    let (color, verbose) = (gate.color, gate.verbose);
    let mut commit = |text: &str| crate::print_newt(text, color, verbose);
    loop {
        let window = surface.open_panel(crate::session_worker::PanelMode::Inline(24));
        crate::panel::drive(&mut panel, 24, window.as_ref())?;
        drop(window);
        let Some(action) = panel.take_action() else {
            return Ok(());
        };
        let report = discovery(&context);
        let Some(server) = report
            .servers
            .iter()
            .find(|server| server.entry.name == action.server)
        else {
            outcome(
                &mut panel,
                "Server configuration changed; reopen MCP settings.".into(),
                &mut commit,
            );
            continue;
        };
        let entry = &server.entry;
        context.cancel.store(false, Ordering::Relaxed);
        if action.kind == ActionKind::Login && entry.transport == TransportKind::Stdio {
            let Some(argv) = entry.operator_login_argv() else {
                outcome(
                    &mut panel,
                    "No trusted login_argv configured. Log in using the server's CLI, then Reconnect.".into(),
                    &mut commit,
                );
                continue;
            };
            match confirm_login(surface, entry) {
                newt_core::HumanQuestionOutcome::Answer(answer)
                    if answer == "yes" || answer.eq_ignore_ascii_case("y") => {}
                newt_core::HumanQuestionOutcome::ExitRequested => {
                    if let Some(exit) = gate.exit {
                        exit.store(true, Ordering::Relaxed);
                    }
                    return Ok(());
                }
                _ => {
                    outcome(&mut panel, "Login cancelled".into(), &mut commit);
                    continue;
                }
            }
            match surface.run_login_argv(argv, gate.color, gate.verbose) {
                Ok(true) => {}
                Ok(false) => {
                    outcome(
                        &mut panel,
                        "Login failed or was interrupted; connection unchanged.".into(),
                        &mut commit,
                    );
                    continue;
                }
                Err(_) => {
                    outcome(
                        &mut panel,
                        "Could not run the configured login; connection unchanged.".into(),
                        &mut commit,
                    );
                    continue;
                }
            }
        }
        let result = perform(mcp, surface, gate, &context, entry, action.kind)?;
        let message = match result {
            Ok(count) => format!("Success · {count} tools available"),
            Err(error) => failure_message(&error),
        };
        outcome(&mut panel, message, &mut commit);
        panel.refresh(views(mcp, &discovery(&context)));
        if gate.exit.is_some_and(|exit| exit.load(Ordering::Relaxed)) {
            return Ok(());
        }
    }
}

/// Race cancellable protocol I/O against the terminal owner's existing flag.
/// OAuth's listener is joined by its own flow before this wraps its reconnect.
pub(crate) async fn until_cancelled<T>(
    cancel: &AtomicBool,
    operation: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let result = tokio::select! {
        biased;
        () = async {
            while !cancel.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        } => anyhow::bail!("MCP operation cancelled"),
        result = operation => result,
    };
    anyhow::ensure!(!cancel.load(Ordering::Relaxed), "MCP operation cancelled");
    result
}

/// One Login / Reconnect / Test against `entry`, under the same caveats, net
/// grants and Ctrl-C watch whichever surface asked (the panel, or
/// `/mcp login-all`). The outer error is the gate's; the inner is the
/// operation's.
fn perform<F>(
    mcp: &mut Mcp,
    surface: &mut dyn InputSurface,
    gate: &mut PromptPermissionGate<'_, F>,
    context: &Context<'_>,
    entry: &McpServerEntry,
    kind: ActionKind,
) -> anyhow::Result<anyhow::Result<usize>>
where
    F: FnMut(&PromptWindow, &SurfaceInteraction) -> PromptChoice,
{
    let caveats = gate.current_caveats()?;
    let mut hosts = context
        .cfg
        .tui
        .as_ref()
        .map_or_else(Vec::new, |tui| tui.permissions.net.clone());
    hosts.extend(gate.retained_net_hosts());
    hosts.sort();
    hosts.dedup();
    let insecure = context
        .cfg
        .tui
        .as_ref()
        .map_or(&[][..], |tui| tui.mcp_allow_insecure_hosts.as_slice());
    surface.turn_started(context.cancel.clone());
    crate::print_newt(
        "MCP operation running · Ctrl-C cancels",
        gate.color,
        gate.verbose,
    );
    let mut grant_net = |request: &newt_core::PermissionRequest| gate.ask_mcp_net_grant(request);
    let result = crate::with_interrupt_watch(!context.terminal_owns_turn, &context.cancel, || {
        tokio::task::block_in_place(|| {
            context.runtime.block_on(async {
                if kind == ActionKind::Login && entry.transport == TransportKind::Http {
                    mcp.login_http(
                        entry,
                        (caveats, hosts),
                        insecure,
                        Some(&mut grant_net),
                        context.cancel.clone(),
                    )
                    .await
                } else {
                    let result = until_cancelled(&context.cancel, async {
                        if kind == ActionKind::Test {
                            mcp.test_connection(&entry.name).await
                        } else {
                            mcp.reconnect(entry, &caveats, &hosts, insecure, Some(&mut grant_net))
                                .await
                        }
                    })
                    .await;
                    if context.cancel.load(Ordering::Relaxed) {
                        mcp.cancel_connection(&entry.name);
                        return Err(anyhow::anyhow!("MCP operation cancelled"));
                    }
                    result
                }
            })
        })
    });
    surface.turn_ended();
    Ok(result)
}

/// `/mcp login-all` (#3028): sign in to every admitted HTTP server that needs
/// it, one after another, through the panel's own Login path. Ctrl-C stops the
/// run; logins that already finished stay saved.
pub(crate) fn login_all<F>(
    mcp: &mut Mcp,
    surface: &mut dyn InputSurface,
    gate: &mut PromptPermissionGate<'_, F>,
    context: Context<'_>,
) -> anyhow::Result<()>
where
    F: FnMut(&PromptWindow, &SurfaceInteraction) -> PromptChoice,
{
    let entries: Vec<McpServerEntry> = discovery(&context)
        .servers
        .into_iter()
        .map(|server| server.entry)
        .collect();
    let targets = crate::auth_command::login_all_targets(
        &entries,
        &|var| std::env::var(var).ok(),
        &crate::mcp::saved_oauth_valid,
    );
    let (color, verbose) = (gate.color, gate.verbose);
    context.cancel.store(false, Ordering::Relaxed);
    let cancel = context.cancel.clone();
    // The summary is already printed; a failed run is not a panel error.
    let _ = crate::auth_command::login_all(
        &targets,
        |entry| match perform(mcp, surface, gate, &context, entry, ActionKind::Login) {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(failure_message(&error)),
            Err(error) => Err(error.to_string()),
        },
        &|| cancel.load(Ordering::Relaxed),
        &mut |line| crate::print_newt(line, color, verbose),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn management_ready_operation_cannot_report_success_after_cancel() {
        let cancel = AtomicBool::new(false);
        let result = until_cancelled(&cancel, async {
            cancel.store(true, Ordering::Relaxed);
            Ok(7usize)
        })
        .await;
        assert_eq!(result.unwrap_err().to_string(), "MCP operation cancelled");
    }

    #[tokio::test]
    async fn management_cancel_drops_the_inflight_connection_operation() {
        struct Pending(Arc<AtomicBool>);
        impl Drop for Pending {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let pending = Pending(dropped.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        let worker = tokio::spawn(async move {
            until_cancelled(&signal, async move {
                let _pending = pending;
                std::future::pending::<anyhow::Result<usize>>().await
            })
            .await
        });
        tokio::task::yield_now().await;
        cancel.store(true, Ordering::Relaxed);
        // The worker noticing the flag (through its 25 ms poll) is the event;
        // the shared guard only names a hang. A 1 s budget expired under load
        // before the task was even scheduled.
        let error = tokio::time::timeout(newt_core::test_guard::HANG_GUARD, worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.to_string(), "MCP operation cancelled");
        assert!(dropped.load(Ordering::Relaxed));
    }

    #[test]
    fn an_outcome_is_committed_to_scrollback_as_well_as_the_panel() {
        let mut panel = McpPanel::new(Vec::new(), true);
        let mut committed = Vec::new();
        outcome(
            &mut panel,
            "Success · 3 tools available".into(),
            &mut |text| committed.push(text.to_string()),
        );
        assert_eq!(panel.message, "Success · 3 tools available");
        assert_eq!(committed, ["Success · 3 tools available"]);
    }

    /// #3013: an outcome that only sets `panel.message` is invisible once the
    /// panel closes. The helper above is the one place allowed to do it.
    #[test]
    fn no_outcome_bypasses_the_commit_helper() {
        let src = include_str!("mcp_manager.rs");
        let production = src.split("#[cfg(test)]").next().unwrap();
        let direct = production.matches("panel.message =").count();
        assert_eq!(direct, 1, "only `outcome` may assign panel.message");
    }

    /// #3028: a 401 on connect/Reconnect, or an OAuth `invalid_grant` (a dead
    /// refresh token), tells the operator how to sign in again.
    #[test]
    fn a_rejected_credential_points_at_login_and_login_all_3028() {
        let unauthorized = anyhow::Error::new(newt_mcp_client::HttpStatusError::new(
            401,
            "Unauthorized",
            "",
        ));
        assert_eq!(
            failure_message(&unauthorized),
            "HTTP 401 · login required or credential rejected · choose Login, or run /mcp login-all"
        );
        let invalid_grant = anyhow::Error::new(crate::mcp_token::OAuthGrantRejected)
            .context("token refresh failed");
        assert_eq!(
            failure_message(&invalid_grant),
            "Saved login was rejected (invalid_grant) · choose Login, or run /mcp login-all"
        );
    }
}
