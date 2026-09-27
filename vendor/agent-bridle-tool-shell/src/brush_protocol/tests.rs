//! Hostile-frame and one-output-history proofs for the ABES worker protocol.

use super::*;
use agent_bridle_core::DenialKind;

const BUDGET: usize = 4096;

fn limits() -> StreamLimits {
    stream_limits(BUDGET).expect("within the repository ceiling")
}

/// Build a raw frame with full control over every header field, so a test can
/// forge exactly the malformation it means to.
fn frame(kind: u8, sequence: u64, payload: &[u8]) -> Vec<u8> {
    forged(
        MAGIC,
        VERSION,
        kind,
        sequence,
        payload.len() as u32,
        payload,
    )
}

fn forged(
    magic: [u8; 4],
    version: u8,
    kind: u8,
    sequence: u64,
    declared_len: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&magic);
    out.push(version);
    out.push(kind);
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&declared_len.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn terminal_payload(response: &WorkerResponse) -> Vec<u8> {
    serde_json::to_vec(response).expect("serialize")
}

fn ok_response() -> WorkerResponse {
    WorkerResponse {
        exit_code: 0,
        denials: Vec::new(),
        error: None,
        dropped: DroppedEvidence::default(),
    }
}

fn read(bytes: &[u8]) -> ToolResult<WorkerOutcome> {
    read_stream(bytes, limits(), |_, _| {})
}

fn denial(target: &str) -> Denial {
    Denial {
        kind: DenialKind::Exec,
        target: target.to_string(),
        reason: "outside grant".to_string(),
    }
}

// ── the happy path, and the single output history ───────────────────────────

/// The accumulated live stream IS the transcript. There is no second copy in
/// the terminal that could disagree with what an observer saw.
#[test]
fn the_accumulated_live_stream_is_the_authoritative_transcript() {
    let mut stream = frame(STARTED, 0, &[]);
    stream.extend(frame(STDOUT, 1, b"hello "));
    stream.extend(frame(STDERR, 2, b"warn"));
    stream.extend(frame(STDOUT, 3, b"world"));
    stream.extend(frame(TERMINAL, 4, &terminal_payload(&ok_response())));

    let mut observed_stdout = Vec::new();
    let mut observed_stderr = Vec::new();
    let outcome = read_stream(stream.as_slice(), limits(), |stream, bytes| {
        match stream {
            crate::ShellOutputStream::Stdout => observed_stdout.extend_from_slice(bytes),
            crate::ShellOutputStream::Stderr => observed_stderr.extend_from_slice(bytes),
        };
    })
    .expect("a well-formed stream");

    assert_eq!(outcome.stdout, b"hello world");
    assert_eq!(outcome.stderr, b"warn");
    // The load-bearing equality: what was displayed live and what is returned
    // as final are the same bytes, because they are the same accumulation.
    assert_eq!(outcome.stdout, observed_stdout);
    assert_eq!(outcome.stderr, observed_stderr);
    assert!(outcome.dropped.is_empty());
}

/// The terminal type structurally cannot carry a competing transcript: a
/// payload that tries to smuggle `stdout` back in is refused outright.
#[test]
fn a_terminal_carrying_its_own_stdout_copy_is_refused() {
    let smuggled = serde_json::json!({
        "exit_code": 0,
        "denials": [],
        "error": null,
        "dropped": {"stdout_bytes": 0, "stderr_bytes": 0, "events": 0},
        "stdout": "a different transcript",
    });
    let mut stream = frame(STARTED, 0, &[]);
    stream.extend(frame(STDOUT, 1, b"the real transcript"));
    stream.extend(frame(
        TERMINAL,
        2,
        &serde_json::to_vec(&smuggled).expect("serialize"),
    ));
    let error = read(&stream).expect_err("deny_unknown_fields must refuse a second output copy");
    assert!(
        error.to_string().contains("invalid terminal"),
        "unexpected error: {error}"
    );
}

// ── framing limits ──────────────────────────────────────────────────────────

#[test]
fn an_oversized_declared_frame_is_refused_before_allocating() {
    // Declares a payload past the repository frame ceiling but sends no bytes.
    // The reader must refuse on the declared length, not try to read it.
    let hostile = forged(
        MAGIC,
        VERSION,
        STDOUT,
        0,
        (MAX_FRAME_PAYLOAD + 1) as u32,
        &[],
    );
    let error = read(&hostile).expect_err("oversized declared frame");
    assert!(
        error.to_string().contains("frame payload exceeded its cap"),
        "unexpected error: {error}"
    );
}

#[test]
fn an_oversized_total_stream_is_refused() {
    let limits = limits();
    let mut stream = frame(STARTED, 0, &[]);
    let chunk = vec![b'x'; MAX_EMIT_CHUNK];
    let mut sequence = 1_u64;
    // Well-formed frames, each individually legal, that together exceed the
    // total-stream cap: the per-frame bound alone would not catch this.
    while stream.len() <= limits.max_total {
        stream.extend(frame(STDOUT, sequence, &chunk));
        sequence += 1;
    }
    let error = read(&stream).expect_err("oversized total stream");
    assert!(
        error.to_string().contains("total byte cap"),
        "unexpected error: {error}"
    );
}

#[test]
fn unreasonable_output_limits_are_refused_before_deriving_caps() {
    assert!(stream_limits(0).is_err(), "a zero budget is not a budget");
    assert!(
        stream_limits(MAX_CONFIGURED_OUTPUT + 1).is_err(),
        "an above-ceiling budget must be refused, not clamped"
    );
    assert!(
        stream_limits(usize::MAX).is_err(),
        "a budget whose derived caps would overflow must be refused"
    );
    // The ceiling itself is admissible, and its derived total stays bounded.
    let ok = stream_limits(MAX_CONFIGURED_OUTPUT).expect("the ceiling is admissible");
    assert!(ok.max_total <= MAX_STREAM_BYTES);
    assert_eq!(ok.max_payload, MAX_FRAME_PAYLOAD);
}

// ── sequencing ──────────────────────────────────────────────────────────────

#[test]
fn a_sequence_gap_is_refused() {
    let mut stream = frame(STARTED, 0, &[]);
    stream.extend(frame(STDOUT, 2, b"skipped one"));
    let error = read(&stream).expect_err("sequence gap");
    assert!(
        error.to_string().contains("expected sequence 1"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_duplicate_sequence_is_refused() {
    let mut stream = frame(STARTED, 0, &[]);
    stream.extend(frame(STDOUT, 1, b"first"));
    stream.extend(frame(STDOUT, 1, b"replayed"));
    let error = read(&stream).expect_err("duplicate sequence");
    assert!(
        error.to_string().contains("expected sequence 2"),
        "unexpected error: {error}"
    );
}

// ── lifecycle ordering ──────────────────────────────────────────────────────

#[test]
fn output_before_started_is_refused() {
    let error = read(&frame(STDOUT, 0, b"early")).expect_err("output before started");
    assert!(error.to_string().contains("output arrived before started"));
}

#[test]
fn a_duplicate_started_frame_is_refused() {
    let mut stream = frame(STARTED, 0, &[]);
    stream.extend(frame(STARTED, 1, &[]));
    let error = read(&stream).expect_err("duplicate started");
    assert!(error.to_string().contains("invalid or duplicate started"));
}

#[test]
fn a_denial_before_started_is_refused() {
    let payload = serde_json::to_vec(&denial("rm")).expect("serialize");
    let error = read(&frame(DENIED, 0, &payload)).expect_err("denial before started");
    assert!(error.to_string().contains("denial arrived before started"));
}

/// A terminal may precede `started` ONLY as an explicit worker failure — the
/// pre-authentication path, where no run ever began.
#[test]
fn a_terminal_before_started_is_refused_unless_it_is_an_explicit_failure() {
    let silent = read(&frame(TERMINAL, 0, &terminal_payload(&ok_response())))
        .expect_err("a successful terminal cannot precede started");
    assert!(silent
        .to_string()
        .contains("pre-start terminal must be an explicit worker error"));

    let explicit = WorkerResponse::failure("handshake refused");
    let outcome = read(&frame(TERMINAL, 0, &terminal_payload(&explicit)))
        .expect("an explicit pre-start failure is the one legal case");
    assert_eq!(outcome.response.error.as_deref(), Some("handshake refused"));
    assert!(outcome.stdout.is_empty());
}

#[test]
fn bytes_after_the_terminal_are_refused() {
    let mut stream = frame(STARTED, 0, &[]);
    stream.extend(frame(TERMINAL, 1, &terminal_payload(&ok_response())));
    stream.extend_from_slice(b"trailing garbage");
    let error = read(&stream).expect_err("bytes after terminal");
    assert!(error.to_string().contains("bytes followed the terminal"));
}

#[test]
fn a_stream_that_ends_without_a_terminal_is_refused() {
    let mut stream = frame(STARTED, 0, &[]);
    stream.extend(frame(STDOUT, 1, b"and then nothing"));
    let error = read(&stream).expect_err("no terminal");
    assert!(error
        .to_string()
        .contains("ended before its terminal result"));
}

// ── malformed headers and payloads ──────────────────────────────────────────

#[test]
fn a_bad_magic_or_unknown_version_or_kind_is_refused() {
    let bad_magic = forged(*b"XXXX", VERSION, STARTED, 0, 0, &[]);
    assert!(read(&bad_magic)
        .expect_err("magic")
        .to_string()
        .contains("frame magic mismatch"));

    let bad_version = forged(MAGIC, VERSION + 1, STARTED, 0, 0, &[]);
    assert!(read(&bad_version)
        .expect_err("version")
        .to_string()
        .contains("unsupported frame version"));

    let mut unknown_kind = frame(STARTED, 0, &[]);
    unknown_kind.extend(forged(MAGIC, VERSION, 99, 1, 0, &[]));
    assert!(read(&unknown_kind)
        .expect_err("kind")
        .to_string()
        .contains("unknown frame kind 99"));
}

#[test]
fn a_truncated_header_is_refused() {
    let full = frame(STARTED, 0, &[]);
    // Any prefix shorter than a header, but non-empty: an empty stream is a
    // different (already covered) error.
    for cut in 1..HEADER_LEN {
        let error = read(&full[..cut]).expect_err("truncated header");
        assert!(
            error.to_string().contains("truncated frame header"),
            "cut {cut}: unexpected error: {error}"
        );
    }
}

#[test]
fn a_truncated_payload_is_refused() {
    let mut stream = frame(STARTED, 0, &[]);
    // Declares 64 bytes, supplies 4.
    stream.extend(forged(MAGIC, VERSION, STDOUT, 1, 64, b"tiny"));
    let error = read(&stream).expect_err("truncated payload");
    assert!(
        error.to_string().contains("truncated frame payload"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_malformed_denial_or_terminal_payload_is_refused() {
    let mut stream = frame(STARTED, 0, &[]);
    stream.extend(frame(DENIED, 1, b"{not json"));
    assert!(read(&stream)
        .expect_err("denial payload")
        .to_string()
        .contains("invalid denial"));

    let mut stream = frame(STARTED, 0, &[]);
    stream.extend(frame(TERMINAL, 1, b"{not json"));
    assert!(read(&stream)
        .expect_err("terminal payload")
        .to_string()
        .contains("invalid terminal"));
}

// ── denial agreement ────────────────────────────────────────────────────────

#[test]
fn streamed_denials_must_match_the_terminal_record() {
    let streamed = serde_json::to_vec(&denial("rm")).expect("serialize");
    let mut mismatched = frame(STARTED, 0, &[]);
    mismatched.extend(frame(DENIED, 1, &streamed));
    let response = WorkerResponse {
        denials: vec![denial("curl")],
        ..ok_response()
    };
    mismatched.extend(frame(TERMINAL, 2, &terminal_payload(&response)));
    let error = read(&mismatched).expect_err("denial mismatch");
    assert!(error
        .to_string()
        .contains("streamed denials do not match the terminal result"));

    // The agreeing case is accepted.
    let mut agreeing = frame(STARTED, 0, &[]);
    agreeing.extend(frame(DENIED, 1, &streamed));
    let response = WorkerResponse {
        denials: vec![denial("rm")],
        ..ok_response()
    };
    agreeing.extend(frame(TERMINAL, 2, &terminal_payload(&response)));
    assert_eq!(
        read(&agreeing).expect("agreeing denials").response.denials,
        vec![denial("rm")]
    );
}

// ── truncation accounting ───────────────────────────────────────────────────

/// A worker that streams more than the supervisor's capture budget must leave
/// exact evidence rather than a silently short transcript.
#[test]
fn supervisor_side_truncation_is_counted_exactly() {
    let mut stream = frame(STARTED, 0, &[]);
    let chunk = vec![b'y'; 1024];
    let mut sequence = 1_u64;
    // Stream twice the capture budget.
    for _ in 0..(BUDGET / 1024 * 2) {
        stream.extend(frame(STDOUT, sequence, &chunk));
        sequence += 1;
    }
    stream.extend(frame(TERMINAL, sequence, &terminal_payload(&ok_response())));

    let outcome = read(&stream).expect("a well-formed but over-budget stream");
    assert_eq!(outcome.stdout.len(), BUDGET, "capture is bounded");
    assert_eq!(
        outcome.dropped.stdout_bytes, BUDGET as u64,
        "the omitted bytes must be counted exactly, not flagged"
    );
    assert_eq!(outcome.dropped.stderr_bytes, 0);
    assert!(outcome.dropped.events > 0);
}

/// A `truncated` notice frame is accepted mid-stream and must be well-formed.
#[test]
fn a_truncation_notice_frame_round_trips_and_must_be_well_formed() {
    let evidence = DroppedEvidence {
        stdout_bytes: 10,
        stderr_bytes: 0,
        events: 1,
    };
    let mut stream = frame(STARTED, 0, &[]);
    stream.extend(frame(
        TRUNCATED,
        1,
        &serde_json::to_vec(&evidence).expect("serialize"),
    ));
    stream.extend(frame(TERMINAL, 2, &terminal_payload(&ok_response())));
    assert!(read(&stream).is_ok());

    let mut bad = frame(STARTED, 0, &[]);
    bad.extend(frame(TRUNCATED, 1, b"{not json"));
    assert!(read(&bad)
        .expect_err("malformed notice")
        .to_string()
        .contains("invalid truncation notice"));
}

// ── the emitter's own invariants ────────────────────────────────────────────

#[test]
fn the_emitter_writes_one_started_one_terminal_and_bounded_output() {
    let sink: Vec<u8> = Vec::new();
    let emitter = WorkerEmitter::new(sink, stream_limits(8).expect("limits"));
    emitter.started().expect("started");
    assert!(
        emitter.started().is_err(),
        "started must be refused a second time"
    );
    // 8-byte budget: 4 bytes stream, 6 are dropped and counted.
    emitter.output(crate::ShellOutputStream::Stdout, b"abcd");
    emitter.output(crate::ShellOutputStream::Stdout, b"efghij");
    emitter.terminal(ok_response()).expect("terminal");
    assert!(
        emitter.terminal(ok_response()).is_err(),
        "terminal must be refused a second time"
    );
}

/// The emitter's exact drop accounting reaches the terminal, and the whole
/// stream it produced is accepted by the reader — the two halves agree.
///
/// This is the round-trip that makes the other tests trustworthy: they forge
/// frames by hand, so without this nothing proves the hand-built shape matches
/// what the real emitter writes.
#[test]
fn emitter_output_round_trips_through_the_reader_with_exact_drop_evidence() {
    let sink = SharedSink::default();
    let emitter = WorkerEmitter::new(sink.clone(), stream_limits(8).expect("limits"));
    emitter.started().expect("started");
    // 8-byte budget: 8 bytes stream, 2 are dropped and counted.
    emitter.output(crate::ShellOutputStream::Stdout, b"abcdefghij");
    emitter.output(crate::ShellOutputStream::Stderr, b"err");
    emitter.terminal(ok_response()).expect("terminal");

    let bytes = sink.bytes();
    assert!(!bytes.is_empty(), "the emitter must actually have written");

    let outcome = read_stream(bytes.as_slice(), limits(), |_, _| {})
        .expect("the reader must accept what the emitter writes");
    assert_eq!(outcome.stdout, b"abcdefgh");
    assert_eq!(outcome.stderr, b"err");
    assert_eq!(
        outcome.response.dropped.stdout_bytes, 2,
        "the worker's own omitted-byte count must reach the terminal"
    );
    assert_eq!(outcome.response.dropped.stderr_bytes, 0);
}

/// A bounded pipe capture must not hide omissions from the worker protocol.
#[test]
fn capped_pipe_drain_preserves_worker_drop_accounting() {
    use crate::output_observer::{drain_capped, OutputEmitter};

    let sink = SharedSink::default();
    let emitter = WorkerEmitter::new(sink.clone(), stream_limits(8).expect("limits"));
    emitter.started().expect("started");
    let worker = emitter.clone();
    let output = OutputEmitter::to_worker_channel(std::sync::Arc::new(move |stream, chunk| {
        worker.output(stream, chunk);
    }));
    let (captured, truncated) = drain_capped(
        std::io::Cursor::new(b"abcdefghij"),
        8,
        &output,
        crate::ShellOutputStream::Stdout,
    )
    .expect("drain");
    emitter.terminal(ok_response()).expect("terminal");

    assert_eq!(captured, b"abcdefgh");
    assert!(truncated);
    let bytes = sink.bytes();
    let outcome = read_stream(bytes.as_slice(), limits(), |_, _| {}).expect("bounded result");
    assert_eq!(outcome.stdout, captured);
    assert_eq!(outcome.response.dropped.stdout_bytes, 2);
    assert_eq!(outcome.response.dropped.stderr_bytes, 0);
}

/// A `Vec<u8>` sink the test can read back after the emitter has written to it.
#[derive(Clone, Default)]
struct SharedSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl SharedSink {
    fn bytes(&self) -> Vec<u8> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
