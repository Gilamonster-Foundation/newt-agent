//! #2823: real provider loop, production permission gate, authenticated Brush worker.
//! Run explicitly with --ignored; no kernel skip and no ambient network concession.
#[path = "exec_grant_retry_loop/turn.rs"]
mod turn;
use newt_core::{Caveats, PermissionAction, Scope};
use newt_tui::native_permission_test::Session;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn main() {
    if let Some(code) = newt_core::maybe_dispatch() {
        std::process::exit(code);
    }
    if !std::env::args().any(|a| a == "--ignored") {
        println!("exec_grant_retry_loop: explicit native lane; pass --ignored");
        return;
    }
    assert!(
        newt_core::confined_exec::kernel_fs_fence_available(),
        "native kernel fence required"
    );
    newt_core::process_env::remove_var("NEWT_SHELL_ENGINE");
    newt_core::process_env::remove_var("NEWT_DISABLE_OCAP");
    newt_core::process_env::remove_var("NEWT_FULL_ACCESS");
    let journal = tempfile::tempdir().unwrap();
    newt_core::process_env::set_var(
        "NEWT_EVENT_JOURNAL",
        journal.path().join("events").to_str().unwrap(),
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for program in ["bash", "sh"] {
                immediate_retry(program).await;
            }
            for boundary in ["completion", "cancellation", "conversation-switch"] {
                expiry(boundary).await;
            }
        });
    println!("BRUSH_PROVIDER_GRANT_RETRY_CONFIRMED");
}

fn reply(command: Option<&str>, index: usize) -> ResponseTemplate {
    let message = match command {
        Some(cmd) => serde_json::json!({"role":"assistant","content":null,"tool_calls":[{
            "id":format!("call_{index}"),"type":"function","function":{"name":"run_command","arguments":serde_json::json!({"command":cmd}).to_string()}}]}),
        None => serde_json::json!({"role":"assistant","content":"done"}),
    };
    ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices":[{"message":message}],"usage":{"prompt_tokens":100,"completion_tokens":10}}))
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, Caveats) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("script.sh"), "printf 'ran\\n' >> marker\n").unwrap();
    let base = Caveats {
        exec: Scope::none(),
        net: Scope::none(),
        ..newt_core::confined_exec::workspace_confined_caveats(&root)
    };
    (dir, root, base)
}

async fn immediate_retry(program: &str) {
    let command = format!("{program} script.sh");
    let (_dir, root, base) = fixture();
    let marker = root.join("marker");
    let prompts = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let server = MockServer::start().await;
    let observed = calls.clone();
    let prompt_count = prompts.clone();
    let observed_marker = marker.clone();
    Mock::given(method("POST"))
        .respond_with(move |req: &Request| {
            let n = observed.fetch_add(1, Ordering::SeqCst);
            let body: serde_json::Value = req.body_json().unwrap();
            let results: Vec<_> = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "tool")
                .collect();
            match n {
                0 => (),
                1 => {
                    assert!(
                        !observed_marker.exists(),
                        "approval auto-replayed the script"
                    );
                    assert_eq!(prompt_count.load(Ordering::SeqCst), 1);
                    assert!(results.last().unwrap()["content"]
                        .as_str()
                        .unwrap()
                        .contains("granted:"));
                }
                2 | 3 => {
                    assert_eq!(
                        std::fs::read_to_string(&observed_marker).unwrap(),
                        "ran\n",
                        "retry did not execute exactly once"
                    );
                    assert_eq!(
                        prompt_count.load(Ordering::SeqCst),
                        if n == 2 { 1 } else { 2 }
                    );
                }
                _ => panic!("unexpected provider round {n}"),
            }
            reply((n < 3).then_some(command.as_str()), n)
        })
        .mount(&server)
        .await;
    let mut session = Session::default();
    let cancel = AtomicBool::new(false);
    let count = prompts.clone();
    let mut gate = session.turn(base.clone(), "one", &cancel, move || {
        match count.fetch_add(1, Ordering::SeqCst) {
            0 => PermissionAction::AllowOnce,
            1 => PermissionAction::Deny,
            _ => panic!("unexpected prompt"),
        }
    });
    turn::run(
        &server.uri(),
        root.to_str().unwrap(),
        &base,
        &mut gate,
        &cancel,
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(prompts.load(Ordering::SeqCst), 2);
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "ran\n");
}

async fn expiry(boundary: &str) {
    let (_dir, root, base) = fixture();
    let mut session = Session::default();
    let prompts = Arc::new(AtomicUsize::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let stop = cancel.clone();
    let cancelled = boundary == "cancellation";
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            if n == 1 && cancelled {
                stop.store(true, Ordering::SeqCst);
            }
            // An unrelated call must neither spend nor extend the deferred grant.
            reply(
                match n {
                    0 => Some("bash script.sh"),
                    1 if !cancelled => Some("printf unrelated"),
                    _ => None,
                },
                n,
            )
        })
        .mount(&server)
        .await;
    {
        let count = prompts.clone();
        let mut gate = session.turn(base.clone(), "one", &cancel, move || {
            assert_eq!(count.fetch_add(1, Ordering::SeqCst), 0);
            PermissionAction::AllowOnce
        });
        turn::run(
            &server.uri(),
            root.to_str().unwrap(),
            &base,
            &mut gate,
            &cancel,
        )
        .await;
    }
    assert!(!root.join("marker").exists());
    assert_eq!(prompts.load(Ordering::SeqCst), 1);
    cancel.store(false, Ordering::SeqCst);
    server.reset().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            reply((n == 0).then_some("bash script.sh"), n)
        })
        .mount(&server)
        .await;
    let count = prompts.clone();
    let mut gate = session.turn(
        base.clone(),
        if boundary == "conversation-switch" {
            "two"
        } else {
            "one"
        },
        &cancel,
        move || {
            count.fetch_add(1, Ordering::SeqCst);
            PermissionAction::Deny
        },
    );
    turn::run(
        &server.uri(),
        root.to_str().unwrap(),
        &base,
        &mut gate,
        &cancel,
    )
    .await;
    assert_eq!(
        prompts.load(Ordering::SeqCst),
        2,
        "{boundary}: unused approval survived"
    );
    assert!(
        !root.join("marker").exists(),
        "{boundary}: stale authority executed"
    );
    println!("expiry control passed: {boundary}");
}
