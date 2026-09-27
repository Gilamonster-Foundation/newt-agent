//! A child-side rendezvous proves output reaches the observer before exit.

use std::io::Write;
use std::time::{Duration, Instant};

pub(super) fn output_probe() -> i32 {
    println!("LIVE_OUTPUT");
    std::io::stdout().flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::path::Path::new("observer-permit").exists() {
        if Instant::now() >= deadline {
            eprintln!("observer did not release the live child");
            return 47;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::io::stdout().write_all(&vec![b'o'; 80 * 1024]).unwrap();
    std::io::stderr().write_all(&vec![b'e'; 80 * 1024]).unwrap();
    0
}

#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
mod native {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use agent_bridle_core::Tool;
    use agent_bridle_tool_shell::{ShellInvocationId, ShellOutputObserver, ShellOutputStream};

    use super::super::{context, verify, Fixture};

    struct Observer {
        permit: PathBuf,
        stdout: Mutex<Vec<u8>>,
        stderr: Mutex<Vec<u8>>,
        finished: AtomicBool,
    }

    impl ShellOutputObserver for Observer {
        fn on_output(&self, _id: ShellInvocationId, stream: ShellOutputStream, bytes: &[u8]) {
            let sink = match stream {
                ShellOutputStream::Stdout => &self.stdout,
                ShellOutputStream::Stderr => &self.stderr,
            };
            let mut sink = sink.lock().unwrap();
            sink.extend_from_slice(bytes);
            if stream == ShellOutputStream::Stdout && sink.starts_with(b"LIVE_OUTPUT\n") {
                std::fs::write(&self.permit, b"observer received live output").unwrap();
            }
        }

        fn on_finish(&self, _id: ShellInvocationId) {
            self.finished.store(true, Ordering::Release);
        }
    }

    pub(in super::super) async fn observer_is_live_and_matches_bounded_output() {
        let fixture = Fixture::new();
        let observer = Arc::new(Observer {
            permit: fixture.workspace.join("observer-permit"),
            stdout: Mutex::new(Vec::new()),
            stderr: Mutex::new(Vec::new()),
            finished: AtomicBool::new(false),
        });
        let value = fixture
            .tool(true)
            .with_output_observer(observer.clone())
            .invoke(
                fixture.arguments("observed-output"),
                &context(&fixture.caveats()),
            )
            .await
            .unwrap();
        assert_eq!(
            value["exit_code"], 0,
            "live observer must release child: {value}"
        );
        verify(&value, &fixture);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !observer.finished.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("normal completion must finish queued output");

        for (name, sink) in [("stdout", &observer.stdout), ("stderr", &observer.stderr)] {
            let recorded = sink.lock().unwrap();
            let captured = value[name].as_str().unwrap().as_bytes();
            assert_eq!(captured.len(), 64 * 1024, "capture cap: {name}");
            assert_eq!(&*recorded, captured, "observer/envelope parity: {name}");
            assert_eq!(value[format!("{name}_truncated")], true);
        }
    }
}

#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
pub(super) use native::observer_is_live_and_matches_bounded_output;

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
