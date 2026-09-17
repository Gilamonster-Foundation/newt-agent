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
                " · login required or credential rejected"
            } else {
                " · connection failed"
            }
        );
    }
    if let Some(required) = error.downcast_ref::<newt_mcp_client::HttpNetGrantRequired>() {
        return format!("OCAP network approval needed for {}", required.host);
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
            panel.message = "Server configuration changed; reopen MCP settings.".into();
            continue;
        };
        let entry = &server.entry;
        context.cancel.store(false, Ordering::Relaxed);
        if action.kind == ActionKind::Login && entry.transport == TransportKind::Stdio {
            let Some(argv) = entry.operator_login_argv() else {
                panel.message = "No trusted login_argv configured. Log in using the server's CLI, then Reconnect.".into();
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
                    panel.message = "Login cancelled".into();
                    continue;
                }
            }
            match surface.run_login_argv(argv, gate.color, gate.verbose) {
                Ok(true) => {}
                Ok(false) => {
                    panel.message = "Login failed or was interrupted; connection unchanged.".into();
                    continue;
                }
                Err(_) => {
                    panel.message =
                        "Could not run the configured login; connection unchanged.".into();
                    continue;
                }
            }
        }
        let caveats = gate.current_caveats();
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
        let mut grant_net =
            |request: &newt_core::PermissionRequest| gate.ask_mcp_net_grant(request);
        let result =
            crate::with_interrupt_watch(!context.terminal_owns_turn, &context.cancel, || {
                tokio::task::block_in_place(|| {
                    context.runtime.block_on(async {
                        if action.kind == ActionKind::Login
                            && entry.transport == TransportKind::Http
                        {
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
                                if action.kind == ActionKind::Test {
                                    mcp.test_connection(&entry.name).await
                                } else {
                                    mcp.reconnect(
                                        entry,
                                        &caveats,
                                        &hosts,
                                        insecure,
                                        Some(&mut grant_net),
                                    )
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
        panel.message = match result {
            Ok(count) => format!("Success · {count} tools available"),
            Err(error) => failure_message(&error),
        };
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
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.to_string(), "MCP operation cancelled");
        assert!(dropped.load(Ordering::Relaxed));
    }
}
