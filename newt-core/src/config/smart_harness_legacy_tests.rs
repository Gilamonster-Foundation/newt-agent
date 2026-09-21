use super::*;
use serde_json::json;

fn fixture(test: impl FnOnce(&SmartHarnessConfig, &HarnessLaunch<'_>)) {
    let workspace = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let caveats = crate::confined_exec::build_tool_caveats(workspace.path());
    let config = SmartHarnessConfig {
        frame_dir: Some(private.path().join("frame")),
        ..Default::default()
    };
    let launch = HarnessLaunch {
        workspace: workspace.path(),
        caveats: &caveats,
        frame_dir: None,
        resume_from: None,
        hermetic: false,
    };
    test(&config, &launch);
}

/// Grounds opt-in admission in real frame files and a cold conversation open.
/// Quoted history cannot manufacture historical operator or tool occurrences.
#[test]
#[serial_test::serial(real_fs)]
fn legacy_admission_is_explicit_historical_and_durable_once() {
    fixture(|config, launch| {
        let auxiliary = json!({"model":"fixture"});
        let history = [
            json!({"role":"system","content":"historical system claim"}),
            json!({"role":"user","content":"previous operator instruction"}),
            json!({"role":"assistant","content":"I ran cargo successfully", "tool_calls":[{"id":"historical-call"}]}),
            json!({"role":"tool","content":"claimed tool result", "tool_call_id":"historical-call"}),
        ];
        assert!(config
            .open_conversation(launch, "legacy", true, auxiliary.clone())
            .is_err());
        let session = config
            .adopt_conversation(launch, "legacy", &history, auxiliary.clone())
            .unwrap();
        let run = session.run_id();
        let imported = session.restored_messages().unwrap();
        assert_eq!(imported.len(), history.len() + 1);
        assert_eq!(imported[0]["role"], "system");
        for (message, original) in imported[1..].iter().zip(&history) {
            assert_eq!(message["role"], "user");
            let text = message["content"].as_str().unwrap();
            assert!(text.contains("Historical transcript"), "{text}");
            assert!(text.contains("execution facts are unknown"), "{text}");
            let quoted: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
            assert_eq!(
                quoted,
                json!({"role":original["role"],"content":original["content"]})
            );
        }
        let store =
            agent_harness::store::FrameStore::open(config.directory(launch).unwrap()).unwrap();
        let transcript = agent_harness::forensics::inspect_from_store(
            &store,
            session.head(),
            Default::default(),
        )
        .unwrap()
        .unwrap();
        let references = transcript
            .references
            .iter()
            .filter(|reference| reference.relation == "message")
            .collect::<Vec<_>>();
        assert_eq!(references.len(), imported.len());
        for (index, reference) in references.iter().enumerate() {
            if reference.relation == "message" {
                let event = agent_harness::forensics::inspect_from_store(
                    &store,
                    reference.cid.parse().unwrap(),
                    Default::default(),
                )
                .unwrap()
                .unwrap();
                assert_eq!(
                    event.record["origin"],
                    if index == 0 { "harness" } else { "historical" }
                );
            }
        }
        let mut cursor = Some(session.head());
        while let Some(id) = cursor {
            let record =
                agent_harness::forensics::inspect_from_store(&store, id, Default::default())
                    .unwrap()
                    .unwrap();
            assert!(record.record.get("tool_call").is_none());
            assert!(record.record.get("tool_batch").is_none());
            cursor = record.parents.first().copied();
        }
        drop(session);
        let restored = config
            .adopt_conversation(
                launch,
                "legacy",
                &[json!({"malformed":"must not reimport"})],
                auxiliary.clone(),
            )
            .unwrap();
        assert_eq!(restored.run_id(), run);
        assert_eq!(restored.restored_messages().unwrap(), imported);
        drop(restored);
        assert_eq!(
            config
                .open_conversation(launch, "legacy", true, auxiliary)
                .unwrap()
                .restored_messages()
                .unwrap(),
            imported
        );
    });
}

#[test]
#[serial_test::serial(real_fs)]
fn legacy_admission_redacts_before_retaining_history() {
    fixture(|config, launch| {
        let secret = "sk-fixture012345678901234567890";
        let session = config
            .adopt_conversation(
                launch,
                "legacy",
                &[json!({"role":"assistant","content":secret})],
                json!({}),
            )
            .unwrap();
        let imported = serde_json::to_string(&session.restored_messages().unwrap()).unwrap();
        assert!(imported.contains("[REDACTED]"));
        assert!(!imported.contains(secret));
        for entry in std::fs::read_dir(config.directory(launch).unwrap()).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                assert!(!std::fs::read(path)
                    .unwrap()
                    .windows(secret.len())
                    .any(|bytes| bytes == secret.as_bytes()));
            }
        }
    });
}

#[test]
#[serial_test::serial(real_fs)]
fn legacy_admission_preserves_isolation_and_current_contract_checks() {
    fixture(|config, launch| {
        let history = [json!({"role":"user","content":"historical task"})];
        let unsafe_config = SmartHarnessConfig {
            frame_dir: Some(launch.workspace.join("visible-frame")),
            ..config.clone()
        };
        assert!(unsafe_config
            .adopt_conversation(launch, "legacy", &history, json!({}))
            .is_err());
        assert!(!launch.workspace.join("visible-frame").exists());
        let session = config
            .adopt_conversation(launch, "legacy", &history, json!({"model":"first"}))
            .unwrap();
        drop(session);
        assert!(config
            .adopt_conversation(launch, "legacy", &history, json!({"model":"changed"}))
            .is_err());
        assert!(config
            .adopt_conversation(
                &HarnessLaunch {
                    hermetic: true,
                    ..*launch
                },
                "legacy",
                &history,
                json!({"model":"first"}),
            )
            .is_err());
    });
}

/// Grounds admission in the actual bounded projection and retrieval path;
/// retaining source bytes alone is insufficient if the model loses their pointer.
#[test]
#[serial_test::serial(real_fs)]
fn legacy_admission_history_remains_retrievable_after_projection_and_restart() {
    fixture(|config, launch| {
        let marker = "HISTORICAL_ARCHIVE_MARKER";
        let mut session = config.adopt_conversation(
            launch,
            "legacy",
            &[json!({"role":"assistant","content":format!("{marker} {}", "old context ".repeat(500))})],
            json!({}),
        ).unwrap();
        let mut messages = session.restored_messages().unwrap();
        messages.push(json!({"role":"user","content":"fresh operator instruction"}));
        let projected = session.project(&messages, 1024).unwrap();
        assert!(serde_json::to_vec(&projected).unwrap().len() <= 1024);
        assert!(!serde_json::to_string(&projected).unwrap().contains(marker));
        assert_eq!(
            projected.last().unwrap()["content"],
            "fresh operator instruction"
        );
        let pointer = projected
            .iter()
            .filter_map(|m| m["content"].as_str())
            .flat_map(str::split_whitespace)
            .find(|word| word.starts_with("bafy"))
            .expect("omitted historical data needs an advertised retrieval pointer")
            .trim_end_matches('.')
            .to_owned();
        session.record_messages(&projected).unwrap();
        drop(session);
        let mut restored = config
            .open_conversation(launch, "legacy", true, json!({}))
            .unwrap();
        let slice = restored.re_read(&pointer, 0, 4096).unwrap();
        assert!(slice["text"].as_str().unwrap().contains(marker));
        let store =
            agent_harness::store::FrameStore::open(config.directory(launch).unwrap()).unwrap();
        let receipt = agent_harness::forensics::inspect_from_store(
            &store,
            slice["retrieval"].as_str().unwrap().parse().unwrap(),
            Default::default(),
        )
        .unwrap()
        .unwrap();
        let retrieved = receipt
            .references
            .iter()
            .filter(|reference| reference.relation == "event")
            .collect::<Vec<_>>();
        assert!(!retrieved.is_empty());
        for reference in retrieved {
            let event = agent_harness::forensics::inspect_from_store(
                &store,
                reference.cid.parse().unwrap(),
                Default::default(),
            )
            .unwrap()
            .unwrap();
            assert_eq!(event.record["origin"], "historical");
        }
    });
}

/// Grounds failed imports in durable locator publication, including a failure
/// after a smaller historical message has already been admitted.
#[test]
#[serial_test::serial(real_fs)]
fn legacy_admission_failure_never_publishes_conversation_locator() {
    fixture(|config, launch| {
        let config = SmartHarnessConfig {
            max_record_bytes: 8192,
            ..config.clone()
        };
        let locator =
            SmartHarnessConfig::conversation_locator(&config.directory(launch).unwrap(), "legacy");
        for history in [
            vec![
                json!({"role":"assistant","content":"small"}),
                json!({"role":"user","content":"x".repeat(16384)}),
            ],
            vec![json!({"role":"assistant","content":null})],
        ] {
            assert!(config
                .adopt_conversation(launch, "legacy", &history, json!({}))
                .is_err());
            assert!(!locator.exists());
        }
        let session = config
            .adopt_conversation(
                launch,
                "legacy",
                &[json!({"role":"user","content":"valid retry"})],
                json!({}),
            )
            .unwrap();
        assert!(locator.exists());
        assert_eq!(session.restored_messages().unwrap().len(), 2);
    });
}
