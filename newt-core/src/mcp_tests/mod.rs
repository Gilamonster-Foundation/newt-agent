use super::*;

fn stdio(name: &str, command: &str) -> McpServerEntry {
    McpServerEntry {
        name: name.into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some(command.into()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        request_timeout_secs: None,
        trust: McpTrust::Trusted,
    }
}

// Families beside this file. Both attributes are required: rustc needs only
// the `#[path]`, but the ratchets' shared scanner resolves a child ONLY when
// a `#[cfg(test)]` immediately precedes the `mod` (#2149).
#[cfg(test)]
#[path = "admission.rs"]
mod admission;
#[cfg(test)]
#[path = "claude_source.rs"]
mod claude_source;
#[cfg(test)]
#[path = "codex_source.rs"]
mod codex_source;
#[cfg(test)]
#[path = "discovery.rs"]
mod discovery;
#[cfg(test)]
#[path = "http_url.rs"]
mod http_url;
#[cfg(test)]
#[path = "interpolation.rs"]
mod interpolation;
#[cfg(test)]
#[path = "newt_mcp_toml.rs"]
mod newt_mcp_toml;
#[cfg(test)]
#[path = "precedence.rs"]
mod precedence;
#[cfg(test)]
#[path = "secret_value.rs"]
mod secret_value;
#[cfg(test)]
#[path = "transport.rs"]
mod transport;
#[cfg(test)]
#[path = "trust_boundary.rs"]
mod trust_boundary;
