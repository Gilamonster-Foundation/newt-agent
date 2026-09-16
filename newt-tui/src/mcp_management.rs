//! Selected-server lifecycle operations use the startup admission/connection path.

use super::*;

pub(super) async fn connect_entry(
    entry: &McpServerEntry,
    caveats: &newt_core::Caveats,
    http_policy: &mut (newt_core::Caveats, Vec<String>),
    allow_insecure_hosts: &[String],
    grant_net: &mut Option<&mut McpNetGrantPrompt<'_>>,
) -> anyhow::Result<ReconnectableServer> {
    anyhow::ensure!(entry.enabled, "server is disabled in configuration");
    let admitted = newt_core::mcp::admit(entry).map_err(|denied| anyhow::anyhow!("{denied}"))?;
    match entry.transport {
        TransportKind::Stdio => connect_stdio(&admitted, caveats)
            .await
            .map(|live| ReconnectableServer { live, http: None }),
        TransportKind::Http => {
            anyhow::ensure!(!has_plaintext_authorization_header(entry),
                "plaintext Authorization credential in MCP config; replace it with an environment/file reference");
            connect_http_with_net_prompt(&admitted, http_policy, allow_insecure_hosts, grant_net)
                .await
        }
        TransportKind::Sse => anyhow::bail!("legacy SSE transport (use type = \"http\")"),
    }
}

#[cfg(any(feature = "rich-tui", test))]
impl Mcp {
    /// Replace only this server. A failed reconnect cannot leave stale tools advertised.
    pub(crate) async fn reconnect(
        &mut self,
        entry: &McpServerEntry,
        caveats: &newt_core::Caveats,
        explicit_hosts: &[String],
        allow_insecure_hosts: &[String],
        mut grant_net: Option<&mut McpNetGrantPrompt<'_>>,
    ) -> anyhow::Result<usize> {
        let prefix = server_prefix(&entry.name, self.sanitize_server_names);
        anyhow::ensure!(
            newt_core::mcp::runtime_server_prefix_is_unambiguous(
                &entry.name,
                self.sanitize_server_names
            ),
            "ambiguous server namespace"
        );
        anyhow::ensure!(
            !self
                .servers
                .iter()
                .any(|server| server.live.name != entry.name
                    && server_prefix(&server.live.name, self.sanitize_server_names) == prefix),
            "server namespace conflict"
        );
        self.servers.retain(|server| server.live.name != entry.name);
        let mut policy = (caveats.clone(), explicit_hosts.to_vec());
        let result = connect_entry(
            entry,
            caveats,
            &mut policy,
            allow_insecure_hosts,
            &mut grant_net,
        )
        .await;
        synchronize_http_reconnect_policy(&mut self.servers, &policy);
        match result {
            Ok(connected) => {
                let count = connected.live.tools.len();
                self.set_status(
                    &entry.name,
                    McpStatus::Connected {
                        tools: count,
                        confinement: Confinement::from_sandbox(connected.live.sandbox_kind),
                        net: NetGate::from_posture(connected.live.net_posture),
                    },
                );
                self.servers.push(connected);
                Ok(count)
            }
            Err(error) => {
                self.set_status(
                    &entry.name,
                    if entry.enabled {
                        McpStatus::Skipped(format!("{error:#}"))
                    } else {
                        McpStatus::Disabled
                    },
                );
                Err(error)
            }
        }
    }

    /// Metadata-only health check. Never invokes any server tool.
    pub(crate) async fn test_connection(&mut self, name: &str) -> anyhow::Result<usize> {
        let server = self
            .servers
            .iter_mut()
            .find(|server| server.live.name == name)
            .ok_or_else(|| anyhow::anyhow!("server is disconnected; reconnect first"))?;
        match server.live.conn.list_tools().await {
            Ok(tools) => {
                let count = tools.len();
                server.live.tools = tools;
                let status = McpStatus::Connected {
                    tools: count,
                    confinement: Confinement::from_sandbox(server.live.sandbox_kind),
                    net: NetGate::from_posture(server.live.net_posture),
                };
                self.set_status(name, status);
                Ok(count)
            }
            Err(error) => {
                self.servers.retain(|server| server.live.name != name);
                self.set_status(name, McpStatus::Skipped(format!("{error:#}")));
                Err(error)
            }
        }
    }

    /// OAuth uses the same admission and exact-host prompt as connection. A login
    /// can cross several distinct discovery/token hosts, each approved separately.
    #[cfg(feature = "rich-tui")]
    pub(crate) async fn login_http(
        &mut self,
        entry: &McpServerEntry,
        mut policy: (newt_core::Caveats, Vec<String>),
        allow_insecure_hosts: &[String],
        mut grant_net: Option<&mut McpNetGrantPrompt<'_>>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> anyhow::Result<usize> {
        anyhow::ensure!(entry.enabled, "server is disabled in configuration");
        newt_core::mcp::admit(entry).map_err(|denied| anyhow::anyhow!("{denied}"))?;
        anyhow::ensure!(
            entry.transport == TransportKind::Http,
            "OAuth requires streamable HTTP"
        );
        anyhow::ensure!(
            !has_configured_authorization_header(entry),
            "authentication is managed by the configured Authorization reference"
        );
        let url = entry
            .url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("missing server endpoint"))?;
        let mut reconnect_policy = policy.clone();
        let mut approved = std::collections::BTreeSet::new();
        loop {
            let hop_policy =
                crate::mcp_token::OAuthHopPolicy::with_explicit_hosts(&policy.0.net, &policy.1);
            match crate::mcp_token::run_oauth_flow_cancellable(
                &entry.name,
                url,
                &hop_policy,
                Some(cancel.clone()),
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    let Some(required) =
                        error.downcast_ref::<newt_mcp_client::HttpNetGrantRequired>()
                    else {
                        return Err(error);
                    };
                    let host = required.host.clone();
                    if cancel.load(std::sync::atomic::Ordering::Relaxed)
                        || approved.len() >= 8
                        || approved.contains(&host)
                    {
                        return Err(error);
                    }
                    let Some((granted, hosts, remembered)) =
                        request_net_grant(&error, &entry.name, &mut grant_net, "mcp login",
                            "Allow once covers this OAuth login only. The new server connection may ask separately; session/permanent can cover both.")
                    else {
                        return Err(error);
                    };
                    apply_login_net_grant(
                        &mut policy,
                        &mut reconnect_policy,
                        &mut approved,
                        host,
                        (granted, hosts, remembered),
                    );
                }
            }
        }
        anyhow::ensure!(
            !cancel.load(std::sync::atomic::Ordering::Relaxed),
            "MCP login cancelled"
        );
        // Reconnect still re-admits and validates the original resource. It may
        // need its own grant when discovery reached a different origin.
        let result = crate::mcp_manager::until_cancelled(
            &cancel,
            self.reconnect(
                entry,
                &reconnect_policy.0,
                &reconnect_policy.1,
                allow_insecure_hosts,
                grant_net,
            ),
        )
        .await;
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            self.cancel_connection(&entry.name);
            return Err(anyhow::anyhow!("MCP operation cancelled"));
        }
        result
    }

    #[cfg(feature = "rich-tui")]
    pub(crate) fn cancel_connection(&mut self, name: &str) {
        self.servers.retain(|server| server.live.name != name);
        self.set_status(
            name,
            McpStatus::Skipped("connection operation cancelled".into()),
        );
    }

    fn set_status(&mut self, name: &str, status: McpStatus) {
        if let Some((_, current)) = self.statuses.iter_mut().find(|(n, _)| n == name) {
            *current = status;
        } else {
            self.statuses.push((name.to_string(), status));
        }
    }

    #[cfg(feature = "rich-tui")]
    pub(crate) fn server_tools(&self, name: &str) -> Vec<newt_mcp_client::RemoteTool> {
        self.servers
            .iter()
            .find(|server| server.live.name == name)
            .map_or_else(Vec::new, |server| server.live.tools.clone())
    }
}

#[cfg(any(feature = "rich-tui", test))]
fn apply_login_net_grant(
    operation: &mut (newt_core::Caveats, Vec<String>),
    reconnect: &mut (newt_core::Caveats, Vec<String>),
    approved: &mut std::collections::BTreeSet<String>,
    host: String,
    (granted, hosts, remembered): (newt_core::Caveats, Vec<String>, bool),
) {
    if remembered {
        reconnect.0 = granted.clone();
        reconnect.1.extend(hosts.iter().cloned());
        reconnect.1.sort();
        reconnect.1.dedup();
    }
    approved.insert(host);
    // Every temporary member crossed the live gate; other axes remain clamped.
    let once = approved
        .iter()
        .cloned()
        .map(|host| (newt_core::DenialKind::Net, host))
        .collect::<Vec<_>>();
    operation.0 = newt_core::widen_caveats(&granted, &once);
    operation.1.extend(hosts);
    operation.1.sort();
    operation.1.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::caveats::Scope;

    #[test]
    fn management_oauth_once_then_session_does_not_promote_earlier_once_grant() {
        let mut base = newt_core::Caveats::top();
        base.net = Scope::none();
        let mut operation = (base.clone(), Vec::new());
        let mut reconnect = operation.clone();
        let mut approved = std::collections::BTreeSet::new();
        let mut once = base.clone();
        once.net = Scope::only(["once.example.test".into()]);
        apply_login_net_grant(
            &mut operation,
            &mut reconnect,
            &mut approved,
            "once.example.test".into(),
            (once, vec!["once.example.test".into()], false),
        );
        assert_eq!(reconnect.0, base);
        assert!(reconnect.1.is_empty());
        let mut session = base;
        session.net = Scope::only(["session.example.test".into()]);
        session.fs_write = Scope::none();
        apply_login_net_grant(
            &mut operation,
            &mut reconnect,
            &mut approved,
            "session.example.test".into(),
            (session.clone(), vec!["session.example.test".into()], true),
        );
        assert_eq!(
            operation.0.net,
            Scope::only(["once.example.test".into(), "session.example.test".into()])
        );
        assert_eq!(operation.0.fs_write, Scope::none());
        assert_eq!(reconnect.0, session);
        assert_eq!(reconnect.1, ["session.example.test"]);
        assert_eq!(operation.1, ["once.example.test", "session.example.test"]);
    }

    #[test]
    fn management_oauth_full_access_keeps_exact_once_names_out_of_reconnect() {
        let mut operation = (newt_core::Caveats::top(), vec![]);
        let mut reconnect = operation.clone();
        let mut approved = std::collections::BTreeSet::new();
        apply_login_net_grant(
            &mut operation,
            &mut reconnect,
            &mut approved,
            "private.example.test".into(),
            (
                newt_core::Caveats::top(),
                vec!["private.example.test".into()],
                false,
            ),
        );
        assert_eq!(operation.1, ["private.example.test"]);
        assert!(reconnect.1.is_empty());
    }
}
