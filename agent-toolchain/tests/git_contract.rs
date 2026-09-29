#![cfg(feature = "embedded-git")]

use agent_mesh_protocol::caveats::{Caveats, Scope};
use agent_toolchain::embedded_git::{check_git_read_scope, GitTool};
use agent_toolchain::git_caveats::GitCaveats;

struct Adapter;

impl GitTool for Adapter {
    fn dispatch(
        &self,
        op: &str,
        _args: &serde_json::Value,
        _git: &GitCaveats,
        session: &Caveats,
    ) -> Result<String, String> {
        check_git_read_scope(op, &session.fs_read)?;
        Ok(op.into())
    }
}

/// A consumer outside newt-core can implement the adapter with the published
/// protocol's exact authority types. Extraction must not introduce an adapter
/// that grants authority or a Newt dependency to use this contract.
#[test]
fn independent_consumer_preserves_the_existing_read_boundary() {
    let mut session = Caveats::top();
    let git = GitCaveats::read_only();
    let args = serde_json::json!({});
    assert_eq!(
        Adapter.dispatch("status", &args, &git, &session).unwrap(),
        "status"
    );
    session.fs_read = Scope::none();
    assert!(Adapter.dispatch("status", &args, &git, &session).is_err());
    assert!(!git.permits_commit());
}
