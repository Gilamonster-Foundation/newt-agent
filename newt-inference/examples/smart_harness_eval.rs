//! Recorded comparison, not a quality gate. See tests/fixtures/README.md.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use content_addressable::{canonical::to_canonical_dagcbor, ContentId, RawContentId};
use newt_core::agentic::smart_harness::parse_verdict;
use newt_core::config::SmartHarnessConfig;
use newt_core::{BackendKind, NudgeClassifier};
use serde::Deserialize;
use serde_json::{json, Value};

const FIXTURES: &str = include_str!("../tests/fixtures/smart_harness_cases.json");

#[derive(Deserialize)]
struct Case {
    case: String,
    expected: String,
    reply: String,
    antecedent: Option<String>,
    task: String,
}

fn addressed_file(path: &Path) -> anyhow::Result<Value> {
    let bytes = std::fs::read(path)?;
    Ok(json!({"bytes":bytes.len(), "cid":RawContentId::from_content(&bytes)}))
}

fn addressed_report(report: Value) -> anyhow::Result<Value> {
    let id = ContentId::from_canonical_bytes_checked(&to_canonical_dagcbor(&report)?)?;
    Ok(json!({"id":id,"report":report}))
}

fn publish_report(source: &str, destination: &str) -> anyhow::Result<()> {
    let envelope: Value = serde_json::from_slice(&std::fs::read(source)?)?;
    let mut report = envelope["report"].clone();
    anyhow::ensure!(report.is_object(), "missing report object");
    let verified = addressed_report(report.clone())?;
    anyhow::ensure!(
        verified["id"] == envelope["id"],
        "source report identity mismatch"
    );
    for pointer in [
        "/configuration/model_path",
        "/auxiliary_manifest/model_path",
    ] {
        if let Some(path) = report
            .pointer_mut(pointer)
            .filter(|value| value.is_string())
        {
            let filename = Path::new(path.as_str().unwrap())
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow::anyhow!("model path has no filename"))?;
            *path = json!(format!("<MODEL_DIR>/{filename}"));
        }
    }
    report["publication"] = json!({
        "source_report":envelope["id"],
        "redaction":"Local model paths replaced with <MODEL_DIR>/<filename>; measurements and asset identities unchanged. Original report retained outside the repository."
    });
    std::fs::write(
        destination,
        serde_json::to_vec_pretty(&addressed_report(report)?)?,
    )?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--publish-report") {
        anyhow::ensure!(
            args.len() == 3,
            "usage: --publish-report ORIGINAL_JSON PUBLICATION_JSON"
        );
        return publish_report(&args[1], &args[2]);
    }
    anyhow::ensure!(
        (1..=5).contains(&args.len()),
        "usage: smart_harness_eval MODEL_GGUF|--baseline-only [REPORT_JSON [TIMEOUT_MS [INSTRUCTION_FILE|- [SYSTEM_FILE]]]]"
    );
    let baseline_only = args[0] == "--baseline-only";
    let mut config = SmartHarnessConfig {
        enabled: true,
        model: Some("qwen2.5-0.5b".into()),
        model_path: (!baseline_only).then(|| args[0].clone().into()),
        ..Default::default()
    };
    // Classification needs one JSON string; this run's override is recorded.
    config.adjudication.max_output_tokens = 16;
    if let Some(timeout) = args.get(2) {
        config.adjudication.timeout_ms = timeout.parse()?;
        config.adjudication.total_timeout_ms = config.adjudication.timeout_ms;
        config.adjudication.validate()?;
    }
    if let Some(instruction) = args.get(3).filter(|path| path.as_str() != "-") {
        config.adjudication.instruction = std::fs::read_to_string(instruction)?;
        config.adjudication.validate()?;
    }
    if let Some(system) = args.get(4) {
        config.adjudication.system_instruction = std::fs::read_to_string(system)?;
        config.adjudication.validate()?;
    }
    let auxiliary = (!baseline_only)
        .then(|| newt_inference::smart_harness::build(&config, "", BackendKind::Embedded))
        .transpose()?;
    let artifacts = if baseline_only {
        Value::Null
    } else {
        let path = Path::new(&args[0]);
        json!({"weights":addressed_file(path)?,
            "tokenizer":addressed_file(&path.with_file_name("tokenizer.json"))?})
    };
    let cases: Vec<Case> = serde_json::from_str(FIXTURES)?;
    let baseline = NudgeClassifier::builtin();
    let mut rows = Vec::new();
    let mut baseline_confusion = BTreeMap::<String, usize>::new();
    let mut auxiliary_confusion = BTreeMap::<String, usize>::new();
    for case in cases {
        let mut session = agent_harness::Session::new(Default::default())?;
        let request = session.record_request(
            json!({"messages":[{"role":"user","content":"evaluation fixture"}]}),
            "openai",
        )?;
        let reply = session.record_reply(request.id, case.reply.as_bytes())?;
        let prompt = config.adjudication.classification_prompt(
            reply,
            &case.reply,
            case.antecedent.as_deref(),
            Some(&case.task),
        );
        let started = Instant::now();
        let old = baseline.classify(&case.reply);
        let baseline_us = started.elapsed().as_micros();
        // This is the old pending-action gate's binary action, not an invented
        // third-class baseline. Retain its actual class and score alongside it.
        let old_action = if old.is_pending_action() {
            "narration"
        } else {
            "answer"
        };
        *baseline_confusion
            .entry(format!("{}->{old_action}", case.expected))
            .or_default() += 1;
        let mut row = json!({"case":case.case, "expected":case.expected,
            "reply":case.reply,"reply_cid":reply,"antecedent":case.antecedent,"task":case.task,
            "prompt_cid":RawContentId::from_content(prompt.as_bytes()),
            "baseline":{"prediction":old_action,"class":format!("{:?}",old.class),
                "score":old.score,"elapsed_us":baseline_us}});
        if let Some(auxiliary) = &auxiliary {
            let started = Instant::now();
            let result = (auxiliary.complete)(prompt).await;
            let (raw, error) = match result {
                Ok(raw) => (Some(raw), None),
                Err(error) => (None, Some(error.to_string())),
            };
            let prediction = raw.as_deref().and_then(parse_verdict).unwrap_or("failure");
            *auxiliary_confusion
                .entry(format!("{}->{prediction}", case.expected))
                .or_default() += 1;
            row["auxiliary"] = json!({"prediction":prediction,"raw":raw,"error":error,
                "elapsed_ms":started.elapsed().as_millis()});
            eprintln!(
                "{}: expected {}, got {prediction}",
                case.case, case.expected
            );
        }
        rows.push(row);
    }
    let metrics = |side: &str| {
        let total = rows.len();
        let correct = rows
            .iter()
            .filter(|row| row[side]["prediction"] == row["expected"])
            .count();
        let failures = rows
            .iter()
            .filter(|row| row[side]["prediction"] == "failure")
            .count();
        json!({"total":total,"correct":correct,"failures":failures,
            "accuracy":correct as f64 / total as f64,
            "failure_rate":failures as f64 / total as f64})
    };
    let summary = json!({"baseline":metrics("baseline"),
        "auxiliary":auxiliary.as_ref().map(|_|metrics("auxiliary"))});
    let report = json!({"schema":1,"fixture_cid":RawContentId::from_content(FIXTURES.as_bytes()),
        "fixture_origin":"curated regression examples; not sampled production traffic",
        "protocol_origin":match args.len() {
            5 => "development system instruction override; reused fixtures",
            4 if args[3] != "-" => "development instruction override; reused fixtures",
            _ => "bundled production instruction"
        },
        "build_source":newt_core::build_info::SOURCE_ID,
        "baseline":"bundled NudgeClassifier, pending-action gate; unknown accepts, no question class",
        "configuration":config,"auxiliary_manifest":auxiliary.map(|a|a.manifest),
        "model_artifacts":artifacts,"debug_build":cfg!(debug_assertions),
        "architecture":std::env::consts::ARCH,
        "ambient_embedded_device":std::env::var("NEWT_EMBEDDED_DEVICE").ok(),
        "recorded_unix_seconds":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        "summary":summary,"baseline_confusion":baseline_confusion,"auxiliary_confusion":auxiliary_confusion,
        "cases":rows});
    let output = serde_json::to_vec_pretty(&addressed_report(report)?)?;
    if let Some(path) = args.get(1) {
        std::fs::write(path, &output)?;
    } else {
        use std::io::Write;
        std::io::stdout().lock().write_all(&output)?;
    }
    Ok(())
}
