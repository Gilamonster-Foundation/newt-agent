//! Conversation ownership of validated smart-harness capabilities.

use std::{collections::BTreeMap, sync::Arc};

use newt_core::{
    agentic::smart_harness::SmartHarness,
    config::{HarnessLaunch, SmartHarnessConfig},
    BackendKind, OpenAiApi,
};
use newt_inference::smart_harness::Auxiliary;
use serde_json::Value;

pub(super) struct Sessions {
    startup_caveats: newt_core::Caveats,
    sessions: BTreeMap<String, (Value, Arc<SmartHarness>)>,
}

impl Sessions {
    pub(super) fn new(startup_caveats: newt_core::Caveats) -> Self {
        Self {
            startup_caveats,
            sessions: BTreeMap::new(),
        }
    }

    fn preflight(
        &self,
        config: &SmartHarnessConfig,
        launch: &HarnessLaunch<'_>,
    ) -> anyhow::Result<()> {
        let directory = config.directory(launch)?;
        SmartHarnessConfig::validate_frame_directory(
            &directory,
            &self.startup_caveats,
            launch.workspace,
        )
    }

    pub(super) fn get(
        &mut self,
        conversation: (&str, bool),
        config: &SmartHarnessConfig,
        launch: &HarnessLaunch<'_>,
        primary_endpoint: &str,
        primary_kind: BackendKind,
        primary_api: OpenAiApi,
    ) -> anyhow::Result<Arc<SmartHarness>> {
        self.preflight(config, launch)?;
        let mut auxiliary =
            newt_inference::smart_harness::build(config, primary_endpoint, primary_kind)?;
        auxiliary.manifest["primary_api"] = serde_json::json!(primary_api.label());
        self.admit(conversation, config, launch, auxiliary)
    }

    fn admit(
        &mut self,
        conversation: (&str, bool),
        config: &SmartHarnessConfig,
        launch: &HarnessLaunch<'_>,
        auxiliary: Auxiliary,
    ) -> anyhow::Result<Arc<SmartHarness>> {
        self.preflight(config, launch)?;
        let resolved =
            serde_json::to_value(config.session_config(launch, auxiliary.manifest.clone())?)?;
        if let Some((previous, harness)) = self.sessions.get(conversation.0) {
            anyhow::ensure!(
                previous == &resolved,
                "smart-harness authority or settings changed; start a new conversation"
            );
            return Ok(harness.clone());
        }
        let session =
            config.open_conversation(launch, conversation.0, conversation.1, auxiliary.manifest)?;
        let harness = Arc::new(SmartHarness::new(
            session,
            auxiliary.complete,
            config.adjudication.clone(),
        )?);
        self.sessions
            .insert(conversation.0.to_owned(), (resolved, harness.clone()));
        Ok(harness)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::config::HarnessLaunch;

    /// Grounds conversation isolation in the same real durable directory used by chat.
    #[test]
    fn conversations_reuse_only_matching_current_authority_and_settings() {
        let workspace = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let caveats = newt_core::confined_exec::build_tool_caveats(workspace.path());
        let launch = HarnessLaunch {
            workspace: workspace.path(),
            caveats: &caveats,
            frame_dir: None,
            resume_from: None,
            hermetic: false,
        };
        let config = SmartHarnessConfig {
            frame_dir: Some(private.path().join("frame")),
            ..Default::default()
        };
        let auxiliary = || newt_inference::smart_harness::Auxiliary {
            complete: Arc::new(|_| Box::pin(async { Ok("answer".into()) })),
            manifest: serde_json::json!({"model":"fixture","placement":"cpu"}),
        };
        let mut sessions = Sessions::new(caveats.clone());
        let first = sessions
            .admit(("one", false), &config, &launch, auxiliary())
            .unwrap();
        let same = sessions
            .admit(("one", true), &config, &launch, auxiliary())
            .unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        let other = sessions
            .admit(("two", false), &config, &launch, auxiliary())
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &other));
        let mut changed = config.clone();
        changed.max_dereferences += 1;
        assert!(sessions
            .admit(("one", true), &changed, &launch, auxiliary())
            .is_err());
        let changed = SmartHarnessConfig {
            frame_dir: Some(private.path().join("other-frame")),
            ..config.clone()
        };
        assert!(sessions
            .admit(("one", true), &changed, &launch, auxiliary())
            .is_err());
        assert!(Arc::ptr_eq(
            &first,
            &sessions
                .admit(("one", true), &config, &launch, auxiliary())
                .unwrap()
        ));
    }

    /// Grounds the retained MCP startup grant against real symlink retargeting.
    #[cfg(unix)]
    #[test]
    fn new_conversation_cannot_move_frame_into_a_retained_mcp_grant() {
        let workspace = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let old_grant = tempfile::tempdir().unwrap();
        let current = newt_core::confined_exec::build_tool_caveats(workspace.path());
        let mut startup = current.clone();
        let newt_core::Scope::Only(reads) = &current.fs_read else {
            panic!("fixture requires confined reads");
        };
        startup.fs_read = newt_core::Scope::only(reads.iter().cloned().chain(std::iter::once(
            old_grant.path().to_str().unwrap().to_owned(),
        )));
        let original = private.path().join("original");
        std::fs::create_dir(&original).unwrap();
        let link = private.path().join("frame-link");
        std::os::unix::fs::symlink(&original, &link).unwrap();
        let config = SmartHarnessConfig {
            frame_dir: Some(link.clone()),
            ..Default::default()
        };
        let mut launch = HarnessLaunch {
            workspace: workspace.path(),
            caveats: &startup,
            frame_dir: None,
            resume_from: None,
            hermetic: false,
        };
        config.directory(&launch).unwrap();
        let mut sessions = Sessions::new(startup.clone());
        let auxiliary = || Auxiliary {
            complete: Arc::new(|_| Box::pin(async { Ok("answer".into()) })),
            manifest: serde_json::json!({"model":"fixture","placement":"cpu"}),
        };
        sessions
            .admit(("original", false), &config, &launch, auxiliary())
            .unwrap();
        launch.caveats = &current;
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(old_grant.path(), &link).unwrap();
        config.directory(&launch).unwrap();
        let Err(error) = sessions.admit(("new", false), &config, &launch, auxiliary()) else {
            panic!("the live MCP process retains its broader startup grant");
        };
        assert!(error
            .to_string()
            .contains("overlaps model filesystem authority"));
    }
}
