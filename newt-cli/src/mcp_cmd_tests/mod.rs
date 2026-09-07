//! Test families for [`super`] (`newt_agent::mcp_cmd`), split out of
//! `mcp_cmd.rs` by what each test asserts. Sibling files; no behaviour change.
//!
//! The module is still named `tests`: `.github/workflows/mcp-import-real.yml`
//! selects the real-resource transaction tests with the filter
//! `mcp_cmd::tests::`, and that prefix must keep matching.
//!
//! `stdio_entry` lives here because eight tests in five families build one.

// A glob RE-EXPORT, not a plain glob: the moved bodies write `super::X` from
// when `super` was `mcp_cmd`, and a private glob binding is not nameable by
// path from a child module.
pub(crate) use super::*;
pub(crate) use clap::Parser;

fn stdio_entry(name: &str, command: Option<&str>) -> McpServerEntry {
    McpServerEntry {
        name: name.into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: command.map(str::to_string),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        request_timeout_secs: None,
        trust: McpTrust::Trusted,
    }
}

#[cfg(test)]
#[path = "binary_resolution.rs"]
mod binary_resolution;
#[cfg(test)]
#[path = "import_naming.rs"]
mod import_naming;
#[cfg(test)]
#[path = "import_selection.rs"]
mod import_selection;
#[cfg(test)]
#[path = "import_transaction.rs"]
mod import_transaction;
#[cfg(test)]
#[path = "import_url.rs"]
mod import_url;
#[cfg(test)]
#[path = "invocation.rs"]
mod invocation;
#[cfg(test)]
#[path = "listing.rs"]
mod listing;
#[cfg(test)]
#[path = "registration_entry.rs"]
mod registration_entry;
