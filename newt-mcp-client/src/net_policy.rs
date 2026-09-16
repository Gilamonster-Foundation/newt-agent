//! Named HTTP approvals survive lowering public egress to an unrestricted scope.
//!
//! Model: GPT-6 | Harness: Codex | Operator: S Hartsock | Time: 12:01 EDT | Date: 2026-09-15

use std::collections::BTreeSet;

use newt_core::Scope;

use super::{canonical_granted_host, net_scope_permits_http_host};

/// Transient HTTP/OAuth policy: active authority plus operator-named approvals.
/// The names come from trusted configuration or an explicit permission decision,
/// never from discovered MCP metadata. This policy is not a capability grant.
#[derive(Clone, Debug)]
pub struct HttpNetworkPolicy {
    net: Scope<String>,
    explicit_hosts: BTreeSet<String>,
}

impl HttpNetworkPolicy {
    #[must_use]
    pub fn new(net: &Scope<String>) -> Self {
        Self::with_explicit_hosts(net, &[])
    }

    /// Retain exact names even when `net` was lowered from a wildcard or full access.
    #[must_use]
    pub fn with_explicit_hosts(net: &Scope<String>, configured: &[String]) -> Self {
        let mut explicit_hosts: BTreeSet<_> = configured
            .iter()
            .filter_map(|host| {
                let canonical = canonical_granted_host(host);
                if canonical.is_none() && host != "*" {
                    tracing::warn!("ignoring invalid exact MCP net grant: use a bare hostname or IP without a scheme, port, path, or wildcard pattern");
                }
                canonical
            })
            .collect();
        if let Scope::Only(hosts) = net {
            explicit_hosts.extend(hosts.iter().filter_map(|host| canonical_granted_host(host)));
        }
        Self {
            net: net.clone(),
            explicit_hosts,
        }
    }

    #[must_use]
    pub fn permits_host(&self, host: &str) -> bool {
        net_scope_permits_http_host(&self.net, host)
    }

    #[must_use]
    pub fn explicitly_grants_host(&self, host: &str) -> bool {
        self.permits_host(host)
            && canonical_granted_host(host).is_some_and(|host| self.explicit_hosts.contains(&host))
    }
}

/// A host approval the interactive caller can request before retrying a connection.
#[derive(Debug)]
pub struct HttpNetGrantRequired {
    pub host: String,
    pub private: bool,
}

impl std::fmt::Display for HttpNetGrantRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host `{}` {} — grant the exact hostname in trusted [tui.permissions] net, then restart; wildcard/full-access alone does not approve private hosts",
            self.host,
            if self.private {
                "resolved to a private/non-global address without an exact net grant"
            } else {
                "is outside the session net allow-list"
            })
    }
}

impl std::error::Error for HttpNetGrantRequired {}
