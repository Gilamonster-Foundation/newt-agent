use super::*;
use crate::agentic::NoMcp;

// --- R1: BATCH-level atomic tool-call validation (invariant #3) ---

/// Run a batch through the gate and DISPATCH (count) only on Ok — the honest
/// invocation-counting model of the real loop's two phases. Returns the number
/// of tools that would run; a rejected batch runs ZERO.
fn dispatched_count(
    calls: &[(Option<&str>, Option<&str>, &serde_json::Value)],
    require_call_id: bool,
) -> usize {
    match validate_tool_call_batch(calls, require_call_id) {
        Ok(validated) => validated.len(), // phase 2 executes each; count == invocations
        Err(_) => 0,                      // phase 1 rejected → zero executes
    }
}

#[test]
fn batch_valid_then_malformed_dispatches_zero() {
    let a = serde_json::json!("{\"op\":\"status\"}");
    let bad = serde_json::json!("{\"op\": "); // truncated JSON
    let calls = [
        (Some("id1"), Some("git"), &a),
        (Some("id2"), Some("write_file"), &bad),
    ];
    assert_eq!(
        dispatched_count(&calls, true),
        0,
        "a malformed sibling rejects the whole batch — the valid mutating call must NOT run first"
    );
}

#[test]
fn batch_malformed_then_valid_dispatches_zero() {
    let bad = serde_json::json!("not json");
    let b = serde_json::json!("{}");
    let calls = [
        (Some("id1"), Some("git"), &bad),
        (Some("id2"), Some("list_dir"), &b),
    ];
    assert_eq!(dispatched_count(&calls, true), 0);
}

#[test]
fn batch_missing_call_id_dispatches_zero_when_required() {
    let a = serde_json::json!("{}");
    let calls = [(None, Some("git"), &a)];
    assert_eq!(dispatched_count(&calls, true), 0);
    // ...but the id-less Ollama wire (require_call_id=false) accepts it.
    assert_eq!(dispatched_count(&calls, false), 1);
}

#[test]
fn batch_duplicate_call_ids_dispatch_zero() {
    let a = serde_json::json!("{}");
    let calls = [
        (Some("dup"), Some("git"), &a),
        (Some("dup"), Some("list_dir"), &a),
    ];
    assert_eq!(
        dispatched_count(&calls, true),
        0,
        "duplicate ids mis-correlate results — reject the batch"
    );
}

#[test]
fn batch_malformed_argument_json_dispatches_zero() {
    let bad = serde_json::json!("{\"path\": \"a"); // truncated
    let calls = [(Some("id1"), Some("write_file"), &bad)];
    assert_eq!(dispatched_count(&calls, true), 0);
}

#[test]
fn batch_all_valid_dispatches_every_call() {
    let a = serde_json::json!("{\"op\":\"status\"}");
    let b = serde_json::json!(serde_json::json!({"path": "x"})); // object value
    let c = serde_json::Value::Null; // no-arg tool
    let calls = [
        (Some("id1"), Some("git"), &a),
        (Some("id2"), Some("write_file"), &b),
        (Some("id3"), Some("list_dir"), &c),
    ];
    let out = validate_tool_call_batch(&calls, true).expect("all valid");
    assert_eq!(out.len(), 3);
    assert_eq!(
        out.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
        vec!["git", "write_file", "list_dir"]
    );
    assert_eq!(out[0].call_id, "id1");
}

// The rejection CLASS decides recovery: a correlation problem is
// unrecoverable (caller aborts); a content problem is recoverable (caller may
// echo a keyed rejection and re-dispatch).

#[test]
fn batch_missing_id_is_correlation_impossible() {
    let a = serde_json::json!("{}");
    let calls = [(None, Some("git"), &a)];
    assert!(matches!(
        validate_tool_call_batch(&calls, true),
        Err(BatchRejection::CorrelationImpossible(_))
    ));
}

#[test]
fn batch_blank_id_is_correlation_impossible() {
    // #1526 review: a present-but-blank/whitespace id cannot correlate a
    // function_call_output any more than a missing one — reject the batch.
    let a = serde_json::json!("{}");
    let calls = [(Some("   "), Some("git"), &a)];
    assert!(matches!(
        validate_tool_call_batch(&calls, true),
        Err(BatchRejection::CorrelationImpossible(_))
    ));
}

#[test]
fn batch_duplicate_id_is_correlation_impossible() {
    let a = serde_json::json!("{}");
    let calls = [
        (Some("dup"), Some("git"), &a),
        (Some("dup"), Some("list_dir"), &a),
    ];
    assert!(matches!(
        validate_tool_call_batch(&calls, true),
        Err(BatchRejection::CorrelationImpossible(_))
    ));
}

#[test]
fn batch_bad_args_with_valid_ids_is_content_invalid() {
    // ids are present + unique → correlation is fine; the failure is content.
    let bad = serde_json::json!("not json");
    let calls = [(Some("id1"), Some("git"), &bad)];
    assert!(matches!(
        validate_tool_call_batch(&calls, true),
        Err(BatchRejection::ContentInvalid(_))
    ));
}

// --- per-call validator (still used by the batch gate) ---

#[test]
fn validate_accepts_a_string_encoded_object() {
    let (name, args) = validate_tool_call(
        Some("write_file"),
        &serde_json::json!("{\"path\":\"a.txt\"}"),
    )
    .expect("valid");
    assert_eq!(name, "write_file");
    assert_eq!(args["path"], "a.txt");
}

#[test]
fn validate_accepts_an_object_value_directly() {
    let (name, args) =
        validate_tool_call(Some("git"), &serde_json::json!({"op": "status"})).expect("valid");
    assert_eq!(name, "git");
    assert_eq!(args["op"], "status");
}

#[test]
fn validate_treats_absent_or_empty_arguments_as_no_args() {
    // A no-arg tool: null, absent, and "" all mean an empty object — valid.
    for raw in [
        serde_json::Value::Null,
        serde_json::json!(""),
        serde_json::json!("   "),
    ] {
        let (_, args) = validate_tool_call(Some("list_dir"), &raw).expect("valid no-args");
        assert_eq!(args, serde_json::json!({}), "raw={raw:?}");
    }
}

#[test]
fn validate_rejects_unparseable_arguments_instead_of_coercing_to_null() {
    // The core bug this closes: a truncated/garbled args string used to become
    // `null` and execute anyway. It must now be rejected.
    let err = validate_tool_call(Some("write_file"), &serde_json::json!("{\"path\": \"a"))
        .expect_err("truncated JSON must be rejected");
    assert!(err.contains("not valid JSON"), "got: {err}");
    assert!(err.contains("write_file"), "names the tool: {err}");
}

#[test]
fn validate_rejects_non_object_json_arguments() {
    // A JSON scalar or array is not a tool-args object.
    for raw in [serde_json::json!("[1,2,3]"), serde_json::json!("\"bare\"")] {
        let err =
            validate_tool_call(Some("git"), &raw).expect_err("non-object args must be rejected");
        assert!(
            err.contains("must be a JSON object"),
            "raw={raw:?} got: {err}"
        );
    }
    // ...and a live (already-parsed) non-object value is rejected too.
    assert!(validate_tool_call(Some("git"), &serde_json::json!(42)).is_err());
}

#[test]
fn validate_rejects_a_missing_or_blank_name() {
    assert!(validate_tool_call(None, &serde_json::json!({})).is_err());
    assert!(validate_tool_call(Some(""), &serde_json::json!({})).is_err());
    assert!(validate_tool_call(Some("   "), &serde_json::json!({})).is_err());
}

#[test]
fn malformed_calls_are_never_dispatched_invocation_count_is_zero() {
    // The atomic guarantee, as an invocation-counting proof: run a batch of
    // calls through the ONE validation gate, dispatching (incrementing the
    // counter) ONLY on a valid `(name, args)`. Every malformed call must yield
    // ZERO dispatches — no tool is ever invoked on garbage.
    let batch = vec![
        // valid
        serde_json::json!({"name": "git", "arguments": "{\"op\":\"status\"}"}),
        // malformed: truncated JSON args (the historical null-coercion bug)
        serde_json::json!({"name": "write_file", "arguments": "{\"path\": \"a"}),
        // malformed: missing name
        serde_json::json!({"arguments": "{}"}),
        // valid: no-arg tool
        serde_json::json!({"name": "list_dir"}),
        // malformed: non-object args
        serde_json::json!({"name": "git", "arguments": "[1,2]"}),
    ];

    let mut invocations = 0usize;
    let mut dispatched_names = Vec::new();
    for call in &batch {
        match validate_tool_call(call["name"].as_str(), &call["arguments"]) {
            Ok((name, _args)) => {
                // The ONLY path that reaches a tool.
                invocations += 1;
                dispatched_names.push(name);
            }
            Err(_reason) => {
                // Malformed → echoed back, never dispatched. No side effect.
            }
        }
    }

    assert_eq!(
        invocations, 2,
        "exactly the two well-formed calls dispatch; the three malformed ones invoke nothing"
    );
    assert_eq!(dispatched_names, vec!["git", "list_dir"]);
}

#[test]
fn exit_plan_mode_result_appends_mandatory_edit_only_when_tenacity_requires_it() {
    use crate::tenacity::Tenacity;
    // Advisory levels: plain result, no forcing directive.
    for t in [Tenacity::Relaxed, Tenacity::Standard] {
        let out = exit_plan_mode_result(t);
        assert!(out.starts_with("exited the model-entered PLAN PHASE"));
        assert!(
            !out.contains("must be a concrete"),
            "{t} must not force an edit: {out}"
        );
    }
    // Forcing levels: the mandatory-edit directive is appended.
    for t in [Tenacity::Insistent, Tenacity::Relentless] {
        let out = exit_plan_mode_result(t);
        assert!(out.contains("now EXECUTE it"), "{t}: {out}");
        assert!(out.contains("must be a concrete"), "{t}: {out}");
        assert!(out.contains("edit_file or write_file"), "{t}: {out}");
    }
    // The two sets agree with the level's own predicate.
    for t in Tenacity::all() {
        assert_eq!(
            exit_plan_mode_result(t).contains("now EXECUTE it"),
            t.exit_plan_requires_edit()
        );
    }
}

// ── #1258: the embedded `find` size column (pure, fs-free) ──────────────

/// A `FindOpts` for the finalize/parse tests: defaults except the fields a
/// test overrides.
fn find_opts(max_results: usize, show_size: bool, sort: FindSort) -> FindOpts<'static> {
    FindOpts {
        name: None,
        type_filter: FindType::Any,
        category: FindCategory::Any,
        language: None,
        max_depth: None,
        max_results,
        respect_gitignore: true,
        case_sensitive: true,
        show_size,
        show_lines: false,
        sort,
    }
}

#[test]
fn find_opts_parses_size_column_options() {
    let sized = serde_json::json!({ "show_size": true, "sort": "size" });
    let opts = find_opts_from_args(&sized);
    assert!(opts.show_size);
    assert_eq!(opts.sort, FindSort::Size);
    // Defaults: no size column, name order.
    let empty = serde_json::json!({});
    let d = find_opts_from_args(&empty);
    assert!(!d.show_size);
    assert_eq!(d.sort, FindSort::Name);
    // An unknown sort value falls back to name (never errors).
    let bogus = serde_json::json!({ "sort": "bogus" });
    let bad = find_opts_from_args(&bogus);
    assert_eq!(bad.sort, FindSort::Name);
}

#[test]
fn find_opts_parses_line_count_options() {
    // #1387: line count is a first-class evidence measure, parsed like size.
    let lined = serde_json::json!({ "show_lines": true, "sort": "lines" });
    let opts = find_opts_from_args(&lined);
    assert!(opts.show_lines);
    assert_eq!(opts.sort, FindSort::Lines);
    // Default: no line column.
    let empty = serde_json::json!({});
    assert!(!find_opts_from_args(&empty).show_lines);
}

#[test]
fn find_opts_parse_harness_source_category_and_language() {
    let source = serde_json::json!({ "category": "source", "language": "C++" });
    let opts = find_opts_from_args(&source);

    assert_eq!(opts.category, FindCategory::Source);
    assert_eq!(opts.language, Some("C++"));
    let empty = serde_json::json!({});
    let defaults = find_opts_from_args(&empty);
    assert_eq!(defaults.category, FindCategory::Any);
    assert_eq!(defaults.language, None);
}

#[test]
fn finalize_find_line_sort_is_lines_descending_with_show_lines() {
    // The metric column carries line counts in line mode; ordering is
    // descending with a path tie-break — the "files with the most lines"
    // answer, no `wc -l`.
    let entries = vec![
        (12, "short.rs".to_string()),
        (4247, "huge.rs".to_string()),
        (300, "mid.rs".to_string()),
    ];
    let opts = FindOpts {
        show_lines: true,
        sort: FindSort::Lines,
        ..find_opts(1000, false, FindSort::Lines)
    };
    let (lines, _) = finalize_find(entries, &opts);
    assert_eq!(
        lines,
        vec!["4247\thuge.rs", "300\tmid.rs", "12\tshort.rs"],
        "line count descending, each line prefixed '<lines>\\t<path>'"
    );
}

#[test]
fn count_newlines_matches_wc_l_semantics() {
    // Newlines are counted (a trailing line without a newline is not),
    // mirroring `wc -l` — verified purely over bytes, no filesystem.
    assert_eq!(count_newlines(b"a\nb\nc\n"), 3);
    assert_eq!(
        count_newlines(b"a\nb"),
        1,
        "trailing partial line uncounted"
    );
    assert_eq!(count_newlines(b""), 0);
    assert_eq!(count_newlines(b"no newline at all"), 0);
}

#[test]
fn finalize_find_name_sort_is_paths_ascending() {
    let entries = vec![
        (10, "src/b.rs".to_string()),
        (99, "src/a.rs".to_string()),
        (1, "src/c.rs".to_string()),
    ];
    let (lines, truncated) = finalize_find(entries, &find_opts(1000, false, FindSort::Name));
    assert_eq!(lines, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
    assert!(!truncated, "under the cap");
}

#[test]
fn finalize_find_size_sort_is_bytes_descending_with_show_size() {
    let entries = vec![
        (10, "small.rs".to_string()),
        (900, "big.rs".to_string()),
        (50, "mid.rs".to_string()),
    ];
    let (lines, _) = finalize_find(entries, &find_opts(1000, true, FindSort::Size));
    assert_eq!(
        lines,
        vec!["900\tbig.rs", "50\tmid.rs", "10\tsmall.rs"],
        "byte size descending, each line prefixed '<size>\\t<path>'"
    );
}

#[test]
fn finalize_find_size_ties_break_by_path_for_determinism() {
    let entries = vec![(42, "z.rs".to_string()), (42, "a.rs".to_string())];
    let (lines, _) = finalize_find(entries, &find_opts(1000, false, FindSort::Size));
    assert_eq!(lines, vec!["a.rs", "z.rs"], "equal sizes → path ascending");
}

#[test]
fn finalize_find_size_sort_truncates_to_true_top_n() {
    // The N largest, not the first-N-walked: order THEN truncate.
    let entries = vec![
        (1, "a".to_string()),
        (100, "b".to_string()),
        (50, "c".to_string()),
        (200, "d".to_string()),
    ];
    let (lines, truncated) = finalize_find(entries, &find_opts(2, true, FindSort::Size));
    assert_eq!(lines, vec!["200\td", "100\tb"]);
    assert!(truncated, "two matches dropped past the cap");
}

#[test]
fn finalize_find_dedups_by_path() {
    let entries = vec![
        (10, "dup.rs".to_string()),
        (10, "dup.rs".to_string()),
        (20, "other.rs".to_string()),
    ];
    let (lines, _) = finalize_find(entries, &find_opts(1000, false, FindSort::Name));
    assert_eq!(lines, vec!["dup.rs", "other.rs"]);
}

#[derive(Default)]
pub(super) struct RecordingLiveOutput {
    pub(in crate::agentic::tools) events: std::sync::Mutex<Vec<String>>,
}

impl crate::agentic::LiveToolOutput for RecordingLiveOutput {
    fn start(&self, _generation: u64) {
        self.events.lock().unwrap().push("start".into());
    }

    fn write(&self, _generation: u64, stream: crate::agentic::ToolOutputStream, chunk: &[u8]) {
        self.events
            .lock()
            .unwrap()
            .push(format!("{stream:?}:{}", String::from_utf8_lossy(chunk)));
    }

    fn finish(&self, _generation: u64) {
        self.events.lock().unwrap().push("finish".into());
    }

    fn abandon(&self, _generation: u64) {
        self.events.lock().unwrap().push("abandon".into());
    }
}

/// #1264: the `find` live-stream protocol — the dispatch arm's per-hit
/// producer (each discovery framed as one `line\n` chunk through the
/// relay) emits start → incremental writes in DISCOVERY order → finish.
/// Fully mocked (no fs): the walk's `on_hit` seam is driven directly with
/// the same closure shape the arm wires.
#[test]
fn find_live_stream_emits_start_incremental_writes_then_finish() {
    let sink = std::sync::Arc::new(RecordingLiveOutput::default());
    let mut session = LiveOutputSession::start(Some(sink.clone())).expect("live session");
    let relay = session.relay();
    let on_hit = |line: &str| {
        let mut chunk = line.as_bytes().to_vec();
        chunk.push(b'\n');
        relay.write(crate::agentic::ToolOutputStream::Stdout, &chunk);
    };
    // Discovery order (deliberately not sorted — the live frame shows the
    // walk as it happens; the canonical listing is ordered separately).
    for hit in ["src/b.rs", "src/a.rs", "src/c.rs"] {
        on_hit(hit);
    }
    session.finish();
    assert_eq!(
        *sink.events.lock().unwrap(),
        [
            "start",
            "Stdout:src/b.rs\n",
            "Stdout:src/a.rs\n",
            "Stdout:src/c.rs\n",
            "finish"
        ]
    );
}

/// #1264: after finish, a straggler hit must never reopen the frame — the
/// abandon/no-reopen contract the arm relies on when the walk outruns the
/// presentation worker.
#[test]
fn find_live_stream_hit_after_finish_is_dropped() {
    let sink = std::sync::Arc::new(RecordingLiveOutput::default());
    let mut session = LiveOutputSession::start(Some(sink.clone())).expect("live session");
    let relay = session.relay();
    relay.write(crate::agentic::ToolOutputStream::Stdout, b"early.rs\n");
    session.finish();
    relay.write(crate::agentic::ToolOutputStream::Stdout, b"late.rs\n");
    assert_eq!(
        *sink.events.lock().unwrap(),
        ["start", "Stdout:early.rs\n", "finish"],
        "a post-finish hit must be a no-op"
    );
}

#[test]
fn dropping_live_output_session_finishes_before_returning() {
    struct FinishSignal {
        finished: std::sync::mpsc::Sender<()>,
        abandoned: std::sync::mpsc::Sender<()>,
    }
    impl crate::agentic::LiveToolOutput for FinishSignal {
        fn start(&self, _generation: u64) {}
        fn write(
            &self,
            _generation: u64,
            _stream: crate::agentic::ToolOutputStream,
            _chunk: &[u8],
        ) {
        }
        fn finish(&self, _generation: u64) {
            let _ = self.finished.send(());
        }
        fn abandon(&self, _generation: u64) {
            let _ = self.abandoned.send(());
        }
    }

    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let (abandoned_tx, abandoned_rx) = std::sync::mpsc::channel();
    let session = LiveOutputSession::start(Some(std::sync::Arc::new(FinishSignal {
        finished: finished_tx,
        abandoned: abandoned_tx,
    })))
    .unwrap();
    drop(session);

    finished_rx
        .try_recv()
        .expect("drop closed the live frame synchronously");
    assert!(
        abandoned_rx.try_recv().is_err(),
        "a responsive sink should finish rather than be abandoned"
    );
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_live_sink_cannot_delay_host_timeout() {
    struct BlockingOutput {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl crate::agentic::LiveToolOutput for BlockingOutput {
        fn start(&self, _generation: u64) {}
        fn write(
            &self,
            _generation: u64,
            _stream: crate::agentic::ToolOutputStream,
            _chunk: &[u8],
        ) {
            let _ = self.entered.send(());
            let _ = self.release.lock().unwrap().recv();
        }
        fn finish(&self, _generation: u64) {}
        fn abandon(&self, _generation: u64) {}
    }

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let sink = std::sync::Arc::new(BlockingOutput {
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    });
    let mut session = LiveOutputSession::start(Some(sink)).unwrap();
    let relay = session.relay();
    let run = tokio::spawn(async move {
        host_shell_output_with_timeout(
            "printf ready; sleep 5",
            ".",
            Some(relay),
            std::time::Duration::from_millis(100),
        )
        .await
    });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(std::time::Duration::from_secs(1)))
        .await
        .unwrap()
        .expect("renderer entered its blocking write");

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), run).await;
    if outcome.is_err() {
        let _ = release_tx.send(());
        panic!("blocked presentation defeated the host timeout");
    }
    let run = outcome.unwrap().unwrap().unwrap();
    assert!(run.timed_out);
    assert_eq!(run.exit_code, 124);

    session.cancel();
    release_tx.send(()).unwrap();
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_live_sink_cannot_backpressure_host_pipe_capture() {
    struct BlockingOutput {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl crate::agentic::LiveToolOutput for BlockingOutput {
        fn start(&self, _generation: u64) {}
        fn write(
            &self,
            _generation: u64,
            _stream: crate::agentic::ToolOutputStream,
            _chunk: &[u8],
        ) {
            let _ = self.entered.send(());
            let _ = self.release.lock().unwrap().recv();
        }
        fn finish(&self, _generation: u64) {}
        fn abandon(&self, _generation: u64) {}
    }

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let sink = std::sync::Arc::new(BlockingOutput {
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    });
    let mut session = LiveOutputSession::start(Some(sink)).unwrap();
    let relay = session.relay();
    let run = tokio::spawn(async move {
        host_shell_output_with_timeout(
            "head -c 262144 /dev/zero",
            ".",
            Some(relay),
            std::time::Duration::from_secs(5),
        )
        .await
    });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(std::time::Duration::from_secs(1)))
        .await
        .unwrap()
        .expect("renderer entered its blocking write");

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), run).await;
    if outcome.is_err() {
        let _ = release_tx.send(());
        panic!("blocked presentation backpressured host pipe capture");
    }
    let run = outcome.unwrap().unwrap().unwrap();
    assert!(!run.timed_out);
    assert_eq!(run.exit_code, 0);
    assert_eq!(run.stdout.len(), 262_144);

    session.cancel();
    release_tx.send(()).unwrap();
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn host_bypass_publishes_output_before_command_completion() {
    struct ChannelOutput(std::sync::mpsc::Sender<Vec<u8>>);
    impl crate::agentic::LiveToolOutput for ChannelOutput {
        fn start(&self, _generation: u64) {}
        fn write(&self, _generation: u64, _stream: crate::agentic::ToolOutputStream, chunk: &[u8]) {
            let _ = self.0.send(chunk.to_vec());
        }
        fn finish(&self, _generation: u64) {}
        fn abandon(&self, _generation: u64) {}
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let sink = std::sync::Arc::new(ChannelOutput(tx));
    let session = LiveOutputSession::start(Some(sink)).unwrap();
    let relay = session.relay();
    let handle = tokio::spawn(async move {
        host_shell_output("printf ready; sleep 0.2; printf done", ".", Some(relay)).await
    });

    let first =
        tokio::task::spawn_blocking(move || rx.recv_timeout(std::time::Duration::from_secs(2)))
            .await
            .unwrap()
            .expect("first live host-shell chunk");
    assert_eq!(first, b"ready");
    assert!(
        !handle.is_finished(),
        "command completed before its live chunk"
    );

    let run = handle.await.unwrap().unwrap();
    assert_eq!(run.stdout, b"readydone");
    assert_eq!(run.exit_code, 0);
}

#[cfg(not(windows))]
#[tokio::test]
async fn bridled_shell_forwards_live_bytes_without_changing_the_envelope() {
    let sink = std::sync::Arc::new(RecordingLiveOutput::default());
    let caveats = crate::caveats::Caveats {
        exec: crate::caveats::Scope::only(["echo".to_string()]),
        ..crate::caveats::Caveats::top()
    };

    let envelope = dispatch_bridled_shell(
        serde_json::json!({"cmd": "echo observed", "cwd": "."}),
        &caveats,
        Some(sink.clone()),
    )
    .await
    .expect("confined echo dispatch");

    assert_eq!(envelope["stdout"], "observed\n");
    assert_eq!(
        *sink.events.lock().unwrap(),
        ["start", "Stdout:observed\n", "finish"]
    );
}

/// b1 slice 2 — the LIVE attacker-exec path proof: `run_command` →
/// `dispatch_bridled_shell` → agent-bridle `ShellTool` (child_network =
/// `DenyDirect`, 0.7.15) → spawn. Under `net: none` the seccomp egress floor
/// denies the child's AF_INET socket, so a hostile `run_command` has NO
/// off-box socket of any protocol — the shell path finally inheriting the
/// same complete egress floor as the `ConstrainedExecutor` callers. Skips
/// where Landlock / python3 are unavailable (there the confined spawn fails
/// closed — nothing runs unconfined). Real-resource; grounds the mocked
/// bridle-policy wiring in `bridle_registry`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn run_command_child_under_net_none_cannot_open_a_socket_b1() {
    if !crate::confined_exec::kernel_fs_fence_available()
        || !std::path::Path::new("/usr/bin/python3").exists()
    {
        return;
    }
    let caveats = crate::caveats::Caveats {
        exec: crate::caveats::Scope::only(["python3".to_string()]),
        net: crate::caveats::Scope::none(),
        ..crate::caveats::Caveats::top()
    };
    let envelope = dispatch_bridled_shell(
            serde_json::json!({
                "cmd": r#"python3 -c "import socket; socket.socket(socket.AF_INET, socket.SOCK_DGRAM)""#,
                "cwd": "."
            }),
            &caveats,
            None,
        )
        .await
        .expect("dispatch");
    // The confined path must actually have been taken (else the floor never ran).
    assert_eq!(
        envelope["sandbox_kind"], "landlock",
        "run_command child must be kernel-confined: {envelope}"
    );
    // AF_INET socket creation is seccomp-denied → PermissionError → exit 1.
    // (A net-GRANTED run_command leaves DenyDirect inert; this proves the
    // net:none case is fully fenced — no direct egress of any protocol.)
    assert_ne!(
        envelope["exit_code"], 0,
        "run_command under net:none must deny the child's AF_INET socket (b1): {envelope}"
    );
}

/// Closure-proof: the run_command route allows AF_UNIX (deliberately) and
/// does NOT fence an abstract-namespace `connect()`, so a confined child CAN
/// reach a host abstract-namespace unix-domain deputy — the local-deputy
/// egress path the direct-socket seccomp floor does not close. Pinned so a
/// future fence (netns) that closes it forces the register + public claim to
/// be revisited. (Grounds the honest narrowing: "direct AF_INET/INET6/PACKET
/// denied", NOT "no network egress".)
#[cfg(target_os = "linux")]
#[tokio::test]
async fn run_command_child_can_reach_an_af_unix_abstract_deputy() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};
    if !crate::confined_exec::kernel_fs_fence_available()
        || !std::path::Path::new("/usr/bin/python3").exists()
    {
        return;
    }
    let name = format!("newt-rc-afunix-{}", std::process::id());
    let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let _listener = UnixListener::bind_addr(&addr).unwrap();
    let caveats = crate::caveats::Caveats {
        exec: crate::caveats::Scope::only(["python3".to_string()]),
        net: crate::caveats::Scope::none(),
        ..crate::caveats::Caveats::top()
    };
    let cmd = format!(
        r#"python3 -c "import socket; s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.connect('\0{name}'); print('DEPUTY-REACHED'); s.close()""#
    );
    let envelope =
        dispatch_bridled_shell(serde_json::json!({"cmd": cmd, "cwd": "."}), &caveats, None)
            .await
            .expect("dispatch");
    assert_eq!(
        envelope["sandbox_kind"], "landlock",
        "child must be kernel-confined: {envelope}"
    );
    assert!(
        envelope["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("DEPUTY-REACHED"),
        "run_command child reached an AF_UNIX abstract deputy — the ambient-deputy egress \
             residual is REAL on the run_command route (register it; narrow the claim): {envelope}"
    );
}

/// Closure-proof: the run_command route's FD hygiene is CLOEXEC-based (std's
/// default + agent-bridle `set_cloexec`), NOT the explicit `close_range(3,~0)`
/// the `NetGrant::DenyAll` `newt-net-guard` route performs. This pins that a
/// deliberately-NON-CLOEXEC descriptor IS inherited by the run_command child
/// — so the guarantee "a pre-opened network descriptor cannot bypass the
/// socket() filter" holds ONLY because newt opens its real fds via std
/// (CLOEXEC); a non-CLOEXEC fd would cross. Documents the asymmetry honestly.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn run_command_route_fd_hygiene_is_cloexec_based_not_explicit_close() {
    use std::os::fd::AsRawFd;
    if !crate::confined_exec::kernel_fs_fence_available() {
        return;
    }
    // A marker fd with CLOEXEC deliberately CLEARED (the case std never
    // produces, but a raw-libc caller could).
    let marker = std::fs::File::open("/dev/null").expect("open /dev/null");
    let fd = marker.as_raw_fd();
    // SAFETY: fcntl on a valid owned fd; clears the close-on-exec flag.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
    }
    if !std::path::Path::new("/usr/bin/python3").exists() {
        return;
    }
    let caveats = crate::caveats::Caveats {
        exec: crate::caveats::Scope::only(["python3".to_string()]),
        net: crate::caveats::Scope::none(),
        ..crate::caveats::Caveats::top()
    };
    let cmd = format!(
        r#"python3 -c "import os; print('FD-INHERITED' if os.path.exists('/proc/self/fd/{fd}') else 'fd-closed')""#
    );
    let envelope =
        dispatch_bridled_shell(serde_json::json!({"cmd": cmd, "cwd": "."}), &caveats, None)
            .await
            .expect("dispatch");
    drop(marker);
    let stdout = envelope["stdout"].as_str().unwrap_or_default().to_string();
    // Ground truth: a non-CLOEXEC fd crosses into the run_command child (the
    // route does not explicitly close fds ≥ 3). If this ever flips to
    // fd-closed, the route gained explicit fd-closing — a strict improvement;
    // update the doc/register. Skipped-outcome tolerated where the confined
    // spawn could not run.
    if envelope["sandbox_kind"] == "landlock" {
        assert!(
            stdout.contains("FD-INHERITED"),
            "expected a non-CLOEXEC fd to be inherited (run_command FD hygiene is CLOEXEC-based, \
                 not explicit close). If now fd-closed, the route added explicit closing — update \
                 docs + register: {envelope}"
        );
    }
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_find_on_path(exe: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(exe))
            .find(|p| p.is_file())
    })
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_launcher_path() -> Option<std::path::PathBuf> {
    windows_find_on_path("agent-bridle-aclaunch.exe")
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_netprobe_path() -> Option<std::path::PathBuf> {
    windows_find_on_path("ab-netprobe.exe")
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_appcontainer_available() -> bool {
    let Some(launcher) = windows_launcher_path() else {
        if windows_env_truthy("BRIDLE_REQUIRE_APPCONTAINER") {
            panic!("agent-bridle-aclaunch.exe is required but was not found");
        }
        eprintln!("skipping Windows run_command AppContainer proof: launcher not found");
        return false;
    };
    let out = std::process::Command::new(launcher)
        .args([
            "--name",
            &format!("newt-rc-probe-{}", std::process::id()),
            "cmd.exe",
            "/c",
            "exit 0",
        ])
        .current_dir("C:\\Windows")
        .output()
        .expect("spawn agent-bridle-aclaunch");
    if out.status.success() {
        true
    } else if windows_env_truthy("BRIDLE_REQUIRE_APPCONTAINER") {
        panic!(
            "BRIDLE_REQUIRE_APPCONTAINER is set but AppContainer probe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    } else {
        eprintln!(
            "skipping Windows run_command AppContainer proof: probe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        false
    }
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_low_dir(kind: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix(&format!("newt-rc-{kind}-"))
        .tempdir()
        .expect("create temp dir");
    let _ = std::process::Command::new("icacls")
        .arg(dir.path())
        .args(["/setintegritylevel", "(OI)(CI)Low"])
        .output();
    dir
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_grant_all_appcontainers(path: &std::path::Path) {
    for sid in ["*S-1-15-2-1:(OI)(CI)F", "*S-1-15-2-2:(OI)(CI)F"] {
        let out = std::process::Command::new("icacls")
            .arg(path)
            .args(["/grant", sid])
            .output()
            .expect("run icacls grant");
        assert!(
            out.status.success(),
            "failed to grant AppContainer fixture DACL {sid} on {}; stdout={} stderr={}",
            windows_path(path),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_path(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_stage_netprobe() -> Option<(tempfile::TempDir, std::path::PathBuf)> {
    let Some(source) = windows_netprobe_path() else {
        if windows_env_truthy("BRIDLE_REQUIRE_APPCONTAINER") {
            panic!("ab-netprobe.exe is required but was not found");
        }
        eprintln!("skipping Windows run_command net proof: ab-netprobe.exe not found");
        return None;
    };
    let dir = windows_low_dir("netprobe");
    let dest = dir.path().join("ab-netprobe.exe");
    std::fs::copy(&source, &dest).expect("stage ab-netprobe.exe");
    Some((dir, dest))
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_tcp_listener() -> (u16, std::sync::mpsc::Receiver<Vec<u8>>) {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.write_all(b"ok");
                    let mut buf = Vec::new();
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                    let _ = stream.read_to_end(&mut buf);
                    let _ = tx.send(buf);
                    return;
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => return,
            }
        }
    });
    (port, rx)
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_host_netprobe_connects(port: u16) -> bool {
    windows_netprobe_path()
        .and_then(|probe| {
            std::process::Command::new(probe)
                .args(["127.0.0.1", &port.to_string()])
                .output()
                .ok()
        })
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn cmd_set_content(path: &std::path::Path, value: &str) -> serde_json::Value {
    let command = format!("echo {value}>{}", windows_path(path));
    serde_json::json!({
        "program": "cmd.exe",
        "args": ["/d", "/c", command],
    })
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
struct EnvGuard {
    key: &'static str,
    saved: Option<String>,
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let saved = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, saved }
    }
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.saved.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Windows route proof: `run_command`'s private `dispatch_bridled_shell`
/// path engages AppContainer for fs-restricted caveats. A write to a granted
/// directory succeeds; the same shell command against an ungranted sibling is
/// blocked by the AppContainer/ACL boundary.
#[cfg(all(windows, feature = "windows-appcontainer"))]
#[tokio::test]
async fn run_command_windows_appcontainer_allows_granted_write_denies_sibling_write() {
    let _lock = disable_ocap_tests::env_lock().await;
    let _engine = disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    if !windows_appcontainer_available() {
        return;
    }

    let parent = windows_low_dir("siblings");
    let workspace = parent.path().join("workspace");
    let sibling = parent.path().join("sibling");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    let _ = std::process::Command::new("icacls")
        .arg(&workspace)
        .args(["/setintegritylevel", "(OI)(CI)Low"])
        .output();
    windows_grant_all_appcontainers(&workspace);
    let _ = std::process::Command::new("icacls")
        .arg(&sibling)
        .args(["/setintegritylevel", "(OI)(CI)Low"])
        .output();
    let granted = workspace.join("granted.txt");
    let denied = sibling.join("denied.txt");
    std::fs::write(&granted, "ORIG").unwrap();
    std::fs::write(&denied, "ORIG").unwrap();

    let caveats = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only([windows_path(&workspace), windows_path(&sibling)]),
        fs_write: crate::caveats::Scope::only([windows_path(&workspace)]),
        exec: crate::caveats::Scope::All,
        net: crate::caveats::Scope::All,
        max_calls: crate::caveats::CountBound::Unlimited,
        valid_for_generation: crate::caveats::Scope::All,
    };

    let mut granted_args = cmd_set_content(&granted, "GRANTED");
    granted_args["cwd"] = serde_json::Value::String(windows_path(&workspace));
    let ok = dispatch_bridled_shell(granted_args, &caveats, None)
        .await
        .expect("granted run_command dispatch");
    assert_eq!(
        ok["sandbox_kind"], "app_container",
        "run_command must engage AppContainer on Windows: {ok}"
    );
    assert!(
        std::fs::read_to_string(&granted)
            .unwrap_or_default()
            .contains("GRANTED"),
        "granted workspace write should succeed through run_command; file={:?}; envelope={ok}",
        std::fs::read_to_string(&granted).unwrap_or_default()
    );

    let mut denied_args = cmd_set_content(&denied, "DENIED");
    denied_args["cwd"] = serde_json::Value::String(windows_path(&workspace));
    let no = dispatch_bridled_shell(denied_args, &caveats, None)
        .await
        .expect("denied run_command dispatch");
    assert_eq!(
        no["sandbox_kind"], "app_container",
        "denial must still be from the AppContainer route: {no}"
    );
    assert!(
        !std::fs::read_to_string(&denied)
            .unwrap_or_default()
            .contains("DENIED"),
        "sibling write must not escape run_command's AppContainer fence"
    );
}

/// Windows route proof for the network axis: a run_command child can execute
/// a real helper, but AppContainer net:none prevents it from opening a direct
/// loopback TCP connection that the same helper can open on the host.
#[cfg(all(windows, feature = "windows-appcontainer"))]
#[tokio::test]
async fn run_command_windows_appcontainer_denies_direct_tcp() {
    let _lock = disable_ocap_tests::env_lock().await;
    let _engine = disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    if !windows_appcontainer_available() {
        return;
    }
    let Some((probe_dir, probe)) = windows_stage_netprobe() else {
        return;
    };
    let (host_port, host_rx) = windows_tcp_listener();
    assert!(
        windows_host_netprobe_connects(host_port),
        "host netprobe control must connect"
    );
    assert!(
        host_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .is_ok(),
        "host control listener must observe the connection"
    );

    let workspace = windows_low_dir("tcp");
    let (port, rx) = windows_tcp_listener();
    let caveats = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only([
            windows_path(workspace.path()),
            windows_path(probe_dir.path()),
        ]),
        fs_write: crate::caveats::Scope::only([windows_path(workspace.path())]),
        exec: crate::caveats::Scope::All,
        net: crate::caveats::Scope::none(),
        max_calls: crate::caveats::CountBound::Unlimited,
        valid_for_generation: crate::caveats::Scope::All,
    };
    let envelope = dispatch_bridled_shell(
        serde_json::json!({
            "program": windows_path(&probe),
            "args": ["127.0.0.1", port.to_string()],
            "cwd": windows_path(workspace.path()),
        }),
        &caveats,
        None,
    )
    .await
    .expect("run_command tcp dispatch");
    assert_eq!(
        envelope["sandbox_kind"], "app_container",
        "direct TCP denial must run through AppContainer: {envelope}"
    );
    assert!(
        envelope["exit_code"].as_i64().unwrap_or_default() != 0,
        "direct TCP probe should fail under AppContainer net:none: {envelope}"
    );
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(500))
            .is_err(),
        "parent listener must not observe a denied run_command TCP connection"
    );
}

/// Windows route proof for #8 on the `run_command` shell path: current
/// agent-bridle `ShellTool` Windows children still inherit ambient parent
/// environment. This pins the ACTIVE shared dependency defect instead of
/// adding a Newt-only shim around the bridle spawn path.
#[cfg(all(windows, feature = "windows-appcontainer"))]
#[tokio::test]
async fn run_command_windows_provider_env_inheritance_is_active() {
    let _lock = disable_ocap_tests::env_lock().await;
    let _engine = disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _key = EnvGuard::set("OPENAI_API_KEY", "sk-run-command-windows-secret");
    if !windows_appcontainer_available() {
        return;
    }
    let workspace = windows_low_dir("env");
    windows_grant_all_appcontainers(workspace.path());
    let caveats = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only([windows_path(workspace.path())]),
        fs_write: crate::caveats::Scope::only([windows_path(workspace.path())]),
        exec: crate::caveats::Scope::All,
        net: crate::caveats::Scope::All,
        max_calls: crate::caveats::CountBound::Unlimited,
        valid_for_generation: crate::caveats::Scope::All,
    };
    let envelope = dispatch_bridled_shell(
        serde_json::json!({
            "program": "cmd.exe",
            "args": [
                "/d",
                "/c",
                "if defined OPENAI_API_KEY (echo %OPENAI_API_KEY%) else (echo EMPTY)"
            ],
            "cwd": windows_path(workspace.path()),
        }),
        &caveats,
        None,
    )
    .await
    .expect("run_command env dispatch");
    assert_eq!(
        envelope["sandbox_kind"], "app_container",
        "env proof must run through AppContainer: {envelope}"
    );
    assert_eq!(
        envelope["exit_code"], 0,
        "env probe must execute, not pass by failing to spawn: {envelope}"
    );
    let text = envelope["stdout"].as_str().unwrap_or_default();
    assert!(
            text.contains("sk-run-command-windows-secret"),
            "expected to prove the ACTIVE shared bridle Windows env-inheritance residual; stdout was {text:?}. Flip this test to denial when agent-bridle grows Windows env_clear parity."
        );
}

/// Windows missing-backend truth for `run_command`: with AppContainer support
/// compiled in but the launcher hidden from PATH, the shell route refuses
/// before executing the hostile command. This is a Windows-specific contrast
/// to the cross-platform advisory-backend residual documented in the
/// deviation register.
#[cfg(all(windows, feature = "windows-appcontainer"))]
#[tokio::test]
async fn run_command_windows_missing_launcher_refuses_not_host_fallback() {
    let _lock = disable_ocap_tests::env_lock().await;
    let _engine = disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "host");
    let current_exe = std::env::current_exe().expect("current exe");
    if current_exe
        .parent()
        .map(|p| p.join("agent-bridle-aclaunch.exe"))
        .is_some_and(|p| p.exists())
    {
        eprintln!("skipping run_command missing-launcher proof: launcher is next to the test exe");
        return;
    }

    let empty_path = windows_low_dir("empty-path");
    let _path = EnvGuard::set("PATH", &windows_path(empty_path.path()));
    let _path_mixed_case = EnvGuard::set("Path", &windows_path(empty_path.path()));
    let workspace = windows_low_dir("missing");
    let marker = workspace.path().join("fallback.txt");
    std::fs::write(&marker, "ORIG").unwrap();
    let caveats = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only([windows_path(workspace.path())]),
        fs_write: crate::caveats::Scope::only([windows_path(workspace.path())]),
        exec: crate::caveats::Scope::All,
        net: crate::caveats::Scope::All,
        max_calls: crate::caveats::CountBound::Unlimited,
        valid_for_generation: crate::caveats::Scope::All,
    };
    let result = dispatch_bridled_shell(
            serde_json::json!({"cmd": "echo HOST-FALLBACK>fallback.txt", "cwd": windows_path(workspace.path())}),
            &caveats,
            None,
        )
        .await;
    assert!(
            result.is_err(),
            "missing AppContainer launcher should refuse, not return a host/advisory envelope: {result:?}"
        );
    assert!(
        !std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .contains("HOST-FALLBACK"),
        "missing launcher must not run the shell command on the host"
    );
}

// ---- #717: classify_phantom_reach (pure, no fs) ----

#[test]
fn classify_phantom_rewrite_alias() {
    // A shell alias resolves to the canonical run_command rewrite.
    let got = classify_phantom_reach("bash", &serde_json::json!({"command": "ls"}), "ok", true);
    assert_eq!(
        got,
        Some(crate::PhantomResolution::Rewrite("run_command".into()))
    );
}

#[test]
fn classify_phantom_correct_alias() {
    // An edit alias with the wrong arg shape returns Correct guidance.
    let got = classify_phantom_reach(
        "str_replace_editor",
        &serde_json::json!({}),
        "ignored",
        false,
    );
    match got {
        Some(crate::PhantomResolution::Correct(msg)) => {
            assert!(msg.contains("edit_file"), "guidance names the tool: {msg}");
        }
        other => panic!("expected Correct, got {other:?}"),
    }
}

#[test]
fn classify_phantom_unknown_name() {
    // A foreign name with no alias is a true phantom tool. (Note: #716 turned
    // the plan/crew/workflow notions into recognized aliases, so this uses a
    // name no family claims.)
    let got = classify_phantom_reach(
        "summon_kraken",
        &serde_json::json!({}),
        "unknown tool: summon_kraken",
        false,
    );
    assert_eq!(got, Some(crate::PhantomResolution::Unknown));
}

#[test]
fn classify_phantom_plan_alias_is_correct() {
    // #716 + #717: a foreign plan notion now resolves through the alias seam,
    // so the telemetry classifier records it as a Correct (coach) reach — the
    // new arms get phantom-reach telemetry for free.
    let got = classify_phantom_reach("make_plan", &serde_json::json!({}), "ignored", false);
    match got {
        Some(crate::PhantomResolution::Correct(msg)) => {
            assert!(
                msg.contains("update_plan"),
                "guidance names the tool: {msg}"
            );
        }
        other => panic!("expected Correct, got {other:?}"),
    }
}

#[test]
fn classify_phantom_state_get_miss() {
    // state_get on an unset key is an empty-by-design real-tool miss.
    let got = classify_phantom_reach(
        "state_get",
        &serde_json::json!({"key": "nope"}),
        "no such key: nope",
        true,
    );
    assert_eq!(
        got,
        Some(crate::PhantomResolution::RealToolMiss(
            "state_get on an unset key".into()
        ))
    );
}

#[test]
fn classify_phantom_recall_miss() {
    // recall with no hits is an empty-by-design real-tool miss.
    let got = classify_phantom_reach(
        "recall",
        &serde_json::json!({"query": "zzz"}),
        "no matches in past conversations for \"zzz\" — try different keywords",
        true,
    );
    assert_eq!(
        got,
        Some(crate::PhantomResolution::RealToolMiss(
            "recall returned no matches".into()
        ))
    );
}

#[test]
fn classify_phantom_resume_reach_is_a_rewrite() {
    // #714 + #717: a "where were we" reach resolves through the alias seam to
    // a Rewrite, so the telemetry already captures it (no new wiring needed).
    let got = classify_phantom_reach("where_were_we", &serde_json::json!({}), "ignored", false);
    assert_eq!(
        got,
        Some(crate::PhantomResolution::Rewrite("resume_context".into()))
    );
}

#[test]
fn classify_phantom_real_success_is_none() {
    // An ordinary successful real tool call is not phantom telemetry.
    let got = classify_phantom_reach(
        "read_file",
        &serde_json::json!({"path": "src/lib.rs"}),
        "line 1\nline 2\n",
        true,
    );
    assert_eq!(got, None);
}

// ---- #725: tool_search discovery (alias + name registry) ----

#[test]
fn tool_search_is_a_real_tool_name() {
    // It must be in the canonical registry so a model calling it is never
    // treated as a hallucination.
    assert!(ALL_TOOL_NAMES.contains(&"tool_search"));
}

#[test]
fn discovery_verbs_alias_to_tool_search() {
    // The instinctive "which tool does X?" reaches silently Rewrite to the
    // real tool_search.
    for verb in [
        "find_tool",
        "search_tools",
        "list_tools",
        "which_tool",
        "available_tools",
        "what_tools",
        "tools",
    ] {
        match resolve_tool_alias(verb) {
            Some(AliasOutcome::Rewrite(c)) => assert_eq!(c, "tool_search", "verb: {verb}"),
            other => panic!(
                "expected Rewrite(tool_search) for {verb}, got something else: {}",
                other.is_some()
            ),
        }
    }
}

#[test]
fn tool_search_is_not_an_alias_of_itself() {
    // The real name must fall through unchanged (no recursive rewrite).
    assert!(resolve_tool_alias("tool_search").is_none());
}

#[test]
fn classify_phantom_discovery_reach_is_a_rewrite() {
    // #725 + #717: a discovery reach resolves through the alias seam to a
    // Rewrite, so the phantom telemetry captures it for free.
    let got = classify_phantom_reach("find_tool", &serde_json::json!({}), "ignored", false);
    assert_eq!(
        got,
        Some(crate::PhantomResolution::Rewrite("tool_search".into()))
    );
}

#[test]
fn classify_phantom_tool_search_real_call_is_none() {
    // A real tool_search call is not phantom telemetry.
    let got = classify_phantom_reach(
        "tool_search",
        &serde_json::json!({"query": "read"}),
        "Tools matching \"read\":\n- read_file — Read a file",
        true,
    );
    assert_eq!(got, None);
}

#[test]
fn tool_search_is_not_a_hallucination() {
    assert!(!is_hallucination(
        "tool_search",
        &serde_json::json!({"query": "x"})
    ));
}

// ---- #719: read_file payload window/cap/pagination (pure, no fs) ----

#[test]
fn paginate_read_caps_a_large_file_to_the_default_window() {
    // A 15k-line file must NOT flood the model: default window is 2000 lines
    // with a footer to continue (regression for the 12.5k→168k saturation).
    let body: String = (1..=15_057)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let out = paginate_read(&body, None, None, DEFAULT_MAX_OUTPUT_TOKENS);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "line 1");
    assert_eq!(lines[1999], "line 2000");
    assert!(
        !out.contains("line 2001"),
        "window stops at 2000: {:?}",
        &out[..40]
    );
    assert!(out.contains("of 15057"), "footer names the total");
    assert!(
        out.contains("offset=2001"),
        "footer points at the next window"
    );
}

#[test]
fn paginate_read_offset_and_limit_return_just_that_window() {
    let body: String = (1..=100)
        .map(|n| format!("L{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let out = paginate_read(&body, Some(10), Some(5), DEFAULT_MAX_OUTPUT_TOKENS);
    assert!(out.starts_with("L10\nL11\nL12\nL13\nL14"), "{out:?}");
    assert!(out.contains("offset=15"), "continues at line 15: {out:?}");
}

#[test]
fn paginate_read_small_file_is_returned_verbatim_without_a_footer() {
    // Whole-file read that fits both caps → exact bytes, no footer.
    assert_eq!(
        paginate_read("a\nb\nc\n", None, None, DEFAULT_MAX_OUTPUT_TOKENS),
        "a\nb\nc\n"
    );
}

#[test]
fn paginate_read_char_backstop_tracks_the_token_budget() {
    // #726: the char backstop is now token-derived (budget × chars/token),
    // NOT a hardcoded 100k. One enormous line: the line window can't help;
    // the token-derived char backstop must. With a 1000-token budget the
    // backstop is ~4000 chars, so a 50k-char line is truncated near there.
    let budget = 1_000;
    let max_chars = crate::tokens::TokenEstimation::default().chars_for_tokens(budget);
    let body = "x".repeat(50_000);
    let out = paginate_read(&body, None, None, budget);
    assert!(
        out.len() < max_chars + 300,
        "char-capped to the token budget (~{max_chars} chars): {} bytes",
        out.len()
    );
    assert!(out.contains("truncated"), "marks the truncation");
    assert!(
        out.contains("~1000 tokens"),
        "footer names the token budget: {out:?}"
    );

    // A LARGER budget keeps more of the same line — the backstop tracks the
    // budget rather than a fixed constant.
    let wide = paginate_read(&body, None, None, 4_000);
    assert!(
        wide.len() > out.len(),
        "a wider token budget keeps more chars: {} vs {}",
        wide.len(),
        out.len()
    );
}

#[test]
fn paginate_read_zero_budget_disables_the_char_backstop() {
    // #726: max_output_tokens == 0 means "no cap" — only the line window
    // applies, so a single huge line comes back verbatim.
    let body = "y".repeat(500_000);
    let out = paginate_read(&body, None, None, 0);
    assert_eq!(out, body, "zero budget = no char backstop");
}

#[test]
fn paginate_read_offset_past_end_is_a_clear_message() {
    let out = paginate_read("a\nb", Some(99), None, DEFAULT_MAX_OUTPUT_TOKENS);
    assert!(out.contains("past end"), "{out:?}");
}

// ---- #726: shared token-based model-facing output cap ----

#[test]
fn cap_model_output_passes_small_output_through_unchanged() {
    // Well under budget → exact bytes, no marker.
    let small = "hello\nworld\n";
    assert_eq!(cap_model_output(small, DEFAULT_MAX_OUTPUT_TOKENS), small);
}

#[test]
fn cap_model_output_truncates_over_budget_as_head_tail() {
    let big = format!("HEAD_MARKER\n{}\nTAIL_MARKER", "middle\n".repeat(20_000));
    let out = cap_model_output_with_handle(&big, 1_000, 100, None);
    assert!(out.len() < big.len(), "must shrink: {} bytes", out.len());
    assert!(out.contains("HEAD_MARKER"), "head dropped: {out:?}");
    assert!(out.contains("TAIL_MARKER"), "tail dropped: {out:?}");
    assert!(out.contains("head+tail shown"), "marker present: {out:?}");
    assert!(
        !out.contains(&"middle\n".repeat(1_000)),
        "middle should be elided"
    );
}

#[test]
fn cap_model_output_truncates_at_a_char_boundary() {
    // A multi-byte char straddling the cut must not be split — the body must
    // stay valid UTF-8 (no panic, no replacement char).
    let budget = 10; // ~40 chars
    let body = "é".repeat(1_000); // 2 bytes each
    let out = cap_model_output(&body, budget);
    assert!(out.is_char_boundary(out.len()), "valid boundary");
    assert!(
        out.chars()
            .all(|c| c == 'é' || !c.is_control() || c == '\n'),
        "no split char: {out:?}"
    );
}

#[test]
fn cap_model_output_zero_budget_is_no_cap() {
    let body = "z".repeat(500_000);
    assert_eq!(cap_model_output(&body, 0), body);
}

#[test]
fn token_to_char_math_uses_the_default_four_chars_per_token() {
    // The context ESTIMATOR is the default 4 chars/token. NOTE: the output
    // CAP no longer sizes at this ratio — it uses the conservative
    // `output_cap_chars_per_token` (default 3, ~30k chars for a 10k budget)
    // so dense output can't overrun its token budget. See
    // `output_cap_sizes_at_the_conservative_ratio_not_the_estimate`.
    let est = crate::tokens::TokenEstimation::default();
    assert_eq!(est.chars_for_tokens(DEFAULT_MAX_OUTPUT_TOKENS), 40_000);
}

#[test]
fn output_cap_sizes_at_the_conservative_ratio_not_the_estimate() {
    // The conservative cap ratio (default 3) sizes the char backstop, so a
    // 10k-token budget caps at ~30k chars — not the estimator's 40k. This is
    // what keeps dense output (which tokenizes denser than 4 c/t) at/under
    // its real token budget.
    let cap = crate::tokens::TokenEstimation::new(DEFAULT_OUTPUT_CAP_CHARS_PER_TOKEN);
    assert_eq!(cap.chars_for_tokens(DEFAULT_MAX_OUTPUT_TOKENS), 30_000);
    assert!(
        cap.chars_for_tokens(DEFAULT_MAX_OUTPUT_TOKENS)
            < crate::tokens::TokenEstimation::default().chars_for_tokens(DEFAULT_MAX_OUTPUT_TOKENS),
        "cap must be tighter than the estimate"
    );
}

#[test]
fn cap_model_output_caps_dense_body_the_estimate_would_pass() {
    // A body sized between the conservative cap (30k) and the estimator
    // backstop (40k): the old 4-c/t sizing would pass it VERBATIM; the
    // conservative 3-c/t sizing caps it. Relies on the default cap ratio (3)
    // — no global mutation (matches the max_output_tokens test convention).
    let body = "x".repeat(35_000);
    let out = cap_model_output(&body, DEFAULT_MAX_OUTPUT_TOKENS);
    assert!(
        out.len() < body.len(),
        "conservative cap must truncate a 35k-char body at a 10k-token budget \
             (old 4-c/t backstop of 40k would have passed it); got {} bytes",
        out.len()
    );
    assert!(
        out.contains("head+tail shown"),
        "cap marker present: {out:?}"
    );
}

#[test]
fn find_detail_bare_path_has_no_filters() {
    let opts = FindOpts {
        name: None,
        type_filter: FindType::Any,
        category: FindCategory::Any,
        language: None,
        max_depth: None,
        max_results: 1000,
        respect_gitignore: true,
        case_sensitive: true,
        show_size: false,
        show_lines: false,
        sort: FindSort::Name,
    };
    assert_eq!(find_detail(".", &opts), ".");
}

#[test]
fn find_detail_shows_only_non_default_filters() {
    let opts = FindOpts {
        name: Some("*.rs"),
        type_filter: FindType::Files,
        category: FindCategory::Any,
        language: None,
        max_depth: Some(2),
        max_results: 50,
        respect_gitignore: false,
        case_sensitive: false,
        show_size: false,
        show_lines: false,
        sort: FindSort::Name,
    };
    assert_eq!(
        find_detail("src", &opts),
        "src (name=*.rs, type=f, depth=2, max=50, no-gitignore, icase)"
    );
}

#[test]
fn find_detail_omits_each_default_independently() {
    let opts = FindOpts {
        name: None,
        type_filter: FindType::Dirs,
        category: FindCategory::Any,
        language: None,
        max_depth: None,
        max_results: 1000,
        respect_gitignore: true,
        case_sensitive: true,
        show_size: false,
        show_lines: false,
        sort: FindSort::Name,
    };
    assert_eq!(find_detail(".", &opts), ". (type=d)");
}

#[test]
fn find_detail_notes_the_size_column_and_size_sort() {
    let opts = FindOpts {
        name: Some("*.rs"),
        type_filter: FindType::Files,
        category: FindCategory::Any,
        language: None,
        max_depth: None,
        max_results: 10,
        respect_gitignore: true,
        case_sensitive: true,
        show_size: true,
        show_lines: false,
        sort: FindSort::Size,
    };
    assert_eq!(
        find_detail(".", &opts),
        ". (name=*.rs, type=f, max=10, sort=size, size)"
    );
}

#[test]
fn find_detail_notes_the_line_column_and_line_sort() {
    let opts = FindOpts {
        name: Some("*.rs"),
        type_filter: FindType::Files,
        category: FindCategory::Any,
        language: None,
        max_depth: None,
        max_results: 10,
        respect_gitignore: true,
        case_sensitive: true,
        show_size: false,
        show_lines: true,
        sort: FindSort::Lines,
    };
    assert_eq!(
        find_detail(".", &opts),
        ". (name=*.rs, type=f, max=10, sort=lines, lines)"
    );
}

#[test]
fn find_detail_notes_the_source_category_filter() {
    // #1406: the `code:true` boolean was replaced by the language-pack
    // `category=source` filter; find_detail now surfaces that instead.
    let opts = FindOpts {
        name: None,
        type_filter: FindType::Files,
        max_depth: None,
        max_results: 10,
        respect_gitignore: true,
        case_sensitive: true,
        show_size: false,
        show_lines: true,
        category: FindCategory::Source,
        language: None,
        sort: FindSort::Lines,
    };
    assert_eq!(
        find_detail(".", &opts),
        ". (type=f, category=source, max=10, sort=lines, lines)"
    );
}

#[test]
fn use_skill_tool_is_advertised_in_definitions() {
    let defs = tool_definitions();
    let names: Vec<&str> = defs
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert!(names.contains(&"use_skill"), "got: {names:?}");
}

#[test]
fn merged_tool_definitions_with_empty_mcp_is_builtin_set() {
    let merged = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    let names: Vec<&str> = merged
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "run_command",
            "read_file",
            "write_file",
            "edit_file",
            "delete_file",
            "list_dir",
            "find",
            "use_skill",
            "web_fetch",
            // #721: advertised ALWAYS (core capability-grant request, no
            // presence gate) — part of the base tool_definitions() set.
            "request_permissions",
            // #714: advertised ALWAYS (no presence gate), so it joins the
            // base set even with every `with_*` flag off.
            "resume_context",
            // Exact prompt recovery is an invariant, independent of the
            // optional general-memory disclosure surface.
            "prompt_read",
            // Prompt-rooted work recovery is equally invariant and
            // always present, even before any artifact has been written.
            "artifact_read",
            // #725: advertised ALWAYS (a discovery tool must always be
            // present), so it too joins the base set with every flag off.
            "tool_search",
            // #727: advertised ALWAYS (read-only budget self-read, no
            // presence gate), pushed right after resume_context.
            "get_context_remaining",
            // #728: advertised ALWAYS (a model must always be able to ask the
            // human; degrades honestly headless), pushed last.
            "request_user_input",
            // #891: advertised ALWAYS (the model-facing lifecycle surface;
            // degrades honestly with "no command configured"), pushed after
            // request_user_input.
            "lifecycle",
            // #1004: advertised ALWAYS (present-findings surface; needs no
            // injected capability, degrades to raw source when color is
            // off), pushed after lifecycle.
            "render_report",
            // #1285: advertised ALWAYS (a read-only navigation utility like
            // tool_search; degrades honestly when no symbol index is built),
            // pushed after render_report.
            "where_is",
            // #1387 Code Navigator — Always-gated structural/lexical tools
            // (degrade when session indexes are absent).
            "goto_definition",
            "text_search",
            "find_references",
            "find_tests",
            "find_callers",
            "find_callees",
            "find_implementations",
            "find_hierarchy",
            "inspect_type",
            "impact",
        ]
    );
}

/// FR-1 part 2 (#997): a persona's `tools:` allow-list scopes the ADVERTISED
/// catalog — only the named tools survive, PLUS the always-on infra tools the
/// loop can't run without (which no persona may fence off). `None` leaves the
/// catalog whole (the zero-cost path for every non-persona session).
#[test]
fn persona_allow_list_filters_the_advertised_catalog() {
    let full = merged_tool_definitions(
        &NoMcp, true, true, true, true, true, true, true, true, true, true, true, true,
    );
    let name_set = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str().map(str::to_owned))
            .collect()
    };
    // No persona → catalog untouched.
    assert_eq!(
        name_set(&filter_advertised_tools(full.clone(), None)),
        name_set(&full),
        "None must be a no-op"
    );
    // A read-only coach (`tools = ["read_file"]`): read_file survives; the
    // mutating built-ins are dropped; every always-on infra tool still rides.
    let allow = vec!["read_file".to_string()];
    let got = name_set(&filter_advertised_tools(full, Some(&allow)));
    assert!(got.iter().any(|n| n == "read_file"), "granted tool kept");
    for denied in [
        "write_file",
        "edit_file",
        "delete_file",
        "run_command",
        "list_dir",
    ] {
        assert!(
            !got.iter().any(|n| n == denied),
            "{denied} must be filtered out"
        );
    }
    for infra in [
        "resume_context",
        "prompt_read",
        "tool_search",
        "get_context_remaining",
        "request_user_input",
        "lifecycle",
        "select_operating_mode",
    ] {
        assert!(
            got.iter().any(|n| n == infra),
            "{infra} is session infrastructure and must survive any persona"
        );
    }
}

/// FR-1 part 2 (#997): `persona_tool_allowed` is the single predicate behind
/// BOTH the advertise-filter and the executor reject — a tool is callable iff
/// the persona names it OR it is always-on infra — so the set the model sees
/// and the set it may run can never drift apart.
#[test]
fn persona_tool_allowed_admits_named_and_always_on_only() {
    let allow = vec!["read_file".to_string()];
    assert!(persona_tool_allowed("read_file", &allow), "named → allowed");
    assert!(
        persona_tool_allowed("request_user_input", &allow),
        "always-on infra → allowed even when unlisted"
    );
    assert!(
        persona_tool_allowed("select_operating_mode", &allow),
        "presence-gated session control → allowed even when unlisted"
    );
    assert!(
        !persona_tool_allowed("write_file", &allow),
        "unlisted non-infra → denied"
    );
    assert!(
        !persona_tool_allowed("delete_file", &allow),
        "unlisted non-infra → denied"
    );
}

/// Prompt disposition is an independent, fail-closed catalog boundary:
/// non-Act turns retain only explicit read/recovery tools, so a generic MCP
/// name cannot appear merely because its schema was connected to the session.
#[test]
fn prompt_disposition_filters_catalog_and_unknown_names_fail_closed() {
    let defs = serde_json::json!([
        { "type": "function", "function": { "name": "read_file" } },
        { "type": "function", "function": { "name": "write_file" } },
        { "type": "function", "function": { "name": "run_command" } },
        { "type": "function", "function": { "name": "update_plan" } },
        { "type": "function", "function": { "name": "exit_plan_mode" } },
        { "type": "function", "function": { "name": "select_operating_mode" } },
        { "type": "function", "function": { "name": "request_permissions" } },
        { "type": "function", "function": { "name": "incident__read" } },
        { "not": "a callable definition" }
    ]);
    let names = |defs: &serde_json::Value| {
        defs.as_array()
            .unwrap()
            .iter()
            .filter_map(|def| def["function"]["name"].as_str())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };

    let research = filter_tools_for_disposition(defs.clone(), PromptDisposition::Research);
    assert_eq!(names(&research), vec!["read_file", "select_operating_mode"]);
    let plan = filter_tools_for_disposition(defs.clone(), PromptDisposition::Plan);
    assert_eq!(
        names(&plan),
        vec![
            "read_file",
            "update_plan",
            "exit_plan_mode",
            "select_operating_mode"
        ]
    );
    assert!(tool_allowed(PromptDisposition::Explain, "read_file"));
    assert!(tool_allowed(PromptDisposition::Plan, "update_plan"));
    assert!(!tool_allowed(PromptDisposition::Explain, "update_plan"));
    assert!(!tool_allowed(PromptDisposition::Research, "update_plan"));
    assert!(tool_allowed(PromptDisposition::Plan, "exit_plan_mode"));
    assert!(
        !tool_allowed(PromptDisposition::Plan, "web_fetch"),
        "offline Plan must not advertise a tool its caveat always denies"
    );
    assert!(
        tool_allowed(PromptDisposition::Research, "web_fetch"),
        "Research (including Diagnose) may gather remote read-only evidence"
    );
    assert!(tool_allowed(
        PromptDisposition::Research,
        "select_operating_mode"
    ));
    assert!(tool_allowed(
        PromptDisposition::Plan,
        "select_operating_mode"
    ));
    assert!(!tool_allowed(
        PromptDisposition::Ask,
        "select_operating_mode"
    ));
    assert!(!tool_allowed(PromptDisposition::Explain, "write_file"));
    assert!(!tool_allowed(PromptDisposition::Research, "incident__read"));
    assert!(!tool_allowed(PromptDisposition::Ask, "read_file"));
    assert!(tool_allowed(PromptDisposition::Act, "incident__write"));
    // #1258: `find` carries the size column (sort=size/show_size), so an
    // evidence-only turn answers "largest files" through it — pin that it
    // stays in the Explain/Research set (guards against a future move to a
    // gated tool that would re-box the diagnosed session).
    assert!(tool_allowed(PromptDisposition::Explain, "find"));
    assert!(tool_allowed(PromptDisposition::Research, "find"));
    // #1387 / line-count lock-in: Research must also keep `find`, AND the
    // advertised schema must teach `sort=lines` + `show_lines`. Losing either
    // re-opens the double-bind (Research admits find but can't answer lines
    // → model dumps or reaches for `wc -l` → empty/denied).
    let research_catalog = filter_tools_for_disposition(
        merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false, false, false,
            false,
        ),
        PromptDisposition::Research,
    );
    let find_def = research_catalog
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["function"]["name"].as_str() == Some("find"))
        .expect("Research must advertise find");
    let props = &find_def["function"]["parameters"]["properties"];
    assert!(
        props.get("show_lines").is_some(),
        "Research find schema must expose show_lines: {find_def}"
    );
    assert!(
        props.get("code").is_some(),
        "Research find schema must expose code (source-only filter): {find_def}"
    );
    let desc = find_def["function"]["description"].as_str().unwrap_or("");
    assert!(
        desc.contains("category") && desc.contains("source"),
        "find description must teach category=source for source rankings: {desc}"
    );
    // #1406: GFM-table response steering moved out of the tool description
    // into the prompt-intake layer (see prompt_intake.rs
    // `*_steers_*_markdown_table` tests); the description no longer carries it.
    let sort_enum = props["sort"]["enum"]
        .as_array()
        .expect("sort must be an enum");
    assert!(
        sort_enum.iter().any(|v| v.as_str() == Some("lines")),
        "Research find sort enum must include 'lines': {sort_enum:?}"
    );
    assert!(
        props.get("category").is_some() && props.get("language").is_some(),
        "Research find schema must teach the harness source category + language filter: \
             {find_def}"
    );
    assert!(
        find_def["function"]["description"]
            .as_str()
            .is_some_and(|description| {
                description.contains("repository code investigation")
                    && description.contains("source by default")
            }),
        "the tool catalog must reinforce the standing source-first repository policy: \
             {find_def}"
    );
    // #1259: the formal ask-the-human escalation IS admitted in evidence
    // turns — a boxed-in model ends as a question, not penalized narration…
    assert!(tool_allowed(
        PromptDisposition::Explain,
        "request_user_input"
    ));
    assert!(tool_allowed(
        PromptDisposition::Research,
        "request_user_input"
    ));
    // …but the capability-GRANT path stays excluded: an evidence turn must
    // never mint caveats (the #1259 security boundary, pinned).
    assert!(!tool_allowed(
        PromptDisposition::Explain,
        "request_permissions"
    ));
    assert!(!tool_allowed(
        PromptDisposition::Research,
        "request_permissions"
    ));
    assert_eq!(
        filter_tools_for_disposition(
            serde_json::json!({ "not": "a catalog" }),
            PromptDisposition::Research
        ),
        serde_json::json!([]),
        "a non-Act catalog with no enumerable tool names must fail closed"
    );

    // Act is the compatibility/default path: it preserves definitions,
    // including an opaque extension definition the disposition filter cannot
    // classify by name.
    assert_eq!(
        filter_tools_for_disposition(defs.clone(), PromptDisposition::Act),
        defs
    );
}

/// FR-1 part 2 (#997): the executor is the ENFORCEMENT half. Even a
/// hallucinated call the advertise-filter can't intercept is refused BY NAME
/// before any side effect — while a granted tool and the always-on infra
/// pass. Regression for a coach persona whose `tools:` list must be a real
/// boundary, not a cosmetic hint.
#[tokio::test]
async fn executor_refuses_tools_outside_the_persona_allow_list() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = crate::caveats::Caveats::top();
    let allow = vec!["read_file".to_string()];
    // write_file is NOT granted → refused with the persona message, and the
    // file is never written (top caveats would otherwise permit it).
    let target = ws.path().join("blocked.txt");
    let args = serde_json::json!({
        "path": target.to_string_lossy(),
        "content": "should never be written",
    });
    let out = call_offload("write_file", &args, &ws, &caveats, Some(&allow)).await;
    assert!(
        out.contains("not available under the active persona"),
        "expected persona refusal, got: {out}"
    );
    assert!(!target.exists(), "a denied write must not touch the fs");
    // An always-on infra tool rides even though it is unlisted.
    let infra = call_offload(
        "get_context_remaining",
        &serde_json::json!({}),
        &ws,
        &caveats,
        Some(&allow),
    )
    .await;
    assert!(
        !infra.contains("not available under the active persona"),
        "always-on infra must not be refused: {infra}"
    );
}

/// FR-3 (#998): the absolute deny-list is wired into the executor and is
/// GRANT-INDEPENDENT — even with top caveats and NO persona, a `run_command`
/// whose exec target is forbidden (`ssh`) is refused before the shell runs,
/// while an ordinary command is untouched. Guards against the deny module
/// being present but never called.
#[tokio::test]
async fn executor_enforces_the_absolute_deny_list() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = crate::caveats::Caveats::top(); // maximal grant — deny still bites
    let denied = call_offload(
        "run_command",
        &serde_json::json!({ "command": "ssh host 'uptime'" }),
        &ws,
        &caveats,
        None, // no persona — the floor is independent of any grant
    )
    .await;
    assert!(
        denied.contains("absolute deny-list"),
        "ssh must hit the deny-list, got: {denied}"
    );
    // A benign command sails past the deny gate (it reaches normal exec).
    let ok = call_offload(
        "run_command",
        &serde_json::json!({ "command": "echo coaching" }),
        &ws,
        &caveats,
        None,
    )
    .await;
    assert!(
        !ok.contains("absolute deny-list"),
        "an ordinary command must not be denied, got: {ok}"
    );
}

/// Test-only thin wrapper over the 22-arg [`execute_tool_with_offload`] that
/// fixes every optional seam to `None` and surfaces just the persona list.
async fn call_offload(
    name: &str,
    args: &serde_json::Value,
    ws: &tempfile::TempDir,
    caveats: &crate::caveats::Caveats,
    persona_tools: Option<&[String]>,
) -> String {
    execute_tool_with_offload(
        name,
        args,
        &ws.path().to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        None,  // build_check_cmd
        None,  // note_sink
        None,  // recall_source
        None,  // memory_source
        None,  // permission_gate
        None,  // exec_floor
        None,  // git_tool
        None,  // crew_runner
        None,  // scratchpad_store
        None,  // code_search
        None,  // where_is
        None,  // experience_store
        None,  // step_ledger
        false, // tool_offload
        None,  // spill_store
        persona_tools,
    )
    .await
}

/// #894: each registry entry's schema-builder produces the SAME name the
/// entry declares — catches a copy-paste where the `ToolSpec.name` and the
/// `*_tool_definition()` disagree.
#[test]
fn registry_specs_match_their_definition_names() {
    for spec in EXTENDED_TOOL_REGISTRY {
        let def = (spec.definition)();
        assert_eq!(
            def["function"]["name"].as_str(),
            Some(spec.name),
            "ToolSpec name {:?} != definition name",
            spec.name
        );
    }
}

/// #894: no built-in tool name is declared twice across the base array and
/// the registry (a dup would double-advertise and confuse dispatch).
#[test]
fn builtin_tool_names_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for name in ALL_TOOL_NAMES.iter() {
        assert!(seen.insert(*name), "duplicate built-in tool name: {name}");
    }
}

/// #894 anti-drift (the payoff): with EVERY gate on, the advertised set from
/// `merged_tool_definitions` equals `ALL_TOOL_NAMES` in BOTH directions. This
/// is the test that would have caught the `lifecycle` drift — a tool
/// advertised/dispatched but missing from the real-name set (or vice versa)
/// fails here.
#[test]
fn advertised_set_matches_all_tool_names_both_directions() {
    let all = merged_tool_definitions(
        &NoMcp, true, true, true, true, true, true, true, true, true, true, true, true,
    );
    let advertised: std::collections::HashSet<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    let names: std::collections::HashSet<&str> = ALL_TOOL_NAMES.iter().copied().collect();
    // Every advertised tool is a real (non-hallucinated) name...
    for a in &advertised {
        assert!(
            names.contains(a),
            "advertised but not in ALL_TOOL_NAMES: {a}"
        );
    }
    // ...and every real name is actually advertised when its gate is on.
    for n in &names {
        assert!(
            advertised.contains(n),
            "in ALL_TOOL_NAMES but never advertised: {n}"
        );
    }
}

/// #894: `BASE_TOOL_NAMES` mirrors the names inlined in `tool_definitions()`
/// exactly and in order — the one hand-kept mirror, guarded here.
#[test]
fn base_tool_names_match_tool_definitions() {
    let defs = tool_definitions();
    let base: Vec<&str> = defs
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert_eq!(base, BASE_TOOL_NAMES);
}

/// #894 regression for the concrete drift that motivated the registry: the
/// `lifecycle` tool (#891) is advertised + dispatched, so it MUST be a real
/// name — otherwise every legitimate `lifecycle` call is miscounted as a
/// hallucination (inflating the anti-loop counter). Before the registry it
/// was missing from `ALL_TOOL_NAMES`; the derivation makes that impossible.
#[test]
fn lifecycle_is_a_real_tool_name_not_a_hallucination() {
    assert!(
        ALL_TOOL_NAMES.contains(&"lifecycle"),
        "lifecycle must be a real tool name"
    );
    assert!(
        !is_hallucination("lifecycle", &serde_json::json!({"phase": "test"})),
        "a real lifecycle call must not be flagged as a hallucination"
    );
}

#[test]
fn lifecycle_definition_enum_matches_phase_vocabulary() {
    // The schema's phase enum is built from `Phase::ALL`, so it can never
    // drift from the vocabulary the executor parses with `Phase::from_key`.
    let def = lifecycle_tool_definition();
    assert_eq!(def["function"]["name"], "lifecycle");
    let enum_vals: Vec<&str> = def["function"]["parameters"]["properties"]["phase"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let vocab: Vec<&str> = crate::tooling::Phase::ALL
        .iter()
        .map(|p| p.as_str())
        .collect();
    assert_eq!(enum_vals, vocab);
}

#[test]
fn run_phase_aliases_route_to_lifecycle() {
    for a in ["run_phase", "run_lifecycle", "lifecycle_run"] {
        assert!(
            matches!(
                resolve_tool_alias(a),
                Some(AliasOutcome::Rewrite("lifecycle"))
            ),
            "{a} should rewrite to lifecycle"
        );
    }
    // The canonical name is NOT an alias — it dispatches directly.
    assert!(resolve_tool_alias("lifecycle").is_none());
}

#[tokio::test]
async fn lifecycle_unknown_phase_lists_valid_phases() {
    // An unknown phase returns before any fs/subprocess touch, so this is a
    // fully-mocked unit test.
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "phase": "deploy" });
    let out = execute_tool(
        "lifecycle",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(
        out.starts_with("error: unknown lifecycle phase 'deploy'"),
        "{out}"
    );
    assert!(out.contains("check"), "should name valid phases: {out}");
}

/// #1972 red-first, reproduced against this repo's own real tree (no
/// tempfile): `crates/` carries no lifecycle markers of its own, but its
/// child `crates/newt-tuner/` has a real `Cargo.toml` — the same shape as
/// the reported bug's `agent-voice/Cargo.toml`, invisible to root-anchored
/// detection before this fix. `workspace` is relative to `cargo test`'s cwd
/// (this crate's own directory), so `../crates` is the repo's real
/// `crates/` dir. Closes the loop end to end: the nested project is named
/// (not silently dropped), the message is honest (not `error:`-prefixed),
/// and the no-op no longer ledgers as a claimable success.
#[tokio::test]
async fn lifecycle_root_empty_names_a_nested_project_instead_of_a_silent_noop() {
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "phase": "test" });
    let out = execute_tool(
        "lifecycle",
        &args,
        "../crates",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    assert!(
        out.starts_with("no command configured for lifecycle phase 'test'"),
        "{out}"
    );
    assert!(
        out.contains("newt-tuner"),
        "names the nested project: {out}"
    );
    assert!(out.contains("dir=\"<path>\""), "points at the fix: {out}");
    assert!(
        !out.starts_with("error:"),
        "an honest degrade is not a fake failure: {out}"
    );
    assert!(
        !tool_result_ok(&out),
        "a no-op must not ledger as a claimable success: {out}"
    );
}

/// Twin of the above: `dir` resolves detection AND execution against the
/// SAME real nested project directly — proving the resolve_exec_cwd reuse
/// (#1972 part 1). `action=list` keeps this subprocess-free; `ok=true`
/// confirms a genuinely resolved phase is unaffected by the no-op
/// classifier added for the case above.
#[tokio::test]
async fn lifecycle_dir_param_resolves_a_nested_project_directly() {
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "phase": "test", "action": "list", "dir": "newt-tuner" });
    let out = execute_tool(
        "lifecycle",
        &args,
        "../crates",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    assert_eq!(out, "lifecycle test → cargo test", "got: {out}");
    assert!(
        tool_result_ok(&out),
        "a genuinely resolved phase is still ok=true: {out}"
    );
}

/// `save_note` is sink-gated: absent from the base `tool_definitions`
/// (headless/eval callers see no memory tool) and from the merged set
/// without a sink; present in the merged set when a sink exists.
#[test]
fn save_note_advertised_only_with_a_sink() {
    fn names(defs: &serde_json::Value) -> Vec<&str> {
        defs.as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect()
    }
    // Headless/eval callers see no memory tool in the base set …
    let base = tool_definitions();
    assert!(!names(&base).contains(&"save_note"), "got: {base}");
    // … nor in the merged set without a sink …
    let without = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(!names(&without).contains(&"save_note"));
    // … but a sink advertises it.
    let with = merged_tool_definitions(
        &NoMcp, true, false, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(names(&with).contains(&"save_note"), "got: {with}");
}

/// `recall` is source-gated exactly like `save_note` is sink-gated
/// (Step 17.5): absent from the base set and from the merged set
/// without a source; present when one exists.
#[test]
fn recall_advertised_only_with_a_source() {
    fn names(defs: &serde_json::Value) -> Vec<&str> {
        defs.as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect()
    }
    let base = tool_definitions();
    assert!(!names(&base).contains(&"recall"), "got: {base}");
    let without = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(!names(&without).contains(&"recall"));
    let with = merged_tool_definitions(
        &NoMcp, false, true, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(names(&with).contains(&"recall"), "got: {with}");
    // The two gates are independent: both on advertises both.
    let both = merged_tool_definitions(
        &NoMcp, true, true, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(names(&both).contains(&"save_note"));
    assert!(names(&both).contains(&"recall"));
}

/// `memory_fetch` is source-gated exactly like `recall` (#319): absent
/// from the base set and from the merged set without a `MemorySource`;
/// present when one exists. The flag is independent of the others.
#[test]
fn memory_fetch_advertised_only_with_a_source() {
    fn names(defs: &serde_json::Value) -> Vec<&str> {
        defs.as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect()
    }
    let base = tool_definitions();
    assert!(!names(&base).contains(&"memory_fetch"), "got: {base}");
    // Flag off (every existing caller, the inert default) → not advertised.
    let without = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(!names(&without).contains(&"memory_fetch"));
    // Flag on → advertised.
    let with = merged_tool_definitions(
        &NoMcp, false, false, true, false, false, false, false, false, false, false, false, false,
    );
    assert!(names(&with).contains(&"memory_fetch"), "got: {with}");
    // Independent of the save_note / recall gates: all three on lists all.
    let all = merged_tool_definitions(
        &NoMcp, true, true, true, false, false, false, false, false, false, false, false, false,
    );
    assert!(names(&all).contains(&"save_note"));
    assert!(names(&all).contains(&"recall"));
    assert!(names(&all).contains(&"memory_fetch"));
}

/// `is_hallucination` correctly identifies tool-name-as-command and unknown
/// tool names, and correctly skips MCP-namespaced tools.
#[test]
fn hallucination_detection_coverage() {
    // tool name passed to run_command → hallucination
    assert!(is_hallucination(
        "run_command",
        &serde_json::json!({"command": "list_dir ."})
    ));
    // normal shell command → not a hallucination
    assert!(!is_hallucination(
        "run_command",
        &serde_json::json!({"command": "cargo test"})
    ));
    // unknown tool → hallucination
    assert!(is_hallucination(
        "definitely_not_a_real_tool",
        &serde_json::json!({})
    ));
    // MCP-namespaced tool → not a hallucination
    assert!(!is_hallucination(
        "my_server__some_tool",
        &serde_json::json!({})
    ));
    // known direct tools → not hallucinations when called correctly
    for t in [
        "list_dir",
        "read_file",
        "write_file",
        "edit_file",
        "delete_file",
        "use_skill",
        "web_fetch",
        "save_note",
        "recall",
    ] {
        assert!(!is_hallucination(t, &serde_json::json!({"path": "."})));
    }
}

/// #898/#1022: `run_command_redirect` bounces embedded-tool-served LOCAL git
/// ops (and other direct tools), but lets git passthrough ops fall through
/// to the shell — otherwise a model can never `git push` or `git rm`.
#[test]
fn resolve_exec_cwd_confines_to_workspace() {
    // #1159: relative cwd joins under the workspace; absolute passes through
    // (the fs fence rejects escapes); empty/None → workspace root.
    assert_eq!(resolve_exec_cwd("/ws", None), "/ws");
    assert_eq!(resolve_exec_cwd("/ws", Some("")), "/ws");
    assert_eq!(resolve_exec_cwd("/ws", Some("  ")), "/ws");
    // Relative join uses the platform separator (correct — the cwd feeds
    // the confined shell on this OS); compare against the same join.
    let joined = std::path::Path::new("/ws")
        .join("crates/foo")
        .to_string_lossy()
        .into_owned();
    assert_eq!(resolve_exec_cwd("/ws", Some("crates/foo")), joined);
    // An absolute path passes through verbatim (platform-appropriate).
    let abs = if cfg!(windows) {
        "C:\\ws\\sub"
    } else {
        "/ws/sub"
    };
    assert_eq!(resolve_exec_cwd("/ws", Some(abs)), abs);
    // The dispatch args carry it verbatim.
    let a = confined_dispatch_args("ls", &joined);
    assert_eq!(a["cwd"], joined);
}

#[test]
fn split_leading_cd_folds_the_habitual_cd_prefix() {
    // The reported failure: `cd <workspace> && git checkout -b …` tried to
    // exec the `cd` builtin. Fold it: cwd = the path, run the real command.
    let (path, rest) = split_leading_cd("cd /ws/newt-agent && git checkout -b x");
    assert_eq!(path.as_deref(), Some("/ws/newt-agent"));
    assert_eq!(rest, "git checkout -b x");

    // `;` connective folds the same way.
    let (path, rest) = split_leading_cd("cd sub ; ls -la");
    assert_eq!(path.as_deref(), Some("sub"));
    assert_eq!(rest, "ls -la");

    // A quoted path with spaces is returned unquoted; the remainder is kept.
    let (path, rest) = split_leading_cd("cd \"/a b/c\" && cargo test");
    assert_eq!(path.as_deref(), Some("/a b/c"));
    assert_eq!(rest, "cargo test");

    // Only the FIRST cd is folded — a second cd stays in the remainder for
    // the shell engine (we don't chase chdir chains).
    let (path, rest) = split_leading_cd("cd a && cd b && ls");
    assert_eq!(path.as_deref(), Some("a"));
    assert_eq!(rest, "cd b && ls");

    // A bare `cd <path>` folds to an empty remainder (the caller turns this
    // into a guidance note — nothing to exec).
    let (path, rest) = split_leading_cd("cd /somewhere");
    assert_eq!(path.as_deref(), Some("/somewhere"));
    assert!(rest.is_empty());
}

#[test]
fn split_leading_cd_leaves_non_cd_and_ambiguous_commands_whole() {
    // Not a cd at all → unchanged.
    assert_eq!(split_leading_cd("git status"), (None, "git status".into()));
    // `cd` as a substring of another word is not a match.
    assert_eq!(split_leading_cd("cding foo"), (None, "cding foo".into()));
    // `cd <path>` followed by something other than a sequential connective
    // (a pipe here) is left whole — we only fold the safe `&&`/`;` shapes.
    assert_eq!(
        split_leading_cd("cd x | grep y"),
        (None, "cd x | grep y".into())
    );
}

#[test]
fn run_command_redirect_lets_git_network_ops_through() {
    // Ops the embedded git tool cannot do faithfully → fall through (None).
    for cmd in [
        "git push origin fix/foo",
        "git push",
        "git fetch origin",
        "git pull",
        "git clone https://example.com/r.git",
        "git rm src/cockpit.rs",
    ] {
        assert_eq!(run_command_redirect(cmd), None, "{cmd} must fall through");
    }
    // Local ops the embedded git tool handles → still redirect.
    for cmd in [
        "git status",
        "git log --oneline",
        "git add .",
        "git commit -m x",
    ] {
        assert_eq!(
            run_command_redirect(cmd),
            Some("git"),
            "{cmd} must redirect"
        );
    }
    // Other direct tools still redirect; plain shell commands run as-is.
    assert_eq!(run_command_redirect("read_file foo.txt"), Some("read_file"));
    assert_eq!(run_command_redirect("list_dir ."), Some("list_dir"));
    assert_eq!(run_command_redirect("cargo test"), None);
    assert_eq!(run_command_redirect("gh pr create --fill"), None);
    assert_eq!(run_command_redirect(""), None);
}

/// #1262: a command with shell COMPOSITION is a real shell program the
/// embedded tools cannot serve — it must never be redirected (the diagnosed
/// session's legitimate pipeline was bounced + miscounted as a corrected
/// hallucination). Bare servable forms keep redirecting.
#[test]
fn run_command_redirect_passes_composed_commands_through() {
    // The exact diagnosed pipeline.
    assert_eq!(
        run_command_redirect(
            "find . -name \"*.rs\" -type f -print0 | xargs -0 du -k | sort -rn | head 20"
        ),
        None,
        "a pipeline leading with `find` is not a misdirected find call"
    );
    // Redirects and sequencing are composition too.
    assert_eq!(run_command_redirect("find . -name '*.log' > out.txt"), None);
    assert_eq!(run_command_redirect("git status && git diff"), None);
    assert_eq!(run_command_redirect("list_dir . ; echo done"), None);
    assert_eq!(run_command_redirect("read_file $(pick_file)"), None);
    assert_eq!(run_command_redirect("read_file `pick_file`"), None);
    // Bare servable forms still redirect (the true positives hold).
    assert_eq!(run_command_redirect("find . -name \"*.rs\""), Some("find"));
    assert_eq!(run_command_redirect("list_dir src"), Some("list_dir"));
    assert_eq!(run_command_redirect("git status"), Some("git"));
}

/// #1709 family: a COMPOSED `run_command` that creates a git commit bypasses
/// `LocalGitTool::finalize_commit_message` and would land an unattributed
/// commit. The guard `run_command_creates_shell_git_commit` detects it so the
/// run_command arm can refuse predictably and direct the model to the
/// first-class `git` tool. A bare `git commit` is already bounced by
/// `run_command_redirect` (covered above); these are the composed forms that
/// fall through.
#[test]
fn run_command_creates_shell_git_commit_detects_composed_commit_forms() {
    // Sequencing + commit.
    assert!(run_command_creates_shell_git_commit(
        "git add . && git commit -m \"fix the parser\""
    ));
    assert!(run_command_creates_shell_git_commit(
        "git add . ; git commit -m x"
    ));
    // Pipeline commit (e.g. message from stdin).
    assert!(run_command_creates_shell_git_commit(
        "echo \"msg\" | git commit -F -"
    ));
    // Redirect is composition.
    assert!(run_command_creates_shell_git_commit(
        "git commit -m x > commit.log"
    ));
    // Global option with a value before the subcommand.
    assert!(run_command_creates_shell_git_commit(
        "git -c user.email=evil@example.com commit -m x"
    ));
    assert!(run_command_creates_shell_git_commit(
        "git -C /repo commit -m x"
    ));
    // `--git-dir=<path>` carries its value in-token, so the next token IS
    // the subcommand.
    assert!(run_command_creates_shell_git_commit(
        "git --git-dir=/repo/.git commit -m x"
    ));
    // A bare flag global option before the subcommand.
    assert!(run_command_creates_shell_git_commit(
        "git --no-pager commit -m x"
    ));
    // `--amend` is still the `commit` subcommand.
    assert!(run_command_creates_shell_git_commit(
        "git add . && git commit --amend -m x"
    ));
    // Qualified binary path (the model often uses /usr/bin/git).
    assert!(run_command_creates_shell_git_commit(
        "/usr/bin/git -C /repo commit -m x"
    ));
    // Env-assignment prefix forging commit identity.
    assert!(run_command_creates_shell_git_commit(
        "GIT_AUTHOR_NAME=evil GIT_AUTHOR_EMAIL=evil@example.com git commit -m x"
    ));
    // Command substitution / backtick wrapping.
    assert!(run_command_creates_shell_git_commit("$(git commit -m x)"));
    assert!(run_command_creates_shell_git_commit("`git commit -m x`"));
}

/// #1709 family: the guard must NOT fire on legitimate read-only git, git
/// network ops, unrelated commands, or the abort/quit forms of the
/// commit-producing subcommands — the bypass closure is narrow to commit
/// creation only. (`git merge`/`cherry-pick`/`revert`/`rebase` themselves
/// ARE blocked now — see `run_command_creates_shell_git_commit_detects_other_commit_forms`.)
#[test]
fn run_command_creates_shell_git_commit_preserves_readonly_and_unrelated() {
    // Read-only composed git.
    assert!(!run_command_creates_shell_git_commit(
        "git status && git diff"
    ));
    assert!(!run_command_creates_shell_git_commit(
        "git log | grep commit"
    ));
    assert!(!run_command_creates_shell_git_commit(
        "git log --grep=commit --oneline"
    ));
    // Git network passthrough ops.
    assert!(!run_command_creates_shell_git_commit(
        "git add . && git push origin fix/foo"
    ));
    assert!(!run_command_creates_shell_git_commit("git fetch"));
    // `commit` appearing as an ARGUMENT, not a subcommand.
    assert!(!run_command_creates_shell_git_commit(
        "echo git commit > notes.txt"
    ));
    assert!(!run_command_creates_shell_git_commit("cat commit_log.txt"));
    // Non-git commands.
    assert!(!run_command_creates_shell_git_commit("cargo test"));
    assert!(!run_command_creates_shell_git_commit("just check"));
    assert!(!run_command_creates_shell_git_commit(""));
    // Abort/quit forms create NO commit — preserved (fall through). These
    // back out of an in-progress op without creating a commit.
    assert!(!run_command_creates_shell_git_commit("git rebase --abort"));
    assert!(!run_command_creates_shell_git_commit("git rebase --quit"));
    assert!(!run_command_creates_shell_git_commit(
        "git cherry-pick --abort"
    ));
    assert!(!run_command_creates_shell_git_commit("git revert --quit"));
    assert!(!run_command_creates_shell_git_commit("git merge --abort"));
    assert!(!run_command_creates_shell_git_commit("git merge --quit"));
    // `--skip`/`--continue` DO create commits (they advance the operation),
    // so they are NOT abort forms — covered as blocked below.
}

/// #1709 family (audit req 7/8): the other audit-identified commit-producing
/// shell forms — `git merge`, `git cherry-pick`, `git revert`, `git rebase`
/// — are now BLOCKED (route or deny), bare AND composed, while their
/// `--abort`/`--quit` forms pass through (above). `--skip`/`--continue`
/// create commits and stay blocked.
#[test]
fn run_command_creates_shell_git_commit_detects_other_commit_forms() {
    // Bare commit-producing subcommands.
    assert!(run_command_creates_shell_git_commit("git merge feature/x"));
    assert!(run_command_creates_shell_git_commit(
        "git cherry-pick abc123"
    ));
    assert!(run_command_creates_shell_git_commit("git revert abc123"));
    assert!(run_command_creates_shell_git_commit("git rebase main"));
    // Composed with a preceding `git add`.
    assert!(run_command_creates_shell_git_commit(
        "git add . && git merge feature/x"
    ));
    // Qualified binary path.
    assert!(run_command_creates_shell_git_commit(
        "/usr/bin/git -C /repo cherry-pick abc123"
    ));
    // Env-assignment prefix.
    assert!(run_command_creates_shell_git_commit(
        "GIT_AUTHOR_NAME=evil git revert abc123"
    ));
    // `--skip` and `--continue` advance the operation and CREATE commits —
    // they are NOT abort forms, so they stay blocked.
    assert!(run_command_creates_shell_git_commit("git rebase --skip"));
    assert!(run_command_creates_shell_git_commit(
        "git cherry-pick --continue"
    ));
    assert!(run_command_creates_shell_git_commit(
        "git rebase --continue"
    ));
    // An abort flag ANYWHERE in the args exempts a non-commit subcommand
    // (a real abort never creates a commit).
    assert!(!run_command_creates_shell_git_commit(
        "git rebase --abort --continue"
    ));
}

/// #1709 family (req 13): a model cannot escape harness-managed attribution
/// by shelling out `git commit` with its OWN hand-written trailer or forged
/// identity — the guard blocks the commit attempt ENTIRELY, so the model is
/// forced onto the `git` tool, whose finalizer owns attribution. The model's
/// text never reaches a real `git commit`.
#[test]
fn run_command_creates_shell_git_commit_blocks_model_forged_attribution() {
    // Model hand-writes a trailer to impersonate harness attribution.
    assert!(run_command_creates_shell_git_commit(
        "git commit -m \"fix the parser\n\nCo-authored-by: fake (newt-agent v9.9) <x@y>\""
    ));
    // Model forges the author identity via -c.
    assert!(run_command_creates_shell_git_commit(
        "git -c user.name='newt-agent' -c user.email='x@y' commit -m x"
    ));
    // Model forges identity via env prefix.
    assert!(run_command_creates_shell_git_commit(
        "GIT_AUTHOR_NAME=newt-agent GIT_AUTHOR_EMAIL=x@y git commit -m x"
    ));
    // Model tries to suppress attribution by emptying the message via a file.
    assert!(run_command_creates_shell_git_commit(
        "printf '' | git commit -F -"
    ));
}

/// #1709 family: a bare `git commit` is already caught by
/// `run_command_redirect` (the existing bounce to the `git` tool); the new
/// guard is the composed-shell fallback, not a duplicate of the bare case.
#[test]
fn bare_git_commit_is_caught_by_redirect_not_the_shell_guard() {
    assert_eq!(run_command_redirect("git commit"), Some("git"));
    assert_eq!(run_command_redirect("git commit --amend -m x"), Some("git"));
    // The shell guard also reports it (defense in depth), but the redirect
    // fires first in the run_command arm.
    assert!(run_command_creates_shell_git_commit("git commit"));
}

/// #1262: the loop's `hallucination_count` increments exactly on
/// `is_hallucination` (mod.rs call classification), so this pure pin IS the
/// turn-metrics behavior: the diagnosed pipeline counts ZERO hallucinations;
/// a bare misdirected call still counts one.
#[test]
fn pipeline_is_never_counted_as_a_hallucination() {
    assert!(!is_hallucination(
        "run_command",
        &serde_json::json!({"command":
                "find . -name \"*.rs\" -type f -print0 | xargs -0 du -k | sort -rn | head 20"})
    ));
    assert!(!is_hallucination(
        "run_command",
        &serde_json::json!({"command": "git status && git diff"})
    ));
    // The true positive holds: a bare misdirected call still counts.
    assert!(is_hallucination(
        "run_command",
        &serde_json::json!({"command": "list_dir ."})
    ));
}

/// #898 regression: a real `git push` at run_command must NOT be counted as
/// a hallucination (it now runs), while a local `git status` still is.
#[test]
fn is_hallucination_allows_git_network_ops() {
    assert!(!is_hallucination(
        "run_command",
        &serde_json::json!({"command": "git push origin fix/foo"})
    ));
    assert!(!is_hallucination(
        "run_command",
        &serde_json::json!({"command": "git fetch"})
    ));
    assert!(!is_hallucination(
        "run_command",
        &serde_json::json!({"command": "git rm src/cockpit.rs"})
    ));
    assert!(is_hallucination(
        "run_command",
        &serde_json::json!({"command": "git status"})
    ));
}

/// #898: the forge PR/MR-creation URL is extracted from git's push output
/// (GitHub and GitLab), and ordinary URLs do not false-positive.
#[test]
fn pr_creation_url_extracts_github_and_gitlab() {
    let github = "remote: Create a pull request for 'fix/foo' on GitHub by visiting:\n\
                      remote:      https://github.com/OWNER/REPO/pull/new/fix/foo\n";
    assert_eq!(
        pr_creation_url(github),
        Some("https://github.com/OWNER/REPO/pull/new/fix/foo")
    );
    let gitlab = "remote: To create a merge request for topic, visit:\n\
                      remote:   https://gitlab.com/g/p/-/merge_requests/new?x=topic\n";
    assert_eq!(
        pr_creation_url(gitlab),
        Some("https://gitlab.com/g/p/-/merge_requests/new?x=topic")
    );
    // No PR URL present → None (ordinary fetch/clone output, plain links).
    assert_eq!(pr_creation_url("Already up to date.\n"), None);
    assert_eq!(
        pr_creation_url("see https://github.com/OWNER/REPO/issues/1"),
        None
    );
}

/// step-7.1a / invariant 9: the host-shell child must NOT inherit the two
/// authority switches. An ambient `NEWT_DISABLE_OCAP=1` / `NEWT_FULL_ACCESS=1`
/// (from a wrapper/pod, or this process's own Yolo lane) would otherwise flow
/// into the child and let it re-assert authority the session did not grant.
/// `env_remove` marks the var absent in the child's env plan (`get_envs` →
/// `(key, None)`); this asserts both are so marked. Fails on any code path
/// that builds the child without the `env_remove` calls.
#[cfg(not(windows))]
#[test]
fn host_shell_command_strips_authority_env() {
    let c = host_shell_command("bash", "true", "/tmp");
    let removed: Vec<String> = c
        .as_std()
        .get_envs()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    // #8: newt's WHOLE control plane is excised — every authority switch and
    // every newt-owned secret, not just the two OCAP flags. A regression that
    // drops any one of them re-opens the authority-survives-one-hop /
    // secret-leak hole.
    for key in CHILD_STRIPPED_AUTHORITY_ENV {
        assert!(
            removed.iter().any(|k| k == key),
            "{key} not stripped from the host-shell child; env plan: {removed:?}"
        );
    }
    // The specific credential-grade + Yolo-deriving switches, named so the
    // intent is legible in the test, not just the loop.
    for critical in [
        "NEWT_UNSAFE_HOST_EXEC",
        "NEWT_AGENT_KEY",
        "NEWT_OPERATOR_KEY",
        "NEWT_TOKEN_PASSPHRASE",
    ] {
        assert!(
            removed.iter().any(|k| k == critical),
            "{critical} must never reach a host-shell child"
        );
    }
}

/// #898: after a push whose output carries a PR-creation URL,
/// `shell_envelope_output` appends the `gh pr create` next-step hint (and the
/// URL survives), while ordinary command output is left untouched.
#[test]
fn shell_envelope_output_appends_pr_hint_on_push() {
    let push = serde_json::json!({
        "exit_code": 0,
        "stdout": "",
        "stderr": "remote: Create a pull request for 'fix/foo' on GitHub by visiting:\n\
                   remote:      https://github.com/OWNER/REPO/pull/new/fix/foo\n",
    });
    let out = shell_envelope_output(&push, 50, false, false, None, None);
    assert!(out.contains("gh pr create --fill"), "hint missing: {out}");
    assert!(
        out.contains("https://github.com/OWNER/REPO/pull/new/fix/foo"),
        "url dropped: {out}"
    );

    // Ordinary output: no hint, payload unchanged.
    let plain = serde_json::json!({ "exit_code": 0, "stdout": "hello\n", "stderr": "" });
    let out = shell_envelope_output(&plain, 50, false, false, None, None);
    assert!(!out.contains("gh pr create"), "spurious hint: {out}");
    assert_eq!(out, "hello\n");
}

#[test]
fn shell_envelope_output_spills_full_output_before_head_tail_cap() {
    let full = format!(
        "HEAD_ONLY_MARKER\n{}\nMIDDLE_ONLY_MARKER\n{}\nTAIL_ONLY_MARKER\n",
        "alpha\n".repeat(10_000),
        "omega\n".repeat(10_000)
    );
    let envelope = serde_json::json!({
        "exit_code": 0,
        "stdout": full,
        "stderr": "",
    });
    let store = content_spill::SessionSpillStore::new([7u8; 16]);
    let mut display = ToolDisplay::new(Vec::new(), false, 80, 3, false);
    display.call("run_command", "large-output-command");
    let out = shell_envelope_output(&envelope, 50, false, true, Some(&store), Some(&mut display));
    display.result(&out);

    assert!(out.contains("HEAD_ONLY_MARKER"), "head dropped: {out}");
    assert!(out.contains("TAIL_ONLY_MARKER"), "tail dropped: {out}");
    // The teaser now names a `spill:<cid>` content handle (not a literal s0); it
    // must parse as a canonical CID and resolve in the store to the full payload.
    let handle = out
        .split("spill:")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("teaser names a spill handle");
    let cid = content_spill::SpillCid::parse(handle).expect("handle is a canonical CID");
    assert!(
        out.contains("grep=\"<pattern>\""),
        "search affordance missing: {out}"
    );
    let stored = store.fetch(&cid).expect("full output stored").redacted_text;
    assert!(
        stored.contains("MIDDLE_ONLY_MARKER"),
        "spilled payload was capped before storage"
    );
    assert!(stored.ends_with("TAIL_ONLY_MARKER\n"));
    let rendered = String::from_utf8(display.into_inner()).unwrap();
    assert!(
        rendered.contains("▓ TAIL_ONLY_MARKER\n…\n"),
        "operator spill lost the raw shell tail: {rendered}"
    );
    assert!(
        !rendered.contains("memory_fetch(\"spill:"),
        "operator saw the model teaser instead of raw shell output: {rendered}"
    );
}

#[test]
fn shell_envelope_without_streams_commits_the_exit_result() {
    let envelope = serde_json::json!({
        "exit_code": 3,
        "stdout": "",
        "stderr": "",
    });
    let mut display = ToolDisplay::new(Vec::new(), false, 80, 3, false);
    display.call("run_command", "exit 3");
    let out = shell_envelope_output(&envelope, 50, false, false, None, Some(&mut display));
    display.result(&out);

    // #1969: a NONZERO exit is now marked as a failure, because the ledger's
    // `ok` bit is `tool_result_ok`'s prefix test and `(exit 3)` read as a
    // success. The bare `(exit N)` rendering survives for exit 0, which is
    // the case it was written for.
    assert_eq!(out, "error: command exited 3");
    assert_eq!(
        String::from_utf8(display.into_inner()).unwrap(),
        "⚙  run_command: exit 3\n▒ error: command exited 3\n…\n"
    );
}

#[test]
fn envelope_denied_reads_structured_flag_only() {
    assert!(envelope_denied(&serde_json::json!({"denied": true})));
    assert!(!envelope_denied(&serde_json::json!({"denied": false})));
    assert!(!envelope_denied(&serde_json::json!({})));
    // A non-bool `denied` is treated as not-denied, never a panic.
    assert!(!envelope_denied(&serde_json::json!({"denied": "yes"})));
}

#[test]
fn envelope_denial_reason_joins_or_falls_back() {
    let multi = serde_json::json!({
        "denials": [
            {"kind": "exec", "target": "rm", "reason": "exec rm denied"},
            {"kind": "open", "target": "/etc/shadow", "reason": "open denied"}
        ]
    });
    assert_eq!(
        envelope_denial_reason(&multi),
        "exec rm denied; open denied"
    );
    // Missing or empty denials → the generic message, never a panic.
    let generic = "denied: the capability leash refused an operation";
    assert_eq!(envelope_denial_reason(&serde_json::json!({})), generic);
    assert_eq!(
        envelope_denial_reason(&serde_json::json!({"denials": []})),
        generic
    );
    // Entries without a string `reason` are skipped.
    assert_eq!(
        envelope_denial_reason(&serde_json::json!({"denials": [{"kind": "exec"}]})),
        generic
    );
}

#[test]
fn exec_allowlist_name_takes_basename() {
    assert_eq!(exec_allowlist_name("env"), "env");
    assert_eq!(exec_allowlist_name("/usr/bin/env"), "env");
    assert_eq!(exec_allowlist_name("/usr/bin/"), "bin");
    assert_eq!(exec_allowlist_name("C:\\tools\\env.exe"), "env.exe");
}

/// #775 (§2.5): denial recovery uses the BARE command target(s), never the
/// reason sentence. Stuffing the full reason into the former notice's
/// `'{target}'` field produced the
/// field-report garble `capability denied: exec does not permit '<whole
/// reason sentence>'`.
#[test]
fn exec_denial_target_label_is_the_bare_command_not_the_reason() {
    let one = serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "exec",
            "target": "export",
            "reason": "exec of \"export\" is not within the granted authority"
        }]
    });
    let label = exec_denial_target_label(&one);
    assert_eq!(label, "export");
    // It is the bare command — NEVER the reason sentence (which, in the
    // `'{target}'` slot, was the nested garble).
    assert!(!label.contains("is not within the granted authority"));
    // Multiple targets join cleanly; an envelope with no target falls back
    // to a generic label so the notice still prints one clean line.
    let multi = serde_json::json!({
        "denials": [
            {"kind": "exec", "target": "export", "reason": "r"},
            {"kind": "exec", "target": "set", "reason": "r"}
        ]
    });
    assert_eq!(exec_denial_target_label(&multi), "export, set");
    assert_eq!(
        exec_denial_target_label(&serde_json::json!({})),
        "a command"
    );
}

#[test]
fn host_of_url_extracts_hosts_conservatively() {
    assert_eq!(host_of_url("https://docs.rs/serde"), Some("docs.rs".into()));
    assert_eq!(host_of_url("http://Docs.RS"), Some("docs.rs".into()));
    assert_eq!(
        host_of_url("https://user:pw@example.com:8443/p?q#f"),
        Some("example.com".into())
    );
    assert_eq!(host_of_url("https://[::1]:8080/x"), Some("::1".into()));
    // Unparseable / non-http inputs skip the pre-check (None) rather
    // than guessing — enforcement stays with the leash either way.
    assert_eq!(host_of_url("not a url"), None);
    assert_eq!(host_of_url("ftp://example.com"), None);
    assert_eq!(host_of_url("https:///path-only"), None);
}

#[test]
fn exec_denial_requests_lifts_only_pure_exec_envelopes() {
    // The promptable case: every entry is an exec denial with a target;
    // the request target is the allowlist basename (the grantable name).
    let exec_only = serde_json::json!({
        "denied": true,
        "denials": [
            {"kind": "exec", "target": "/usr/bin/npm", "reason": "exec npm denied"},
            {"kind": "exec", "target": "node", "reason": "exec node denied"}
        ]
    });
    let reqs = exec_denial_requests(&exec_only).expect("promptable");
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].tool, "run_command");
    assert_eq!(reqs[0].kind, DenialKind::Exec);
    assert_eq!(
        reqs[0].target, "npm",
        "basename, same rule as the config hint"
    );
    assert_eq!(reqs[0].reason, "exec npm denied");
    assert_eq!(reqs[1].target, "node");

    // A non-exec entry anywhere keeps the standard denial: mapping an
    // opaque `open` onto an fs axis would over-grant.
    let mixed = serde_json::json!({
        "denials": [
            {"kind": "exec", "target": "npm", "reason": "r"},
            {"kind": "open", "target": "/etc/shadow", "reason": "r"}
        ]
    });
    assert!(exec_denial_requests(&mixed).is_none());

    // Missing/empty pieces are never promptable.
    assert!(exec_denial_requests(&serde_json::json!({})).is_none());
    assert!(exec_denial_requests(&serde_json::json!({"denials": []})).is_none());
    let empty_target = serde_json::json!({
        "denials": [{"kind": "exec", "target": "", "reason": "r"}]
    });
    assert!(exec_denial_requests(&empty_target).is_none());
    let no_target = serde_json::json!({
        "denials": [{"kind": "exec", "reason": "r"}]
    });
    assert!(exec_denial_requests(&no_target).is_none());

    // #1150: a STRUCTURAL refusal must NOT be promptable — offering a grant
    // for `$(` (which the engine cannot interpret) is a grant->denial
    // contradiction. The exact reason strings are agent-bridle's
    // Refusal::Display output (verified against parse.rs).
    let dynamic = serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "exec",
            "target": "command/arithmetic substitution `$(`",
            "reason": "refused by design: command/arithmetic substitution `$(` is a \
                       dynamic construct the confined shell does not interpret (use the \
                       embedder's unbridled/--yolo path for a full shell)"
        }]
    });
    assert!(
        exec_denial_requests(&dynamic).is_none(),
        "structural refusal must not offer a grant menu (#1150)"
    );
    let unsupported = serde_json::json!({
        "denials": [{
            "kind": "exec",
            "target": "heredoc/herestring `<<`",
            "reason": "not yet supported by the confined shell engine: \
                       heredoc/herestring `<<` (tracked on agent-bridle#34)"
        }]
    });
    assert!(exec_denial_requests(&unsupported).is_none());
    // A genuine authority denial (not structural) STAYS promptable.
    let authority = serde_json::json!({
        "denials": [{"kind": "exec", "target": "cargo",
                     "reason": "exec of \"cargo\" is not within the granted authority"}]
    });
    assert!(exec_denial_requests(&authority).is_some());
}

/// #905: a NET denial envelope (agent-bridle #196 shape) lifts to a per-host
/// net PermissionRequest; the target is the CONNECT host verbatim (no
/// basename mangling). Non-net / mixed / empty batches stay flat.
#[test]
fn net_denial_requests_lifts_only_pure_net_envelopes() {
    let net_only = serde_json::json!({
        "denied": true,
        "denials": [
            {"kind": "net", "target": "github.com", "reason": "net does not permit 'github.com'"},
            {"kind": "net", "target": "api.github.com", "reason": "net does not permit 'api.github.com'"}
        ]
    });
    let reqs = net_denial_requests(&net_only).expect("promptable");
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].tool, "run_command");
    assert_eq!(reqs[0].kind, DenialKind::Net);
    assert_eq!(
        reqs[0].target, "github.com",
        "host verbatim, not a basename"
    );
    assert_eq!(reqs[0].reason, "net does not permit 'github.com'");
    assert_eq!(reqs[1].target, "api.github.com");

    // A non-net entry anywhere → not net-promptable (exec lifter handles exec).
    let mixed = serde_json::json!({
        "denials": [
            {"kind": "net", "target": "github.com", "reason": "r"},
            {"kind": "exec", "target": "npm", "reason": "r"}
        ]
    });
    assert!(net_denial_requests(&mixed).is_none());
    // Exec-only is not net-promptable; empty/missing targets never are.
    let exec_only = serde_json::json!({"denials": [{"kind": "exec", "target": "npm"}]});
    assert!(net_denial_requests(&exec_only).is_none());
    assert!(net_denial_requests(&serde_json::json!({"denials": []})).is_none());
    let empty_target = serde_json::json!({"denials": [{"kind": "net", "target": ""}]});
    assert!(net_denial_requests(&empty_target).is_none());
}

/// #905: the human denial NOTICE labels a pure-net refusal `net` (not `exec`),
/// so it never reads "exec does not permit '<host>'". Exec / mixed stay `exec`.
#[test]
fn denials_name_the_exact_recovery_call() {
    // #1160: the model shouldn't infer parameters the harness holds — a
    // denial carries the copy-pasteable request_permissions(...) call.
    let fs = denied_fs_result("fs_write", "/etc/hosts");
    assert!(
        fs.contains(r#"request_permissions(capability="fs_write", target="/etc/hosts""#),
        "{fs}"
    );
    let hint = denial_recovery_hint("exec", "cargo");
    assert!(
        hint.contains(r#"capability="exec""#) && hint.contains(r#"target="cargo""#),
        "{hint}"
    );
}

#[test]
fn denial_axis_label_is_net_only_for_pure_net() {
    let net = serde_json::json!({"denials": [{"kind": "net", "target": "github.com"}]});
    assert_eq!(denial_axis_label(&net), "net");
    let exec = serde_json::json!({"denials": [{"kind": "exec", "target": "rm"}]});
    assert_eq!(denial_axis_label(&exec), "exec");
    let mixed = serde_json::json!({
        "denials": [{"kind": "net", "target": "h"}, {"kind": "exec", "target": "rm"}]
    });
    assert_eq!(denial_axis_label(&mixed), "exec", "mixed defaults to exec");
    assert_eq!(denial_axis_label(&serde_json::json!({})), "exec");
}

#[test]
fn tui_permits_path_prefix_semantics() {
    use crate::caveats::Scope;
    assert!(tui_permits_path(&Scope::All, "/anything/at/all"));
    assert!(!tui_permits_path(&Scope::<String>::none(), "/ws/file"));
    let only = Scope::only(["/ws".to_string()]);
    assert!(tui_permits_path(&only, "/ws/sub/file.rs"));
    assert!(tui_permits_path(&only, "/ws"), "the workspace root itself");
    assert!(!tui_permits_path(&only, "/elsewhere/file.rs"));
    // `..` traversal must NOT escape: a path that lexically resolves outside
    // the workspace is denied even though it textually begins with it.
    assert!(
        !tui_permits_path(&only, "/ws/../etc/passwd"),
        "`..` traversal escapes the workspace"
    );
    assert!(
        !tui_permits_path(&only, "/ws/../../etc/passwd"),
        "repeated `..` traversal escapes the workspace"
    );
    // A sibling dir that merely shares the string prefix is not under /ws.
    assert!(
        !tui_permits_path(&only, "/ws-secret/file.rs"),
        "sibling-prefix collision escapes the workspace"
    );
    // A `..` that stays inside the workspace is still permitted.
    assert!(tui_permits_path(&only, "/ws/sub/../file.rs"));
}

/// Ratchet for the OPEN `fs-canonical-containment` deviation (issue #522,
/// `docs/security/ocap-deviations.md`). `tui_permits_path` is string-lexical:
/// it collapses `..` but does NOT resolve symlinks, so a link *inside* the
/// workspace pointing OUT is permitted even though the OS would read the
/// outside target. This test builds the path the call sites do
/// (`workspace.join(model_path)`) over a REAL symlink and PINS that residual.
///
/// When canonicalize-then-contain lands (the deviation's closure criterion),
/// the gate will deny the symlinked path and this assertion MUST flip to
/// `!tui_permits_path(...)` — that break is the signal to close the deviation.
/// Unix-only: Windows symlinks need privileges (mirrors
/// `find_does_not_follow_symlinks_out_of_workspace`).
#[cfg(unix)]
#[test]
fn tui_permits_path_is_a_lexical_prefilter_not_the_fence() {
    use crate::caveats::Scope;
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret"), b"x").unwrap();
    let ws = tempfile::TempDir::new().unwrap();
    // A symlink under the workspace whose target is OUTSIDE it.
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    let only = Scope::only([ws.path().to_string_lossy().into_owned()]);

    // What the read/write call sites feed the gate for model path "link/secret".
    let via_link = ws.path().join("link").join("secret");
    // `tui_permits_path` is a cheap LEXICAL PRE-FILTER — it still admits the
    // symlinked name (it cannot see through the link, and is not meant to).
    // The authoritative fence is now object-bound: the fs tool arms resolve
    // through `WorkspaceDir` (openat2 RESOLVE_BENEATH), so the *arm* denies
    // this escape even though the predicate admits the name — proven by
    // `{read_file,list_dir,write_file,edit_file,delete_file}_symlink_under_
    // workspace_escaping_is_denied` and `apply_whole_files_denies_symlink_
    // escape_object_bound`. This test therefore pins that the predicate stays
    // a prefilter (NOT that a residual is open — #522 is CLOSED, step-52.7).
    assert!(
        tui_permits_path(&only, &via_link.to_string_lossy()),
        "the lexical prefilter admits the name; object-binding is the fence"
    );

    // Contrast: a plain `..` escape through the SAME root is already denied
    // (lexical containment, the part #502 did fix) — so this isn't a blanket
    // hole, only the symlink-resolution gap.
    let dotdot = ws.path().join("..").join("etc").join("passwd");
    assert!(
        !tui_permits_path(&only, &dotdot.to_string_lossy()),
        "`..` escape is denied even though symlink escape is not"
    );
}

/// The file tools retain the lexical OCAP residual above, but their
/// provenance hook must fail closed so it never labels an outside target as
/// a workspace artifact.
#[cfg(unix)]
#[test]
fn artifact_provenance_rejects_physical_symlink_escapes() {
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("existing"), b"x").unwrap();
    let ws = tempfile::TempDir::new().unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    assert!(artifact_path_is_physically_within_workspace(
        ws.path(),
        &ws.path().join("new/leaf.txt")
    ));
    assert!(!artifact_path_is_physically_within_workspace(
        ws.path(),
        &ws.path().join("link/existing")
    ));
    assert!(!artifact_path_is_physically_within_workspace(
        ws.path(),
        &ws.path().join("link/new-file")
    ));

    std::os::unix::fs::symlink(outside.path().join("missing"), ws.path().join("dangling")).unwrap();
    assert!(!artifact_path_is_physically_within_workspace(
        ws.path(),
        &ws.path().join("dangling")
    ));
}

#[test]
fn artifact_file_streaming_hash_and_postcondition_are_exact() {
    let ws = tempfile::TempDir::new().unwrap();
    let bytes = vec![0x5a; 3 * 64 * 1024 + 17];
    let path = ws.path().join("large.bin");
    std::fs::write(&path, &bytes).unwrap();

    assert_eq!(
        artifact_preimage_state(&path, true),
        super::super::artifact_hooks::ArtifactFileState::from_bytes(&bytes)
    );
    assert!(artifact_file_matches(&path, &bytes).unwrap());
    let mut different = bytes.clone();
    different[64 * 1024] ^= 1;
    assert!(!artifact_file_matches(&path, &different).unwrap());
}

#[cfg(unix)]
#[test]
fn artifact_preimage_never_opens_non_regular_files() {
    let ws = tempfile::TempDir::new().unwrap();
    let socket = ws.path().join("local.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    assert_eq!(
        artifact_preimage_state(&socket, true),
        super::super::artifact_hooks::ArtifactFileState::unavailable("preimage_not_regular_file")
    );
}

// --- PR4: the `git` tool is presence-gated -----------------------------

#[test]
fn git_tool_advertised_only_with_the_presence_gate() {
    fn names(defs: &serde_json::Value) -> Vec<&str> {
        defs.as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect()
    }
    let with = merged_tool_definitions(
        &NoMcp, false, false, false, true, false, false, false, false, false, false, false, false,
    );
    assert!(names(&with).contains(&"git"), "with_git advertises git");
    let without = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(!names(&without).contains(&"git"), "no git without the gate");
    // #479: the /team toggle advertises both crew tools, and only then.
    let team = merged_tool_definitions(
        &NoMcp, false, false, false, false, true, false, false, false, false, false, false, false,
    );
    assert!(
        names(&team).contains(&"crew") && names(&team).contains(&"compose_roster"),
        "with_team advertises crew + compose_roster"
    );
    assert!(
        !names(&without).contains(&"crew"),
        "no crew without the gate"
    );
    // Step 26.4 (#583): the scratchpad state tools, only with the gate on.
    let scratch = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, true, false, false, false, false, false, false,
    );
    for t in ["state_set", "state_get", "state_clear"] {
        assert!(
            names(&scratch).contains(&t),
            "{t} advertised with_scratchpad"
        );
        assert!(!names(&without).contains(&t), "{t} hidden without the gate");
        assert!(
            !is_hallucination(t, &serde_json::json!({})),
            "{t} is a real tool"
        );
    }
    // Step 26.5.5 (#582): the code_search tool, only with its gate on.
    let code = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, true, false, false, false, false, false,
    );
    assert!(
        names(&code).contains(&"code_search"),
        "code_search advertised"
    );
    assert!(
        !names(&without).contains(&"code_search"),
        "code_search hidden without the gate"
    );
    assert!(!is_hallucination("code_search", &serde_json::json!({})));
    // Step 26.6a (#585): the experiential record/recall tools, only with the gate.
    let exp = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, true, false, false, false, false,
    );
    for t in ["experience_record", "experience_recall"] {
        assert!(names(&exp).contains(&t), "{t} advertised with_experiential");
        assert!(!names(&without).contains(&t), "{t} hidden without the gate");
        assert!(
            !is_hallucination(t, &serde_json::json!({})),
            "{t} is a real tool"
        );
    }
    // Step 26.6b (#586) / #715 PR2: the scheduled update_plan + plan_get tools,
    // only with the gate (plan_set/plan_advance collapsed into update_plan).
    let sched = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, true, false, false, false,
    );
    for t in ["update_plan", "plan_get"] {
        assert!(names(&sched).contains(&t), "{t} advertised with_scheduled");
        assert!(!names(&without).contains(&t), "{t} hidden without the gate");
        assert!(
            !is_hallucination(t, &serde_json::json!({})),
            "{t} is a real tool"
        );
    }
    for t in ["enter_plan_mode", "exit_plan_mode"] {
        assert!(
            !names(&sched).contains(&t),
            "{t} needs a session Plan control as well as the scheduled ledger"
        );
    }
    let plan_control_only = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, true, false,
    );
    assert!(
        !names(&plan_control_only).contains(&"enter_plan_mode"),
        "enter_plan_mode needs scheduled planning as well as the session control"
    );
    assert!(
        !names(&plan_control_only).contains(&"exit_plan_mode"),
        "an inactive control must not advertise an unnecessary exit"
    );
    let active_plan_control = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, true, true,
    );
    assert!(
        !names(&active_plan_control).contains(&"enter_plan_mode"),
        "enter still requires the scheduled ledger"
    );
    assert!(
        names(&active_plan_control).contains(&"exit_plan_mode"),
        "an active Plan phase must keep exit available if scheduled planning is toggled off"
    );
    let plan_ready_inactive = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, true, false, true, false,
    );
    assert!(
        names(&plan_ready_inactive).contains(&"enter_plan_mode"),
        "scheduled planning plus a control advertises enter"
    );
    assert!(
        names(&plan_ready_inactive).contains(&"exit_plan_mode"),
        "a frozen multi-round catalog that advertises enter must also advertise same-turn exit"
    );
    let plan_mode = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, true, false, true, true,
    );
    for t in ["enter_plan_mode", "exit_plan_mode"] {
        assert!(
            names(&plan_mode).contains(&t),
            "{t} is advertised when both required seams are present"
        );
    }
    // `/mode auto`: the model-facing selector exists only when the
    // session injects its bounded next-turn control.
    let operating_mode = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, true, false, false,
    );
    assert!(
        names(&operating_mode).contains(&"select_operating_mode"),
        "auto-mode control advertises its selector"
    );
    assert!(
        !names(&without).contains(&"select_operating_mode"),
        "selector is hidden outside /mode auto"
    );
    assert!(!is_hallucination(
        "select_operating_mode",
        &serde_json::json!({})
    ));
}

#[tokio::test]
async fn state_tools_dispatch_only_with_a_store() {
    use crate::agentic::scratchpad::{ScratchpadStore, SessionScratchpadStore};
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "key": "k", "value": "v" });
    // Step 26.4: without a store the tool was never advertised → unknown.
    let none = execute_tool(
        "state_set",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(none.starts_with("unknown tool: state_set"), "{none}");
    // With a store → routes to the executor and mutates it.
    let store = SessionScratchpadStore::default();
    let set = execute_tool(
        "state_set",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&store as &dyn ScratchpadStore),
        None,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(set, "stored: k");
    assert_eq!(store.get("k").as_deref(), Some("v"));
}

#[tokio::test]
async fn code_search_dispatch_only_with_a_searcher() {
    use crate::agentic::semantic::{CodeSearch, Embedder, SessionSemanticIndex};
    struct E;
    #[async_trait::async_trait]
    impl Embedder for E {
        async fn embed(&self, _t: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![1.0])
        }
    }
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "query": "find it" });
    // Step 26.5.5: no searcher → unknown tool (presence-gate parity).
    let none = execute_tool(
        "code_search",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(none.starts_with("unknown tool: code_search"), "{none}");
    // with a searcher (empty index) → routes to the executor (labelled no-match).
    let idx = SessionSemanticIndex::default();
    let search = CodeSearch {
        embedder: &E,
        index: &idx,
        top_k: 1,
        steer: None,
        status: None,
    };
    let out = execute_tool(
        "code_search",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(search),
        None,
        None,
        None,
    )
    .await;
    assert!(out.contains("no code matched"), "{out}");
}

#[tokio::test]
async fn experiential_dispatch_only_with_a_store() {
    use crate::agentic::experiential::{ExperienceStore, SessionExperienceStore};
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({
        "task": "ci flake", "outcome": "fixed", "lesson": "pin the seed for the fuzz test"
    });
    // Step 26.6a: no store → unknown tool for BOTH arms (presence-gate parity).
    for name in ["experience_record", "experience_recall"] {
        let out = execute_tool(
            name, &args, ".", false, 20, &caveats, &mut NoMcp, None, None, None, None, None, None,
            None, None, None, None, None, None, None,
        )
        .await;
        assert!(out.starts_with(&format!("unknown tool: {name}")), "{out}");
    }
    // with a store → record routes to the executor and mutates it.
    let store = SessionExperienceStore::default();
    let out = execute_tool(
        "experience_record",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&store as &dyn ExperienceStore),
        None,
    )
    .await;
    assert_eq!(out, "recorded experience");
    assert_eq!(store.count(), 1);
}

#[tokio::test]
async fn scheduled_dispatch_only_with_a_ledger() {
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "plan": [
            { "step": "a", "status": "in_progress" },
            { "step": "b", "status": "pending" },
        ] });
    // Step 26.6b / #716 / #715 PR2: no ledger → unknown tool for ALL plan arms
    // (presence-gate parity, including the read-only plan_get).
    for name in ["update_plan", "plan_get"] {
        let out = execute_tool(
            name, &args, ".", false, 20, &caveats, &mut NoMcp, None, None, None, None, None, None,
            None, None, None, None, None, None, None,
        )
        .await;
        assert!(out.starts_with(&format!("unknown tool: {name}")), "{out}");
    }
    // with a ledger → update_plan routes to the executor and mutates it.
    let ledger = SessionStepLedger::default();
    let out = execute_tool(
        "update_plan",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&ledger as &dyn StepLedger),
    )
    .await;
    assert!(out.starts_with("<plan>\n"), "{out}");
    assert_eq!(ledger.count(), 2);
    // #716: plan_get with a ledger renders the <plan> block, read-only.
    let got = execute_tool(
        "plan_get",
        &serde_json::json!({}),
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&ledger as &dyn StepLedger),
    )
    .await;
    assert!(got.starts_with("<plan>\n"), "{got}");
    assert_eq!(ledger.count(), 2, "plan_get is read-only");
}

#[tokio::test]
async fn resume_context_dispatch_degrades_without_a_recall_source() {
    // #714: advertised ALWAYS, so dispatch never reports "unknown tool" —
    // with no recall_source (headless) it returns the clear no-history line.
    let caveats = crate::caveats::Caveats::top();
    let out = execute_tool(
        "resume_context",
        &serde_json::json!({}),
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None, // build_check_cmd
        None, // note_sink
        None, // recall_source
        None, // memory_source
        None, // permission_gate
        None, // exec_floor
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert!(
        out.contains("no conversation history available this session"),
        "{out}"
    );
    assert!(!out.starts_with("unknown tool"), "{out}");
}

#[test]
fn run_build_check_reports_pass_fail_and_spawn_error() {
    let ws = tempfile::TempDir::new().unwrap();
    let ws_str = ws.path().to_string_lossy();

    // build_check now runs CONFINED through `ConstrainedExecutor` (P4). On the
    // normative Linux+Landlock platform the trivial commands run under the
    // fence, so we assert the exact confined pass/fail. Off it, the outcome
    // depends on the platform's kernel backend — Windows AppContainer / macOS
    // Seatbelt may confine-and-run, or the spawn fails closed — and BOTH are
    // secure (the executor never runs the repo-controlled command unconfined).
    // So off Linux we assert only a well-formed outcome, never the specific
    // one; the strong confinement guarantee is proven by the real-resource
    // Landlock test (`tests/confined_exec_landlock.rs`).
    //
    // `kernel_fs_fence_available()` is used (not `cfg!() &&
    // agent_bridle::landlock_is_supported()`): that symbol is Linux-only, so
    // calling it under a runtime `cfg!()` fails to COMPILE off Linux.
    let passed = run_build_check(passing_build_check_cmd(), &ws_str);
    if crate::confined_exec::kernel_fs_fence_available() {
        // Under the DenyAll egress floor the trivial command runs confined via
        // the net guard — resolved as a sibling `newt-net-guard` in a dev/test
        // build, or by `newt __net-guard` self-exec in production. In a minimal
        // build layout where the guard binary is not present the spawn fails
        // CLOSED (a secure outcome), so accept either the confined pass or the
        // fail-closed refusal; assert the fail path only when the pass path ran.
        if passed == "  ✓ build check passed" {
            let failed = run_build_check(&failing_build_check_cmd("boom"), &ws_str);
            assert!(failed.contains("✗ build check failed"), "got: {failed}");
            assert!(failed.contains("boom"), "stderr excerpt shown: {failed}");
        } else {
            assert!(
                passed.contains("⚠ build check could not run"),
                "with the egress floor, build_check must confine-and-run or fail \
                     closed, got: {passed}"
            );
        }
    } else {
        assert!(
            passed == "  ✓ build check passed" || passed.contains("⚠ build check could not run"),
            "off Linux, build_check must confine-and-run or fail closed, got: {passed}"
        );
    }
    // A nonexistent workspace dir → the command can't even spawn/confine.
    let err = run_build_check(passing_build_check_cmd(), "/definitely/not/a/dir");
    assert!(err.contains("⚠ build check could not run"), "got: {err}");
}
