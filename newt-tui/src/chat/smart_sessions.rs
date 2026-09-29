//! Conversation ownership of validated smart-harness capabilities.

use std::{collections::BTreeMap, sync::Arc};

use newt_core::{
    agentic::smart_harness::SmartHarness,
    config::{HarnessLaunch, SmartHarnessConfig},
    BackendKind, OpenAiApi,
};
use newt_inference::smart_harness::Auxiliary;
use serde_json::Value;

pub(super) fn adoption_selection_matches(selected: &str, start: &crate::SessionStart) -> bool {
    matches!(start, crate::SessionStart::ResumeNamed(name) if name == selected.trim())
}

pub(super) struct Sessions {
    startup_caveats: newt_core::Caveats,
    sessions: BTreeMap<String, CachedSession>,
    adoption: Option<(String, Vec<Value>)>,
}

struct CachedSession {
    selection: Value,
    configuration: Value,
    harness: Arc<SmartHarness>,
}

impl Sessions {
    pub(super) fn new(startup_caveats: newt_core::Caveats) -> Self {
        Self {
            startup_caveats,
            sessions: BTreeMap::new(),
            adoption: None,
        }
    }

    /// Prepare only from the store's verified snapshot, never current wire input.
    /// Reuse the existing persisted-compaction cut without invoking a summarizer.
    pub(super) fn adopt_selected(
        &mut self,
        store: &newt_core::ConversationStore,
        selected: &str,
        budget: u32,
    ) -> anyhow::Result<()> {
        use newt_core::MemoryProvider;
        let record = store.load_verified(selected)?;
        let mut projection = newt_core::Summarizing::new(budget);
        projection.restore_turns(&record.turns);
        let mut messages = projection.build_messages("", "");
        messages.pop(); // Empty new-task slot; the real current ask is appended later.
        let history = messages.into_iter().skip(1).map(|message| {
            serde_json::json!({"role":message.role.as_str(),"content":message.content})
        }).collect();
        self.adoption = Some((record.id, history));
        Ok(())
    }

    fn adoption_history(&self, conversation: &str) -> Option<&[Value]> {
        self.adoption
            .as_ref()
            .filter(|(id, _)| id == conversation)
            .map(|(_, history)| history.as_slice())
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
        primary: (&str, BackendKind, OpenAiApi),
        build: impl FnOnce() -> anyhow::Result<Auxiliary>,
    ) -> anyhow::Result<Arc<SmartHarness>> {
        self.preflight(config, launch)?;
        let selection = serde_json::json!({"configuration":config,"primary":primary});
        if let Some(cached) = self.sessions.get(conversation.0) {
            let current = serde_json::to_value(
                config.session_config(launch, cached.configuration["auxiliary"].clone())?,
            )?;
            anyhow::ensure!(
                cached.selection == selection && cached.configuration == current,
                "smart-harness authority or settings changed; start a new conversation"
            );
            // Embedded conversations retain their pinned bytes. External
            // factories still recheck mutable credentials and endpoint validity.
            if config.backend.is_none() {
                return Ok(cached.harness.clone());
            }
        }
        let mut auxiliary = build()?;
        auxiliary.manifest["primary_api"] = serde_json::json!(primary.2.label());
        let resolved =
            serde_json::to_value(config.session_config(launch, auxiliary.manifest.clone())?)?;
        if let Some(cached) = self.sessions.get(conversation.0) {
            anyhow::ensure!(
                cached.configuration == resolved,
                "smart-harness authority or settings changed; start a new conversation"
            );
            return Ok(cached.harness.clone());
        }
        let session = match self.adoption_history(conversation.0) {
            Some(history) => {
                config.adopt_conversation(launch, conversation.0, history, auxiliary.manifest)?
            }
            None => config.open_conversation(
                launch,
                conversation.0,
                conversation.1,
                auxiliary.manifest,
            )?,
        };
        let harness = Arc::new(SmartHarness::new(
            session,
            auxiliary.complete,
            config.adjudication.clone(),
        )?);
        if self.adoption_history(conversation.0).is_some() {
            self.adoption = None;
        }
        self.sessions.insert(
            conversation.0.to_owned(),
            CachedSession {
                selection,
                configuration: resolved,
                harness: harness.clone(),
            },
        );
        Ok(harness)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::config::HarnessLaunch;

    struct Fixture {
        workspace: tempfile::TempDir,
        _private: tempfile::TempDir,
        caveats: newt_core::Caveats,
        config: SmartHarnessConfig,
    }

    impl Fixture {
        fn new() -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let private = tempfile::tempdir().unwrap();
            let caveats = newt_core::confined_exec::build_tool_caveats(workspace.path());
            let config = SmartHarnessConfig {
                frame_dir: Some(private.path().join("frame")),
                ..Default::default()
            };
            Self {
                workspace,
                _private: private,
                caveats,
                config,
            }
        }

        fn launch(&self) -> HarnessLaunch<'_> {
            HarnessLaunch {
                workspace: self.workspace.path(),
                caveats: &self.caveats,
                frame_dir: None,
                resume_from: None,
                hermetic: false,
            }
        }
    }

    /// Grounds adoption scope in a verified durable conversation snapshot;
    /// the selected snapshot must never follow a later tab switch.
    #[test]
    fn adoption_snapshot_is_verified_and_bound_to_one_conversation() {
        let fixture = Fixture::new();
        let storage = tempfile::tempdir().unwrap();
        let store =
            newt_core::ConversationStore::new(storage.path(), fixture.workspace.path(), 100)
                .unwrap();
        let selected = store.create("selected", None).unwrap();
        store
            .append_turn(&selected, "preserve this task", "historical cargo denial")
            .unwrap();
        let mut sessions = Sessions::new(fixture.caveats.clone());
        sessions.adopt_selected(&store, &selected, 4096).unwrap();
        assert!(sessions.adoption_history("other").is_none());
        let history = sessions.adoption_history(&selected).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["content"], "preserve this task");
        assert_eq!(history[1]["content"], "historical cargo denial");
        assert!(history.iter().all(|message| message["role"] != "system"));
        assert!(sessions
            .adopt_selected(&store, "missing-conversation", 4096)
            .is_err());
        assert!(
            sessions.adoption_history(&selected).is_some(),
            "failed preparation preserves the selected snapshot"
        );
    }

    #[test]
    fn adoption_snapshot_uses_existing_persisted_compaction_cut() {
        let fixture = Fixture::new();
        let storage = tempfile::tempdir().unwrap();
        let store =
            newt_core::ConversationStore::new(storage.path(), fixture.workspace.path(), 100)
                .unwrap();
        let selected = store.create("selected", None).unwrap();
        store
            .append_turn(&selected, "obsolete task", "old blocker")
            .unwrap();
        let summary = "[CONTEXT COMPACTION — REFERENCE ONLY]\npreserve refactor edits";
        newt_core::persist_compaction_summary(&store, &selected, summary).unwrap();
        store
            .append_turn(&selected, "latest saved instruction", "saved response")
            .unwrap();
        let mut sessions = Sessions::new(fixture.caveats.clone());
        sessions.adopt_selected(&store, &selected, 4096).unwrap();
        let history = sessions.adoption_history(&selected).unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0]["content"], summary);
        assert_eq!(history[1]["content"], "latest saved instruction");
        assert!(!serde_json::to_string(history)
            .unwrap()
            .contains("old blocker"));
    }

    #[test]
    fn adoption_consent_does_not_extend_to_other_tabs_or_reimport_after_restart() {
        let fixture = Fixture::new();
        let storage = tempfile::tempdir().unwrap();
        let store =
            newt_core::ConversationStore::new(storage.path(), fixture.workspace.path(), 100)
                .unwrap();
        let selected = store.create("selected", None).unwrap();
        store
            .append_turn(&selected, "old task", "old claim")
            .unwrap();
        let primary = (
            "http://primary.invalid",
            BackendKind::Openai,
            OpenAiApi::ChatCompletions,
        );
        let build = || {
            Ok(Auxiliary {
                complete: Arc::new(|_| panic!("adoption must not run inference")),
                manifest: serde_json::json!({"backend":"embedded","model":"fixture","placement":"cpu"}),
            })
        };
        let mut sessions = Sessions::new(fixture.caveats.clone());
        sessions.adopt_selected(&store, &selected, 4096).unwrap();
        assert!(sessions
            .get(
                ("other", true),
                &fixture.config,
                &fixture.launch(),
                primary,
                build
            )
            .is_err());
        let harness = sessions
            .get(
                (&selected, true),
                &fixture.config,
                &fixture.launch(),
                primary,
                build,
            )
            .unwrap();
        assert!(sessions.adoption_history(&selected).is_none());
        assert!(harness.head().is_ok());
        drop(harness);
        drop(sessions);
        store
            .append_turn(&selected, "later database history", "not reimported")
            .unwrap();
        let mut reopened = Sessions::new(fixture.caveats.clone());
        let restored = reopened
            .get(
                (&selected, true),
                &fixture.config,
                &fixture.launch(),
                primary,
                build,
            )
            .unwrap();
        drop(restored);
        drop(reopened);
        let mut manifest = build().unwrap().manifest;
        manifest["primary_api"] = serde_json::json!(primary.2.label());
        let restored = fixture
            .config
            .open_conversation(&fixture.launch(), &selected, true, manifest)
            .unwrap();
        let messages = restored.restored_messages().unwrap();
        assert_eq!(
            messages.len(),
            3,
            "notice and original historical messages only"
        );
        let text = serde_json::to_string(&messages).unwrap();
        assert!(text.contains("old task") && text.contains("old claim"));
        assert!(!text.contains("later database history") && !text.contains("not reimported"));
    }

    #[test]
    fn adoption_selection_matches_normal_resume_trimming_but_rejects_overrides() {
        use crate::SessionStart;
        assert!(adoption_selection_matches(
            " saved ",
            &SessionStart::ResumeNamed("saved".into())
        ));
        for start in [
            SessionStart::ResumeNamed("other".into()),
            SessionStart::ResumeExact("saved".into()),
            SessionStart::ResumeLatest,
            SessionStart::Fresh,
            SessionStart::Ephemeral,
        ] {
            assert!(!adoption_selection_matches("saved", &start), "{start:?}");
        }
    }

    #[test]
    fn adoption_refuses_tampered_source_before_preparing_any_history() {
        let fixture = Fixture::new();
        let storage = tempfile::tempdir().unwrap();
        let store =
            newt_core::ConversationStore::new(storage.path(), fixture.workspace.path(), 100)
                .unwrap();
        let selected = store.create("selected", None).unwrap();
        store.append_turn(&selected, "u1", "a1").unwrap();
        store.append_turn(&selected, "u2", "a2").unwrap();
        let connection =
            rusqlite::Connection::open(storage.path().join("conversations.db")).unwrap();
        assert_eq!(connection.execute("UPDATE turns SET assistant='forged' WHERE conversation_id=?1 AND seq=(SELECT MIN(seq) FROM turns WHERE conversation_id=?1)", [&selected]).unwrap(), 1);
        let mut sessions = Sessions::new(fixture.caveats.clone());
        assert!(sessions.adopt_selected(&store, &selected, 4096).is_err());
        assert!(sessions.adoption_history(&selected).is_none());
        assert!(!fixture.config.frame_dir.as_ref().unwrap().exists());
    }

    #[test]
    fn adoption_applies_the_current_provider_disclosure_guard_when_persisting() {
        let fixture = Fixture::new();
        let storage = tempfile::tempdir().unwrap();
        let store =
            newt_core::ConversationStore::new(storage.path(), fixture.workspace.path(), 100)
                .unwrap();
        let selected = store.create("selected", None).unwrap();
        let secret = "opaque-provider-credential-fixture-6482";
        store.append_turn(&selected, "old task", secret).unwrap();
        let primary = (
            "http://primary.invalid",
            BackendKind::Openai,
            OpenAiApi::ChatCompletions,
        );
        let manifest =
            serde_json::json!({"backend":"embedded","model":"fixture","placement":"cpu"});
        let mut sessions = Sessions::new(fixture.caveats.clone());
        sessions.adopt_selected(&store, &selected, 4096).unwrap();
        assert!(
            serde_json::to_string(sessions.adoption_history(&selected).unwrap())
                .unwrap()
                .contains(secret)
        );
        let _disclosure = newt_core::ocap::scoped_session_disclosure(
            newt_core::ocap::session_disclosure_filter(Some(secret)),
        );
        let harness = sessions
            .get(
                (&selected, true),
                &fixture.config,
                &fixture.launch(),
                primary,
                || {
                    Ok(Auxiliary {
                        complete: Arc::new(|_| panic!("adoption must not run inference")),
                        manifest: manifest.clone(),
                    })
                },
            )
            .unwrap();
        drop(harness);
        drop(sessions);
        let mut resolved = manifest;
        resolved["primary_api"] = serde_json::json!(primary.2.label());
        let restored = fixture
            .config
            .open_conversation(&fixture.launch(), &selected, true, resolved)
            .unwrap();
        let text = serde_json::to_string(&restored.restored_messages().unwrap()).unwrap();
        assert!(
            !text.contains(secret),
            "provider value must never enter the frame"
        );
        assert!(text.contains("REDACTED"), "{text}");
    }

    /// Exercises the same lazy factory used by chat; the real pinned-file test
    /// below grounds its asset ownership in the production embedded builder.
    #[test]
    fn get_constructs_once_and_retains_assets_until_the_conversation_is_released() {
        let fixture = Fixture::new();
        let config = &fixture.config;
        let launch = fixture.launch();
        let mut sessions = Sessions::new(fixture.caveats.clone());
        let primary = (
            "http://primary.invalid",
            BackendKind::Openai,
            OpenAiApi::ChatCompletions,
        );
        let calls = std::cell::Cell::new(0);
        let held_assets = std::cell::RefCell::new(Vec::new());
        let build = || {
            calls.set(calls.get() + 1);
            let assets = Arc::new("synthetic pinned assets".to_owned());
            held_assets.borrow_mut().push(Arc::downgrade(&assets));
            Ok(Auxiliary {
                complete: Arc::new(move |_| {
                    let assets = assets.clone();
                    Box::pin(async move { Ok(((*assets).clone(), None)) })
                }),
                manifest: serde_json::json!({"backend":"embedded","model":"fixture","placement":"cpu"}),
            })
        };
        let first = sessions
            .get(("one", false), config, &launch, primary, build)
            .unwrap();
        let same = sessions
            .get(("one", true), config, &launch, primary, build)
            .unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        assert_eq!(
            calls.get(),
            1,
            "cached get must not reconstruct the auxiliary"
        );
        let other = sessions
            .get(("two", false), config, &launch, primary, build)
            .unwrap();
        assert_eq!(calls.get(), 2, "a new conversation pins its own assets");
        assert!(!Arc::ptr_eq(&first, &other));
        drop(sessions);
        drop(first);
        assert!(held_assets.borrow()[0].upgrade().is_some());
        drop(same);
        assert!(held_assets.borrow()[0].upgrade().is_none());
        assert!(held_assets.borrow()[1].upgrade().is_some());
        drop(other);
        assert!(held_assets.borrow()[1].upgrade().is_none());
    }

    #[test]
    fn get_rejects_current_configuration_and_authority_drift_before_building() {
        let fixture = Fixture::new();
        let config = &fixture.config;
        let launch = fixture.launch();
        let mut sessions = Sessions::new(fixture.caveats.clone());
        let primary = (
            "http://primary.invalid",
            BackendKind::Openai,
            OpenAiApi::ChatCompletions,
        );
        let calls = std::cell::Cell::new(0);
        let build = || {
            calls.set(calls.get() + 1);
            Ok(Auxiliary {
                complete: Arc::new(|_| Box::pin(async { Ok(("answer".into(), None)) })),
                manifest: serde_json::json!({"backend":"embedded","model":"fixture","placement":"cpu"}),
            })
        };
        sessions
            .get(("one", false), config, &launch, primary, build)
            .unwrap();
        for change in [
            |c: &mut SmartHarnessConfig| c.max_dereferences += 1,
            |c: &mut SmartHarnessConfig| c.adjudication.timeout_ms += 1,
            |c: &mut SmartHarnessConfig| c.model = Some("another-model".into()),
        ] {
            let mut changed = fixture.config.clone();
            change(&mut changed);
            assert!(sessions
                .get(("one", true), &changed, &launch, primary, build)
                .is_err());
            assert_eq!(
                calls.get(),
                1,
                "configuration drift must refuse before asset loading"
            );
        }
        let mut caveats = fixture.caveats.clone();
        caveats.net = newt_core::Scope::All;
        let changed_launch = HarnessLaunch {
            caveats: &caveats,
            ..fixture.launch()
        };
        assert!(sessions
            .get(("one", true), config, &changed_launch, primary, build)
            .is_err());
        for changed in [
            ("http://other-primary.invalid", primary.1, primary.2),
            (primary.0, BackendKind::Ollama, primary.2),
            (primary.0, primary.1, OpenAiApi::Responses),
        ] {
            assert!(sessions
                .get(("one", true), config, &launch, changed, build)
                .is_err());
        }
        assert_eq!(calls.get(), 1);
    }

    /// Grounds the cache's unchanged external validation in a real credential
    /// file; reusing embedded assets must not bypass this independent check.
    #[test]
    fn get_rechecks_external_credential_availability() {
        let mut fixture = Fixture::new();
        let credentials = tempfile::tempdir().unwrap();
        let key = credentials.path().join("auxiliary.key");
        std::fs::write(&key, b"synthetic-auxiliary-credential").unwrap();
        fixture.config.device = Some("cpu".into());
        fixture.config.backend = Some(newt_core::config::BackendRef {
            kind: Some(BackendKind::Openai),
            endpoint: Some("http://auxiliary.invalid".into()),
            model: Some("fixture".into()),
            api_key_file: Some(key.to_str().unwrap().into()),
            ..Default::default()
        });
        let primary = (
            "http://primary.invalid",
            BackendKind::Openai,
            OpenAiApi::ChatCompletions,
        );
        let config = &fixture.config;
        let launch = fixture.launch();
        let mut sessions = Sessions::new(fixture.caveats.clone());
        let calls = std::cell::Cell::new(0);
        let build = || {
            calls.set(calls.get() + 1);
            newt_inference::smart_harness::build(config, primary.0, primary.1)
        };
        sessions
            .get(("one", false), config, &launch, primary, build)
            .unwrap();
        std::fs::remove_file(&key).unwrap();
        let Err(error) = sessions.get(("one", true), config, &launch, primary, build) else {
            panic!("missing configured credentials must still refuse");
        };
        assert!(error.to_string().contains("credential is unavailable"));
        assert_eq!(calls.get(), 2);
    }

    /// Real files ground factory reuse: live conversations own pinned bytes;
    /// deleting the old paths matters only when constructing another conversation.
    #[cfg(feature = "embedded")]
    #[test]
    fn get_keeps_embedded_assets_pinned_across_path_changes_and_loads_new_conversations() {
        let mut fixture = Fixture::new();
        let assets = tempfile::tempdir().unwrap();
        let weights = assets.path().join("fixture.gguf");
        let tokenizer = assets.path().join("tokenizer.json");
        std::fs::write(&weights, b"original weights").unwrap();
        std::fs::write(&tokenizer, b"{}").unwrap();
        fixture.config.model = Some("qwen2.5-0.5b".into());
        fixture.config.model_path = Some(weights.clone());
        let config = &fixture.config;
        let launch = fixture.launch();
        let mut sessions = Sessions::new(fixture.caveats.clone());
        let primary = (
            "http://primary.invalid",
            BackendKind::Openai,
            OpenAiApi::ChatCompletions,
        );
        let calls = std::cell::Cell::new(0);
        let manifests = std::cell::RefCell::new(Vec::new());
        let build = || {
            calls.set(calls.get() + 1);
            let auxiliary = newt_inference::smart_harness::build(config, primary.0, primary.1)?;
            manifests.borrow_mut().push(auxiliary.manifest.clone());
            Ok(auxiliary)
        };
        let first = sessions
            .get(("one", false), config, &launch, primary, build)
            .unwrap();
        std::fs::remove_file(&weights).unwrap();
        std::fs::remove_file(&tokenizer).unwrap();
        let same = sessions
            .get(("one", true), config, &launch, primary, build)
            .expect("cached conversation must keep its already pinned assets");
        assert!(Arc::ptr_eq(&first, &same));
        assert_eq!(calls.get(), 1);
        assert!(sessions
            .get(("two", false), config, &launch, primary, build)
            .is_err());
        std::fs::write(&weights, b"replacement weights").unwrap();
        std::fs::write(&tokenizer, b"{}").unwrap();
        let other = sessions
            .get(("two", false), config, &launch, primary, build)
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &other));
        assert_eq!(calls.get(), 3);
        let manifests = manifests.borrow();
        assert_ne!(manifests[0]["weights"], manifests[1]["weights"]);
        assert_eq!(manifests[0]["tokenizer"], manifests[1]["tokenizer"]);
    }

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
        let primary = (
            "http://primary.invalid",
            BackendKind::Openai,
            OpenAiApi::ChatCompletions,
        );
        let auxiliary = || {
            Ok(Auxiliary {
                complete: Arc::new(|_| Box::pin(async { Ok(("answer".into(), None)) })),
                manifest: serde_json::json!({"model":"fixture","placement":"cpu"}),
            })
        };
        let mut sessions = Sessions::new(caveats.clone());
        let first = sessions
            .get(("one", false), &config, &launch, primary, auxiliary)
            .unwrap();
        let same = sessions
            .get(("one", true), &config, &launch, primary, auxiliary)
            .unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        let other = sessions
            .get(("two", false), &config, &launch, primary, auxiliary)
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &other));
        let mut changed = config.clone();
        changed.max_dereferences += 1;
        assert!(sessions
            .get(("one", true), &changed, &launch, primary, auxiliary)
            .is_err());
        let changed = SmartHarnessConfig {
            frame_dir: Some(private.path().join("other-frame")),
            ..config.clone()
        };
        assert!(sessions
            .get(("one", true), &changed, &launch, primary, auxiliary)
            .is_err());
        assert!(Arc::ptr_eq(
            &first,
            &sessions
                .get(("one", true), &config, &launch, primary, auxiliary)
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
        let primary = (
            "http://primary.invalid",
            BackendKind::Openai,
            OpenAiApi::ChatCompletions,
        );
        let auxiliary = || {
            Ok(Auxiliary {
                complete: Arc::new(|_| Box::pin(async { Ok(("answer".into(), None)) })),
                manifest: serde_json::json!({"model":"fixture","placement":"cpu"}),
            })
        };
        sessions
            .get(("original", false), &config, &launch, primary, auxiliary)
            .unwrap();
        launch.caveats = &current;
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(old_grant.path(), &link).unwrap();
        config.directory(&launch).unwrap();
        let Err(error) = sessions.get(("new", false), &config, &launch, primary, auxiliary) else {
            panic!("the live MCP process retains its broader startup grant");
        };
        assert!(error
            .to_string()
            .contains("overlaps model filesystem authority"));
    }
}
