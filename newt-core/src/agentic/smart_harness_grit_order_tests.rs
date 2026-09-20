//! #2449: navigation can admit a correction before primary inference does.
use super::*;
use crate::agentic::{
    attempt_capture,
    run_allowance::RunAllowance,
    turn_admission::{CorrectionCause, TurnAdmission, TurnPolicy},
};
use std::sync::atomic::{AtomicBool, Ordering};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

async fn navigation_then_primary(cancel_between: bool) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/aux"))
        .respond_with(|request: &Request| {
            let prompt = std::str::from_utf8(&request.body).unwrap();
            let catalog: Value = serde_json::from_str(prompt.lines().last().unwrap()).unwrap();
            let selected = catalog["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|candidate| {
                    candidate["required"] == true
                        || candidate["pairs"]
                            .as_array()
                            .is_some_and(|pairs| !pairs.is_empty())
                })
                .map(|candidate| candidate["cid"].clone())
                .collect::<Vec<_>>();
            ResponseTemplate::new(200).set_body_json(selected)
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/primary"))
        .respond_with(ResponseTemplate::new(200).set_body_string("done"))
        .mount(&server)
        .await;
    let aux_url = format!("{}/aux", server.uri());
    let h = SmartHarness::new(
        Session::new(Default::default()).unwrap(),
        Arc::new(move |prompt| {
            let url = aux_url.clone();
            Box::pin(async move {
                let reply = reqwest::Client::new().post(url).body(prompt).send().await?;
                Ok((reply.text().await?, None))
            })
        }),
        Default::default(),
    )
    .unwrap();
    let run = RunAllowance::new(3);
    let owner = TurnAdmission::new(
        TurnPolicy {
            tenacity: crate::Tenacity::Resolute,
            budgets: crate::tenacity::TenacityBudgets::default(),
            rounds: crate::tenacity::resolve_tool_round_limit(8, None, None),
            grace_rounds: 0,
        },
        Some(run.clone()),
    );
    owner.observe(Some(crate::ExecOutcome::Failed), false);
    owner
        .propose(CorrectionCause::FailedCheck, true, None)
        .unwrap();
    owner.begin_round();
    let cancel = AtomicBool::new(false);
    let messages = serde_json::json!([
        {"role":"system","content":"preserve instructions"},
        {"role":"user","content":"old source ".repeat(300)},
        {"role":"assistant","content":"old answer"},
        {"role":"user","content":"continue the correction"}
    ]);
    {
        let _binding = h.bind_admission(Some(owner.clone())).unwrap();
        let projected = h
            .project(messages.as_array().unwrap(), 1200, Some(&cancel))
            .await
            .unwrap();
        assert!(serde_json::to_vec(&projected).unwrap().len() <= 1200);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert_eq!(run.remaining(), 2);
        assert_eq!(
            owner.execution_receipt()["grit_used"],
            1,
            "auxiliary work already admitted correction"
        );
        assert_eq!(owner.execution_receipt()["verification_used"], 1);
        let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
        let request = reqwest::Client::new()
            .post(format!("{}/primary", server.uri()))
            .json(&serde_json::json!({"messages":projected}));
        if cancel_between {
            cancel.store(true, Ordering::Relaxed);
        }
        let sent = attempt_capture::send(
            Some(attempt_capture::AttemptScope {
                ledger: &ledger,
                admission: Some(&owner),
                turn: "fixture",
                model: "fixture",
                backend: "fixture",
                cancel: Some(&cancel),
                run_allowance: Some(&run),
            }),
            "primary",
            request,
            "fixture",
        )
        .await;
        if cancel_between {
            assert!(sent.is_err());
            assert!(ledger.lock().unwrap().head().is_none());
        } else {
            let (response, _) = sent.unwrap();
            assert_eq!(response.status().as_u16(), 200);
        }
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            if cancel_between { 1 } else { 2 }
        );
        assert_eq!(run.remaining(), if cancel_between { 2 } else { 1 });
        assert_eq!(
            owner.execution_receipt()["grit_used"],
            1,
            "one continuation, never refund or double charge"
        );
        assert_eq!(owner.rounds_used(), 1);
    }
    assert!(
        h.state().unwrap().admission.is_none(),
        "binding unwinds after either outcome"
    );
}

#[tokio::test]
async fn grit_2449_navigation_admits_correction_before_primary() {
    navigation_then_primary(false).await;
}

#[tokio::test]
async fn grit_2449_cancel_between_navigation_and_primary_keeps_one_debit() {
    navigation_then_primary(true).await;
}
