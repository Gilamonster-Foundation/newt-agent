//! #2875 Part 1 / finding 1: the summarizer's `timeout_secs` is an IDLE bound
//! (the gap between reads), not a total deadline, and the summary request
//! STREAMS so the bound really is a gap between frames.
//!
//! Grounds the mocked idle-timeout contract with a loopback HTTP stub driven by
//! paused tokio time. A `stream: true` request gets its frames one `PIECE_GAP`
//! apart, so every gap is under the 60 s bound while the total is over it. A
//! `stream: false` request (the Ollama warm-up) gets one buffered reply after
//! the same total wait. A stalled stream sends one frame, then goes silent with
//! the socket open. No real sleep happens: [`drive`] advances the paused clock
//! by hand, because tokio auto-advances paused time whenever the runtime is
//! idle, and that includes waiting on real loopback socket I/O. Left to itself
//! the 60 s read timer would fire while a frame is still in the kernel.

use super::{summarize_one_model, SummarizerOpts};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PIECE_GAP: Duration = Duration::from_secs(40);

/// One scripted server reply. `frames` are the streamed pieces (NDJSON lines or
/// SSE events); `whole` is the buffered JSON reply for a `stream: false` request.
/// `stall` keeps the socket open silently after the first streamed frame.
struct Reply {
    frames: &'static [&'static str],
    whole: &'static str,
    stall: bool,
}

/// Read one HTTP request with a Content-Length body and parse its JSON body.
async fn read_json_request(sock: &mut TcpStream) -> Option<serde_json::Value> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
    let len: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + len {
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
        }
    }
    serde_json::from_slice(&bytes[header_end..header_end + len]).ok()
}

async fn serve(mut sock: TcpStream, reply: &'static Reply) {
    let Some(body) = read_json_request(&mut sock).await else {
        return;
    };
    if body["stream"] == true {
        let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        if sock.write_all(head).await.is_err() {
            return;
        }
        for (i, frame) in reply.frames.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(PIECE_GAP).await;
            }
            let chunk = format!("{:x}\r\n{frame}\r\n", frame.len());
            if sock.write_all(chunk.as_bytes()).await.is_err() {
                return;
            }
            if i == 0 && reply.stall {
                std::future::pending::<()>().await;
            }
        }
        let _ = sock.write_all(b"0\r\n\r\n").await;
    } else {
        tokio::time::sleep(PIECE_GAP * reply.frames.len() as u32).await;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            reply.whole.len()
        );
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.write_all(reply.whole.as_bytes()).await;
    }
}

/// Serve every connection concurrently with the same scripted reply, so the
/// Ollama warm-up cannot starve the summary request of its own accept.
async fn spawn_stub(reply: &'static Reply) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(serve(sock, reply));
        }
    });
    format!("http://{addr}")
}

static OLLAMA_STREAM: Reply = Reply {
    frames: &[
        "{\"message\":{\"role\":\"assistant\",\"content\":\"ok \"},\"done\":false}\n",
        "{\"message\":{\"role\":\"assistant\",\"content\":\"summary\"},\"done\":false}\n",
        "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"prompt_eval_count\":1,\"eval_count\":1}\n",
    ],
    whole: "{\"message\":{\"role\":\"assistant\",\"content\":\"ok summary\"},\"done\":true}",
    stall: false,
};

static OLLAMA_STALL: Reply = Reply {
    frames: &[
        "{\"message\":{\"role\":\"assistant\",\"content\":\"ok \"},\"done\":false}\n",
        "{\"message\":{\"role\":\"assistant\",\"content\":\"summary\"},\"done\":false}\n",
    ],
    whole: "{\"message\":{\"role\":\"assistant\",\"content\":\"ok summary\"},\"done\":true}",
    stall: true,
};

static OPENAI_STREAM: Reply = Reply {
    frames: &[
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok \"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"summary\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
        "data: [DONE]\n\n",
    ],
    whole: "{\"choices\":[{\"message\":{\"content\":\"ok summary\"}}]}",
    stall: false,
};

static OPENAI_STALL: Reply = Reply {
    frames: &[
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok \"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"summary\"}}]}\n\n",
    ],
    whole: "{\"choices\":[{\"message\":{\"content\":\"ok summary\"}}]}",
    stall: true,
};

/// Yields between clock advances, so the loopback socket can deliver and the
/// client can read a frame before the paused clock moves again.
const SETTLE_YIELDS: usize = 500;

/// Poll `fut` while advancing the paused clock one `PIECE_GAP` at a time.
/// The advancing branch yields rather than parking, so the runtime is never
/// idle and never auto-advances time past a frame still in flight.
async fn drive<F: std::future::Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    loop {
        tokio::select! {
            biased;
            out = &mut fut => return out,
            () = async {
                for _ in 0..SETTLE_YIELDS {
                    tokio::task::yield_now().await;
                }
                tokio::time::advance(PIECE_GAP).await;
            } => {}
        }
    }
}

/// Eleven frames 40 s apart: every gap is under the 60 s idle bound, but the
/// 400 s total is over the 5 x 60 s backstop.
static OLLAMA_DRIP: Reply = Reply {
    frames: &["{\"message\":{\"role\":\"assistant\",\"content\":\"x\"},\"done\":false}\n"; 11],
    whole: "",
    stall: false,
};

fn opts() -> SummarizerOpts {
    SummarizerOpts {
        timeout_secs: 60,
        retries: 0,
        ..Default::default()
    }
}

#[tokio::test(start_paused = true)]
async fn ollama_stream_running_past_timeout_secs_succeeds() {
    let url = spawn_stub(&OLLAMA_STREAM).await;
    let result = drive(summarize_one_model(
        &url,
        "m",
        false,
        "prompt",
        &opts(),
        &None,
    ))
    .await;
    let (summary, _) = result.unwrap_or_else(|e| {
        panic!("an Ollama stream that keeps producing frames was cut off: {e:#}")
    });
    assert_eq!(summary, "ok summary");
}

#[tokio::test(start_paused = true)]
async fn openai_stream_running_past_timeout_secs_succeeds() {
    let url = spawn_stub(&OPENAI_STREAM).await;
    let result = drive(summarize_one_model(
        &url,
        "m",
        true,
        "prompt",
        &opts(),
        &None,
    ))
    .await;
    let (summary, _) = result.unwrap_or_else(|e| {
        panic!("an OpenAI stream that keeps producing frames was cut off: {e:#}")
    });
    assert_eq!(summary, "ok summary");
}

#[tokio::test(start_paused = true)]
async fn ollama_stream_stalled_mid_stream_fails_on_idle_bound() {
    let url = spawn_stub(&OLLAMA_STALL).await;
    let err = drive(summarize_one_model(
        &url,
        "m",
        false,
        "prompt",
        &opts(),
        &None,
    ))
    .await
    .expect_err("a stream silent past the idle bound must fail");
    assert!(
        err.downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout),
        "expected an idle-bound timeout, got: {err:#}"
    );
}

#[tokio::test(start_paused = true)]
async fn openai_stream_stalled_mid_stream_fails_on_idle_bound() {
    let url = spawn_stub(&OPENAI_STALL).await;
    let err = drive(summarize_one_model(
        &url,
        "m",
        true,
        "prompt",
        &opts(),
        &None,
    ))
    .await
    .expect_err("a stream silent past the idle bound must fail");
    assert!(
        err.downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout),
        "expected an idle-bound timeout, got: {err:#}"
    );
}

#[tokio::test(start_paused = true)]
async fn steady_stream_past_five_times_the_bound_hits_the_total_backstop() {
    let url = spawn_stub(&OLLAMA_DRIP).await;
    let err = drive(summarize_one_model(
        &url,
        "m",
        false,
        "prompt",
        &opts(),
        &None,
    ))
    .await
    .expect_err("a stream that never finishes must hit the total bound");
    assert!(
        err.to_string().contains("total bound"),
        "expected the total backstop, got: {err:#}"
    );
}
