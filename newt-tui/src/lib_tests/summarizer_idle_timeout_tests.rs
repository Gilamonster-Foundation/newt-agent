//! #2875 Part 1: the summarizer's `timeout_secs` is an IDLE bound, not a total
//! deadline. A summary that keeps producing output past `timeout_secs` of
//! wall time must still succeed.
//!
//! Grounds the mocked idle-timeout contract with a loopback HTTP stub driven by
//! paused tokio time: the stub sends the reply in pieces 40 s apart. Every gap
//! is under the 60 s idle bound; the total (80 s) is over it. Paused time
//! auto-advances, so no real sleep happens.

use super::{summarize_one_model, SummarizerOpts};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PIECE_GAP: std::time::Duration = std::time::Duration::from_secs(40);

/// Serve every connection with the same chunked reply, streamed in pieces
/// `PIECE_GAP` apart. Connections are served concurrently: the Ollama warm-up
/// request precedes the summary request, and a dropped warm-up socket must not
/// starve the summary request of its own accept.
async fn spawn_slow_stub(pieces: &'static [&'static str]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut req = Vec::new();
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                          Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                for piece in pieces {
                    tokio::time::sleep(PIECE_GAP).await;
                    let frame = format!("{:x}\r\n{piece}\r\n", piece.len());
                    if sock.write_all(frame.as_bytes()).await.is_err() {
                        return;
                    }
                }
                let _ = sock.write_all(b"0\r\n\r\n").await;
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test(start_paused = true)]
async fn summary_still_streaming_past_timeout_secs_succeeds() {
    static PIECES: [&str; 2] = ["{\"message\":{", "\"content\":\"ok summary\"}}"];
    let url = spawn_slow_stub(&PIECES).await;
    let opts = SummarizerOpts {
        timeout_secs: 60,
        retries: 0,
        ..Default::default()
    };
    let result = summarize_one_model(&url, "m", false, "prompt", &opts, &None).await;
    let (summary, _) = result.unwrap_or_else(|e| {
        panic!("a summary that keeps streaming was cut off at a total deadline: {e:#}")
    });
    assert_eq!(summary, "ok summary");
}
