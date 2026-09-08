//! Real chat-loop grounding for the persona parser and style-prompt unit tests.
//! Only the backend response is simulated; this is not panel or live-model proof.

mod common;

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MODEL: &str = "personality-fixture";
const TURN_MARKERS: [&str; 4] = [
    "PERSONALITY_FIRST_TURN",
    "PERSONALITY_SECOND_TURN",
    "PERSONALITY_UNSTYLED_BEFORE",
    "PERSONALITY_UNSTYLED_AFTER",
];
const PROSE: [&str; 2] = [
    "PERSONA_ONE_PROSE: Explain the answer in a short sentence.",
    "PERSONA_TWO_PROSE: Give the answer clearly and directly.",
];
const LEVELS: [[u8; 5]; 2] = [[0, 20, 40, 60, 80], [100, 80, 60, 40, 20]];
const TRAITS: [(&str, &str); 5] = [
    ("agreeableness", "agreeableness"),
    ("extraversion", "extraversion"),
    ("warmth", "warmth"),
    ("approachability", "approachability"),
    ("prosocial_behavior", "prosocial behavior"),
];

fn final_response(streaming: bool, phase: Option<usize>) -> ResponseTemplate {
    let answer = match phase {
        Some(0) => "One plus one is two.",
        Some(1) => "Two plus two is four.",
        Some(2) => "Three plus three is six.",
        Some(3) => "Four plus four is eight.",
        _ => "Ready.",
    };
    if streaming {
        let frame = json!({"choices": [{"delta": {"content": answer}, "finish_reason": "stop"}]});
        ResponseTemplate::new(200).set_body_raw(
            format!("data: {frame}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        )
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "model": MODEL,
            "choices": [{
                "message": {"role": "assistant", "content": answer},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 32, "completion_tokens": 8}
        }))
    }
}

/// Grounds the pure trait projection in actual named-persona selection and
/// accepted turns. Each current system message must have exactly one fresh
/// style block and the selected persona's original prose, never an old block
/// left in the frozen system or a matching string only in conversation history.
/// Unstyled turns before selection and after clearing have no style overlay.
/// Piped stdin deliberately does not claim visual or panel-key validation.
/// No real inference, model pulls, tool calls, or permission prompts are used;
/// action nudges retain their default behavior for these arithmetic questions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_persona_style_reaches_each_accepted_turn_without_stale_blocks() {
    let root = common::isolated_root();
    let config_dir = root.path().join(".newt");
    let personas = config_dir.join("personas");
    std::fs::create_dir_all(&personas).expect("isolated persona directory");
    for phase in 0..2 {
        let declarations = TRAITS
            .iter()
            .zip(LEVELS[phase])
            .map(|((key, _), level)| format!("{key} = {level}\n"))
            .collect::<String>();
        let document = format!(
            "+++\nrole = \"reviewer\"\ntools = [\"read_file\"]\ncrew = false\n\
             [caveats]\nfs_write = \"none\"\nexec = \"none\"\nnet = \"none\"\n\
             [personality]\n{declarations}+++\n\n{}\n",
            PROSE[phase]
        );
        std::fs::write(personas.join(format!("style-{}.md", phase + 1)), document)
            .expect("disposable named persona");
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": MODEL, "context_length": 131072}]
        })))
        .mount(&server)
        .await;
    let observed = Arc::new(Mutex::new(Vec::<(usize, String)>::new()));
    let capture = Arc::clone(&observed);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("chat request JSON");
            let messages = body["messages"].as_array().expect("chat messages");
            let phase = messages.iter().rev().find_map(|message| {
                if message["role"] != "user" {
                    return None;
                }
                let text = message["content"].as_str()?;
                TURN_MARKERS.iter().position(|marker| text.contains(marker))
            });
            if let Some(phase) = phase {
                let system = messages
                    .iter()
                    .filter(|message| message["role"] == "system")
                    .map(|message| message["content"].as_str().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n");
                capture.lock().expect("capture lock").push((phase, system));
            }
            final_response(body["stream"].as_bool().unwrap_or(false), phase)
        })
        .mount(&server)
        .await;

    let config_path = config_dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_backend = "fixture"
[[backends]]
name = "fixture"
endpoint = "{}"
model = "{MODEL}"
kind = "openai"
api = "chat_completions"
[tui]
max_tool_rounds = 4
workflow_grace_rounds = 0
[tui.permissions]
preset = "read_only"
extra_exec = []
net = []
prompt = false
[memory]
provider = "rolling_window"
context_tokens = 131072
note_nudge_interval = 0
[context]
manager = "append-only"
[context.features]
semantic = false
experiential = false
scheduled = false
"#,
            server.uri()
        ),
    )
    .expect("isolated explicit config");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_newt"));
    common::isolate_loopback_chat(&mut cmd, root.path());
    cmd.arg("--config-dir")
        .arg(&config_dir)
        .arg("--config")
        .arg(&config_path)
        .args([
            "--no-splash",
            "--plain",
            "--ephemeral",
            "--no-agents-file",
            "--no-prompt-for-permissions",
        ])
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("NEWT_NO_MODEL_PULL", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().expect("spawn exact Cargo-built newt");
    let mut input = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let script = format!(
        "What is three plus three? {}\n\
         /persona set style-1 --keep-context\nWhat is one plus one? {}\n\
         /persona set style-2 --keep-context\nWhat is two plus two? {}\n\
         /persona clear\nWhat is four plus four? {}\n/quit\n",
        TURN_MARKERS[2], TURN_MARKERS[0], TURN_MARKERS[1], TURN_MARKERS[3]
    );
    let completed = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::try_join!(
            async move {
                input.write_all(script.as_bytes()).await?;
                drop(input);
                Ok::<(), std::io::Error>(())
            },
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            child.wait()
        )
    })
    .await;
    let status = match completed {
        Ok(Ok(((), _, _, status))) => status,
        failure => {
            // Retain the actual Child across the watchdog: explicitly kill and
            // reap it before reporting failure, rather than abandon a consumed
            // wait_with_output future and rely only on kill_on_drop.
            let killed = tokio::time::timeout(Duration::from_secs(5), child.kill()).await;
            let reaped = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            panic!(
                "chat did not complete: {failure:?}; kill={killed:?}; reap={reaped:?}\n\
                 stdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&err)
            );
        }
    };
    assert!(
        status.success(),
        "chat failed: {status}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );

    let captured = observed.lock().expect("capture lock");
    for phase in 0..2 {
        let systems: Vec<_> = captured
            .iter()
            .filter(|(seen, _)| *seen == phase)
            .map(|(_, system)| system)
            .collect();
        assert!(
            !systems.is_empty(),
            "accepted turn {} never reached the backend",
            phase + 1
        );
        for system in systems {
            assert_eq!(
                system.matches("## Communication style\n").count(),
                1,
                "turn {} needs one current style block",
                phase + 1
            );
            assert_eq!(
                system.matches(PROSE[phase]).count(),
                1,
                "selected persona prose"
            );
            assert!(!system.contains(PROSE[1 - phase]), "stale persona prose");
            for (index, (_, label)) in TRAITS.iter().enumerate() {
                let expected = format!("- {label}: {}/100", LEVELS[phase][index]);
                assert_eq!(
                    system.lines().filter(|line| *line == expected).count(),
                    1,
                    "turn {} needs one {expected}",
                    phase + 1
                );
                let previous = format!("- {label}: {}/100", LEVELS[1 - phase][index]);
                assert!(
                    !system.lines().any(|line| line == previous),
                    "turn {} retained stale value {previous}",
                    phase + 1
                );
            }
        }
    }
    for (phase, marker) in TURN_MARKERS.iter().enumerate().skip(2) {
        let systems: Vec<_> = captured
            .iter()
            .filter(|(seen, _)| *seen == phase)
            .map(|(_, system)| system)
            .collect();
        assert!(
            !systems.is_empty(),
            "unstyled turn {marker} never reached the backend"
        );
        for system in systems {
            assert!(
                !system.contains("## Communication style"),
                "unstyled turn {marker} retained a style block"
            );
            for prose in PROSE {
                assert!(
                    !system.contains(prose),
                    "unstyled turn retained persona prose"
                );
            }
        }
    }
}
