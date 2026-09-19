//! #2451: the binary publishes the strict policy it actually ran and marks
//! premature task completion incomplete without changing the scoring taxonomy.
use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn resolute_2451_headless_reports_captured_obligation_and_incomplete_status() {
    for level in ["resolute", "relentless"] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(CaptureThenFinish {
                requests: Arc::new(Mutex::new(Vec::new())),
            })
            .mount(&server)
            .await;
        let mut command = common::newt();
        let home = command.home().to_path_buf();
        common::isolate_loopback_chat(&mut *command, &home);
        let workspace = home.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let instruction = home.join("instruction.md");
        let events = home.join("events.jsonl");
        std::fs::write(&instruction, "Complete the task without calling a tool.").unwrap();
        command
            .env("NEWT_SELF_VERIFY", "0")
            .env("NEWT_VERIFY_OUTCOMES", "0")
            .env("NEWT_NUDGE", "0")
            .args(["--tenacity", level])
            .args(["--backend-endpoint", &server.uri()])
            .args(["--backend-model", NEMOTRON_MODEL])
            .args(["--backend-kind", "openai", "--backend-api", "chat"])
            .args(["headless", "--cwd"])
            .arg(&workspace)
            .arg("--instruction-file")
            .arg(&instruction)
            .arg("--events")
            .arg(&events)
            .args(["--max-rounds", "1"])
            .assert()
            // Verification shortfalls remain completed attempts for scoring;
            // the solve trace, not a transport failure, carries incompleteness.
            .success()
            .stdout(predicates::str::contains(
                "Harness: verification incomplete",
            ));
        let result = solve_result_from(&events);
        assert_eq!(result["status"], "incomplete", "{level}: {result}");
        assert_eq!(result["end_reason"], "Some(VerificationIncomplete)");
        let contract = contract_from(&events);
        assert_eq!(contract["outcome"], "completed");
        let verification = &contract["receipt"]["verification"];
        assert_eq!(verification["mode"], "result_aware");
        assert_eq!(verification["repair_allowance"], 3);
        assert_eq!(verification["required_by_tenacity"], true);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
