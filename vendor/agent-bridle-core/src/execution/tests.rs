//! Acceptance tests for the managed execution lifecycle (#370).
//!
//! These drive **real child processes**. A sink exercised through a recording
//! control proves that the sink's own state machine is consistent; it proves
//! nothing about whether `cancel` reaps a grandchild, whether a stalled
//! consumer wedges a pipe, or whether a terminal is published while an output
//! writer is still alive. Where a proof genuinely cannot run on the host (the
//! egress proxy stays inert on backends that cannot address-fence loopback),
//! the test says so and skips explicitly rather than asserting something
//! weaker and calling it a pass.

use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::{Denial, DenialKind};

// ── portable: limits, request shape, transport, platform posture ────────────
//
// These need no child process, so they run on every target.

#[test]
fn absurd_and_overflowing_limits_are_refused() {
    assert!(
        ExecutionLimits::new(
            MAX_QUEUED_EVENTS_CEILING + 1,
            4096,
            256,
            Duration::from_secs(1),
            Duration::from_secs(1)
        )
        .is_err(),
        "an above-ceiling event bound must be refused, not clamped"
    );
    assert!(ExecutionLimits::new(
        16,
        MAX_QUEUED_OUTPUT_BYTES_CEILING + 1,
        256,
        Duration::from_secs(1),
        Duration::from_secs(1)
    )
    .is_err());
    assert!(ExecutionLimits::new(
        16,
        4096,
        MAX_OUTPUT_CHUNK_BYTES_CEILING + 1,
        Duration::from_secs(1),
        Duration::from_secs(1)
    )
    .is_err());
    assert!(
        ExecutionLimits::new(0, 4096, 256, Duration::from_secs(1), Duration::from_secs(1)).is_err(),
        "a zero bound is not a bound"
    );
    assert!(
        ExecutionLimits::new(
            16,
            4096,
            256,
            Duration::from_secs(10_000),
            Duration::from_secs(1)
        )
        .is_err(),
        "an unbounded grace period must be refused"
    );
    // The ceilings themselves must not multiply into an overflow.
    assert!(ExecutionLimits::new(
        MAX_QUEUED_EVENTS_CEILING,
        MAX_QUEUED_OUTPUT_BYTES_CEILING,
        MAX_OUTPUT_CHUNK_BYTES_CEILING,
        Duration::from_secs(1),
        Duration::from_secs(1)
    )
    .is_ok());
}

#[test]
fn oversized_requests_are_refused_before_any_work() {
    let mut request = ExecutionRequest::new("x");
    request.argv = vec!["a".to_string(); MAX_ARGV_ENTRIES + 1];
    assert!(request.validate().is_err(), "argv count bound");

    let mut request = ExecutionRequest::new("x");
    request.env = vec![("k".to_string(), "v".to_string()); MAX_ENV_ENTRIES + 1];
    assert!(request.validate().is_err(), "env count bound");

    let mut request = ExecutionRequest::new("x");
    request.stdin = ExecutionStdin::Bytes(vec![0; MAX_STDIN_BYTES_CEILING + 1]);
    assert!(request.validate().is_err(), "stdin bound");

    assert!(
        ExecutionRequest::new("").validate().is_err(),
        "an empty executable is not a request"
    );
    let long = "a".repeat(MAX_ARG_BYTES + 1);
    assert!(ExecutionRequest::new("x").arg(long).validate().is_err());
}

#[test]
fn transport_types_round_trip_and_refuse_hostile_payloads() {
    let request = ExecutionRequest::new("/bin/echo")
        .arg("hi")
        .env("K", "V")
        .stdin(ExecutionStdin::Bytes(b"in".to_vec()));
    let json = serde_json::to_string(&request).expect("serialize");
    let back: ExecutionRequest = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(request, back);

    let limits = ExecutionLimits::default();
    let back: ExecutionLimits =
        serde_json::from_str(&serde_json::to_string(&limits).expect("ser")).expect("de");
    assert_eq!(limits, back);

    // Hostile: a limit above the repository ceiling must not survive
    // deserialization into a value the stream would not enforce.
    let hostile = serde_json::json!({
        "max_queued_events": MAX_QUEUED_EVENTS_CEILING + 1,
        "max_queued_output_bytes": 4096,
        "max_output_chunk_bytes": 256,
        "cancel_grace_ms": 1000,
        "reap_grace_ms": 1000,
    });
    assert!(
        serde_json::from_value::<ExecutionLimits>(hostile).is_err(),
        "an over-ceiling limit must fail deserialization"
    );

    // Hostile: an oversized argv is rejected as the offending element is seen.
    let hostile = serde_json::json!({
        "executable": "x",
        "argv": vec!["a"; MAX_ARGV_ENTRIES + 1],
    });
    assert!(serde_json::from_value::<ExecutionRequest>(hostile).is_err());

    // Terminal/evidence round-trips.
    let terminal = ExecutionTerminal::Denied {
        denial: Denial {
            kind: DenialKind::Exec,
            target: "nope".to_string(),
            reason: "outside grant".to_string(),
        },
    };
    let back: ExecutionTerminal =
        serde_json::from_str(&serde_json::to_string(&terminal).expect("ser")).expect("de");
    assert_eq!(terminal, back);
}

/// Every lifecycle proof above drives a real tree through `/bin/sh`, so those
/// tests are Unix-only by construction. That must not leave Windows silently
/// unstated: this asserts the containment grade the platform actually claims,
/// and runs everywhere.
///
/// Windows reaches descendants with `taskkill /T` — a real tree kill, but not
/// job-object containment, so a descendant that re-parents itself can escape.
/// An equivalent Windows-native grandchild-reap proof is a known residual; the
/// contract reports `PlatformTreeKill` rather than claiming process-group
/// equivalence it does not have.
#[test]
fn the_platform_reports_the_tree_containment_it_actually_provides() {
    let containment = local_tree_containment();
    #[cfg(unix)]
    assert_eq!(
        containment,
        LocalTreeContainment::ProcessGroup,
        "Unix executions lead their own process group"
    );
    #[cfg(all(not(unix), windows))]
    assert_eq!(
        containment,
        LocalTreeContainment::PlatformTreeKill,
        "Windows uses taskkill /T, which is not job-object containment"
    );
    #[cfg(all(not(unix), not(windows)))]
    assert_eq!(containment, LocalTreeContainment::DirectChildOnly);
}

/// A reaper that dies without publishing a result must not leave `wait()`
/// blocked forever.
///
/// `wait()` is the one call with no timeout in its contract, so "the owner
/// died" has to become a terminal rather than an indefinite block. The
/// guard publishes `Failed`; `publish` keeps the first terminal, so this
/// can never mask a real result.
#[test]
fn an_owner_that_dies_without_a_result_still_produces_a_terminal() {
    let control = Arc::new(super::local::LocalControl::for_test());
    let (sink, handle) = execution_stream(
        ExecutionId::next(),
        ExecutionLimits::default(),
        Arc::clone(&control) as Arc<_>,
    );
    sink.accepted().expect("accepted");

    // Simulate the owner thread vanishing mid-flight.
    {
        let _guard = super::local::TerminalGuard::for_test(sink.clone(), control);
    }

    let terminal = handle.wait().expect("wait must not block forever");
    assert!(
        matches!(terminal, ExecutionTerminal::Failed { .. }),
        "a vanished owner must surface as Failed, not as a hang: {terminal:?}"
    );
}

/// The fallback terminal must never overwrite a real one.
#[test]
fn the_fallback_terminal_never_masks_a_real_result() {
    let control = Arc::new(super::local::LocalControl::for_test());
    let (sink, handle) = execution_stream(
        ExecutionId::next(),
        ExecutionLimits::default(),
        Arc::clone(&control) as Arc<_>,
    );
    sink.accepted().expect("accepted");
    let real = ExecutionTerminal::Denied {
        denial: Denial {
            kind: DenialKind::Exec,
            target: "nope".to_string(),
            reason: "outside grant".to_string(),
        },
    };
    control.publish(&sink, real.clone()).expect("publish");
    {
        let _guard = super::local::TerminalGuard::for_test(sink.clone(), control);
    }
    assert_eq!(
        handle.wait().expect("terminal"),
        real,
        "the guard's fallback must not replace a published result"
    );
}

// ── real processes: POSIX only ──────────────────────────────────────────────
//
// Every proof below drives an actual process tree through `/bin/sh`, and the
// grandchild/reap proofs depend on POSIX process groups. Rather than let them
// compile everywhere and silently no-op off Unix — which would read as coverage
// this repository does not have — the whole harness is gated, and
// `the_platform_reports_the_tree_containment_it_actually_provides` above states
// the Windows posture explicitly.

#[cfg(unix)]
mod real_process {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::super::*;
    use crate::{Caveats, Gate, Scope, Tool, ToolContext, ToolResult};

    // ── harness ─────────────────────────────────────────────────────────────────

    fn ctx(granted: Caveats) -> ToolContext {
        struct AnyTool;
        #[async_trait::async_trait]
        impl Tool for AnyTool {
            fn name(&self) -> &str {
                "any"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn invoke(
                &self,
                _a: serde_json::Value,
                _c: &ToolContext,
            ) -> ToolResult<serde_json::Value> {
                Ok(serde_json::Value::Null)
            }
        }
        Gate::new(0)
            .authorize(&AnyTool, &granted)
            .expect("authorize")
    }

    /// A grant that confines nothing, so these tests exercise the *lifecycle*
    /// rather than re-testing admission (which `spawn.rs` already covers).
    fn open_ctx() -> ToolContext {
        ctx(Caveats::top())
    }

    fn sh() -> Option<&'static str> {
        ["/bin/sh", "/usr/bin/sh"]
            .into_iter()
            .find(|p| Path::new(p).exists())
    }

    /// Run `script` under `/bin/sh -c`, or return `None` where no POSIX shell
    /// exists (Windows CI covers its own lifecycle legs separately).
    fn shell_request(script: &str) -> Option<ExecutionRequest> {
        Some(ExecutionRequest::new(sh()?).arg("-c").arg(script))
    }

    /// A unique scratch directory whose path is **shell-safe**.
    ///
    /// Deliberately not built from `ThreadId`: its `Debug` form is `ThreadId(5)`,
    /// and interpolating parentheses into a `sh -c` script is a syntax error that
    /// kills the child instantly. That failure mode is silent — a test that then
    /// asserts "the marker never appeared" passes without ever having exercised the
    /// tree it meant to reap.
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bridle-exec-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        // The property is about interpolation into a `sh -c` script, so it is
        // asserted only where that interpolation happens. A Windows temp path
        // legitimately contains `\\`, `:`, and `~`.
        #[cfg(unix)]
        assert!(
            dir.to_string_lossy()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)),
            "the scratch path must be safe to interpolate into a shell script: {dir:?}"
        );
        dir
    }

    /// Drain events until the terminal, returning every event in order.
    fn drain(handle: &mut ExecutionHandle) -> Vec<ExecutionEvent> {
        let mut events = Vec::new();
        while let Some(event) = handle.next_event() {
            let terminal = event.is_terminal();
            events.push(event);
            if terminal {
                break;
            }
        }
        events
    }

    fn text(events: &[ExecutionEvent], want_stdout: bool) -> String {
        let mut out = Vec::new();
        for event in events {
            match (&event.kind, want_stdout) {
                (ExecutionEventKind::Stdout(b), true) | (ExecutionEventKind::Stderr(b), false) => {
                    out.extend_from_slice(b);
                }
                _ => {}
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn backend() -> LocalExecutionBackend {
        LocalExecutionBackend::new()
    }

    // ── live streaming: output is observable BEFORE completion ──────────────────

    /// #370: "a real process writes `line 1`, waits, writes `line 2`, and exits;
    /// `line 1` is observed while the child is still running."
    ///
    /// The proof is temporal, not merely ordinal: `line 1` must be in hand while
    /// the child is provably still alive, which is why the child sleeps for a span
    /// far longer than the assertion's own tolerance.
    #[test]
    fn stdout_line_one_is_observable_while_the_child_still_runs() {
        let Some(request) = shell_request("echo line 1; sleep 3; echo line 2") else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let started = Instant::now();
        let mut handle = backend()
            .start(&open_ctx(), request)
            .expect("start the execution");

        let mut saw_line_one = None;
        while let Some(event) = handle.next_event() {
            if let ExecutionEventKind::Stdout(bytes) = &event.kind {
                if String::from_utf8_lossy(bytes).contains("line 1") {
                    saw_line_one = Some(started.elapsed());
                    break;
                }
            }
            assert!(!event.is_terminal(), "terminal arrived before `line 1`");
        }

        let elapsed = saw_line_one.expect("`line 1` must reach the consumer");
        assert!(
            elapsed < Duration::from_secs(2),
            "`line 1` must arrive while the child sleeps, not after it exits: {elapsed:?}"
        );

        let terminal = handle.wait().expect("terminal");
        assert!(terminal.is_exited(), "unexpected terminal: {terminal:?}");
        assert!(
            started.elapsed() >= Duration::from_secs(3),
            "the child really did keep running after `line 1`"
        );
    }

    /// The same temporal proof, independently, for stderr.
    #[test]
    fn stderr_is_observable_while_the_child_still_runs() {
        let Some(request) = shell_request("echo err 1 1>&2; sleep 3; echo err 2 1>&2") else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let started = Instant::now();
        let mut handle = backend()
            .start(&open_ctx(), request)
            .expect("start the execution");

        let mut seen = None;
        while let Some(event) = handle.next_event() {
            if let ExecutionEventKind::Stderr(bytes) = &event.kind {
                if String::from_utf8_lossy(bytes).contains("err 1") {
                    seen = Some(started.elapsed());
                    break;
                }
            }
            assert!(!event.is_terminal(), "terminal arrived before `err 1`");
        }

        let elapsed = seen.expect("`err 1` must reach the consumer");
        assert!(
            elapsed < Duration::from_secs(2),
            "stderr must stream live, not replay after exit: {elapsed:?}"
        );
        assert!(handle.wait().expect("terminal").is_exited());
    }

    // ── ordering, sequencing, and the single terminal ───────────────────────────

    /// Sequence numbers are strictly increasing, `Accepted` precedes `Started`, and
    /// the terminal is last and unique.
    #[test]
    fn sequence_is_strictly_increasing_and_the_terminal_is_last() {
        let Some(request) = shell_request("echo a; echo b 1>&2; echo c") else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let mut handle = backend().start(&open_ctx(), request).expect("start");
        let events = drain(&mut handle);

        assert!(
            events.len() >= 3,
            "expected a real event stream: {events:?}"
        );
        for pair in events.windows(2) {
            assert!(
                pair[1].sequence > pair[0].sequence,
                "sequence must strictly increase: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
        assert!(matches!(events[0].kind, ExecutionEventKind::Accepted));
        assert!(
            matches!(events[1].kind, ExecutionEventKind::Started { .. }),
            "Started must follow Accepted: {:?}",
            events[1]
        );

        let terminals = events.iter().filter(|e| e.is_terminal()).count();
        assert_eq!(terminals, 1, "exactly one terminal event");
        assert!(
            events.last().expect("non-empty").is_terminal(),
            "the terminal must follow all delivered output"
        );
        assert_eq!(text(&events, true).matches('a').count(), 1);
        assert!(text(&events, false).contains('b'));
    }

    /// #370: "repeated `wait` returns the same terminal result."
    #[test]
    fn repeated_wait_returns_identical_final_evidence() {
        let Some(request) = shell_request("echo done; exit 7") else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let handle = backend().start(&open_ctx(), request).expect("start");
        let first = handle.wait().expect("first wait");
        let second = handle.wait().expect("second wait");
        let third = handle.wait().expect("third wait");
        assert_eq!(first, second, "wait must be idempotent");
        assert_eq!(second, third, "wait must be idempotent");
        assert_eq!(
            first.exit_evidence().expect("exited").code,
            Some(7),
            "the cached terminal keeps the real exit code"
        );
    }

    // ── the fence identity actually crosses the seam ────────────────────────────

    /// #370: "started/terminal evidence retains the applied `AdmittedFenceId`."
    ///
    /// The point is not that *some* id is present but that the id in the evidence
    /// is the very object the spawn funnel admitted — so `Started` and the terminal
    /// must agree, and the id must not be a placeholder.
    #[test]
    fn started_and_final_evidence_carry_the_same_applied_fence_identity() {
        let Some(request) = shell_request("true") else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let mut handle = backend().start(&open_ctx(), request).expect("start");
        let events = drain(&mut handle);

        let started_fence = events
            .iter()
            .find_map(|e| match &e.kind {
                ExecutionEventKind::Started { fence, .. } => Some(fence.clone()),
                _ => None,
            })
            .expect("a Started event carrying fence evidence");

        let terminal = handle.wait().expect("terminal");
        let evidence = terminal.exit_evidence().expect("exited");
        assert_eq!(
            started_fence, evidence.fence,
            "the terminal must name the same fence the start did"
        );
        assert_eq!(started_fence.sandbox_kind, evidence.fence.sandbox_kind);
        // A real content-addressed id, not a default/placeholder.
        assert!(
            !started_fence.fence_id.0.to_string().is_empty(),
            "fence id must be the admitted object's real CID"
        );
    }

    /// Two executions under the *same* grant admit the same fence, and the id is a
    /// function of the admitted authority — not of the pid or a counter.
    #[test]
    fn fence_identity_is_content_addressed_not_per_process() {
        let Some(a) = shell_request("true") else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let b = shell_request("true").expect("shell");
        let cx = open_ctx();
        let first = backend().start(&cx, a).expect("start");
        let second = backend().start(&cx, b).expect("start");
        let one = first.wait().expect("terminal");
        let two = second.wait().expect("terminal");
        assert_eq!(
            one.exit_evidence().expect("exited").fence.fence_id,
            two.exit_evidence().expect("exited").fence.fence_id,
            "equal admitted authority ⇒ equal fence id"
        );
    }

    // ── authority: denial spawns nothing ────────────────────────────────────────

    /// #370: "a pre-spawn authority denial emits no `Started` event and spawns
    /// nothing."
    #[test]
    fn pre_spawn_denial_emits_no_started_and_launches_no_child() {
        let marker = scratch("denial").join("must-never-exist");
        let _ = std::fs::remove_file(&marker);

        // An exec grant that does not admit the program: admission must refuse
        // before the funnel looks anything up, let alone spawns it.
        let granted = Caveats {
            exec: Scope::Only(["definitely-not-this".to_string()].into_iter().collect()),
            ..Caveats::top()
        };
        let Some(shell) = sh() else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let request = ExecutionRequest::new(shell)
            .arg("-c")
            .arg(format!("touch '{}'", marker.display()));

        let mut handle = backend().start(&ctx(granted), request).expect("start");
        let events = drain(&mut handle);

        assert!(
            matches!(events[0].kind, ExecutionEventKind::Accepted),
            "the stream still opens with Accepted"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, ExecutionEventKind::Started { .. })),
            "a denied execution must emit no Started: {events:?}"
        );
        let terminal = handle.wait().expect("terminal");
        assert!(
            matches!(terminal, ExecutionTerminal::Denied { .. }),
            "expected a Denied terminal, got {terminal:?}"
        );

        // The strongest form of the claim: nothing ran.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !marker.exists(),
            "a denied execution must not have spawned anything"
        );
    }

    // ── process-tree ownership ──────────────────────────────────────────────────

    /// #370: "cancel kills and reaps a real child *and grandchild*; a delayed
    /// marker never appears."
    ///
    /// The grandchild is the load-bearing part: signalling only the direct child
    /// leaves it running, and it would then create the marker.
    #[test]
    fn cancel_reaps_the_child_and_its_grandchild() {
        let dir = scratch("cancel");
        let marker = dir.join("grandchild-marker");
        let _ = std::fs::remove_file(&marker);
        let Some(request) = shell_request(&format!(
            "( sleep 2; touch '{}' ) & echo ready; sleep 30",
            marker.display()
        )) else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };

        let mut handle = backend()
            .start(&open_ctx(), request.limits(short_grace()))
            .expect("start");

        // Wait until the tree is genuinely up before cancelling.
        let mut ready = false;
        while let Some(event) = handle.next_event() {
            if let ExecutionEventKind::Stdout(b) = &event.kind {
                if String::from_utf8_lossy(b).contains("ready") {
                    ready = true;
                    break;
                }
            }
        }
        assert!(ready, "the tree must have started");

        let cancelled_at = Instant::now();
        handle.cancel().expect("cancel");
        let terminal = handle.wait().expect("terminal");
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(20),
            "cancel must not wait out the child's own 30s sleep"
        );
        assert!(
            !matches!(terminal, ExecutionTerminal::Failed { .. }),
            "cancel produced a mechanism failure: {terminal:?}"
        );

        // The grandchild's delayed marker must never appear.
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            !marker.exists(),
            "a cancelled execution left a live grandchild behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `kill` is prompt and tree-wide.
    #[test]
    fn kill_is_prompt_and_tree_wide() {
        let dir = scratch("kill");
        let marker = dir.join("grandchild-marker");
        let _ = std::fs::remove_file(&marker);
        let Some(request) = shell_request(&format!(
            "( sleep 2; touch '{}' ) & echo ready; sleep 30",
            marker.display()
        )) else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };

        let mut handle = backend()
            .start(&open_ctx(), request.limits(short_grace()))
            .expect("start");
        while let Some(event) = handle.next_event() {
            if matches!(&event.kind, ExecutionEventKind::Stdout(b) if String::from_utf8_lossy(b).contains("ready"))
            {
                break;
            }
        }

        let killed_at = Instant::now();
        handle.kill().expect("kill");
        let _ = handle.wait().expect("terminal");
        assert!(
            killed_at.elapsed() < Duration::from_secs(5),
            "kill must be immediate, not grace-delayed: {:?}",
            killed_at.elapsed()
        );

        std::thread::sleep(Duration::from_secs(3));
        assert!(!marker.exists(), "kill must be tree-wide");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #370: "drop/disconnect behavior is explicit; the default must not detach a
    /// child." Dropping the handle terminates *and joins* the tree.
    #[test]
    fn dropping_the_handle_does_not_detach_the_tree() {
        let dir = scratch("drop");
        let marker = dir.join("grandchild-marker");
        let _ = std::fs::remove_file(&marker);
        let Some(request) = shell_request(&format!(
            "( sleep 2; touch '{}' ) & echo ready; sleep 30",
            marker.display()
        )) else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };

        {
            let mut handle = backend()
                .start(&open_ctx(), request.limits(short_grace()))
                .expect("start");
            while let Some(event) = handle.next_event() {
                if matches!(&event.kind, ExecutionEventKind::Stdout(b) if String::from_utf8_lossy(b).contains("ready"))
                {
                    break;
                }
            }
            // Drop without observing the terminal.
        }

        // `abandon` joined the reaper, so by the time the drop returned the tree
        // was already gone — the marker cannot appear afterwards.
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            !marker.exists(),
            "a dropped handle detached a live grandchild"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #370 / #373-C: "a descendant holding inherited stdout/stderr cannot create a
    /// false terminal."
    ///
    /// The direct child exits immediately while a background descendant keeps the
    /// inherited pipe writers open. A naive owner either blocks forever on EOF or
    /// detaches the drain and publishes a terminal that claims completion while an
    /// output producer is still alive. The managed owner instead force-reaps the
    /// tree it owns and *says so* in the evidence.
    #[test]
    fn a_descendant_holding_the_pipes_cannot_create_a_false_terminal() {
        let Some(request) = shell_request("sleep 30 & echo parent-done") else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let started = Instant::now();
        let handle = backend()
            .start(&open_ctx(), request.limits(short_grace()))
            .expect("start");

        let terminal = handle.wait().expect("terminal");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(20),
            "the owner must not wait out the descendant's 30s sleep: {elapsed:?}"
        );
        let evidence = terminal.exit_evidence().expect("exited");
        assert_eq!(
            evidence.disposition,
            ExitDisposition::DescendantsReaped,
            "the terminal must record that descendants had to be reaped, not claim a clean natural exit"
        );
        drop(handle);
    }

    // ── bounded buffering with exact drop accounting ────────────────────────────

    /// #370: "a stalled consumer keeps memory within the configured bound and
    /// produces explicit truncation/backpressure evidence."
    ///
    /// The consumer never reads until the execution is over. The child produces far
    /// more output than the queue may hold, so the bound is exercised for real: the
    /// child must not wedge (its pipe keeps draining), the queue must not grow past
    /// its configured count, and the omitted bytes must be reported exactly rather
    /// than as a boolean.
    #[test]
    fn a_stalled_consumer_stays_bounded_and_reports_exact_dropped_bytes() {
        // 64 events of at most 256 bytes, and 4 KiB of queued output.
        let limits = ExecutionLimits::new(
            64,
            4096,
            256,
            Duration::from_secs(5),
            Duration::from_millis(500),
        )
        .expect("within ceilings");

        // ~512 KiB of stdout — orders of magnitude past the queue bound.
        let Some(request) = shell_request(
            "i=0; while [ $i -lt 512 ]; do \
             printf '%01024d' $i; i=$((i+1)); done",
        ) else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };

        let mut handle = backend()
            .start(&open_ctx(), request.limits(limits))
            .expect("start");

        // The stall: do not consume a single event until the child is finished.
        let terminal = handle
            .wait()
            .expect("the child must not wedge on a full queue");
        let evidence = terminal.exit_evidence().expect("exited");

        assert!(
            evidence.dropped.stdout_bytes > 0,
            "the bound must actually have been hit"
        );
        assert_eq!(
            evidence.dropped.stderr_bytes, 0,
            "nothing was written to stderr"
        );
        assert!(
            evidence.dropped.events > 0,
            "dropped events must be counted, not merely flagged"
        );

        // Now drain and check the *physical* bound held.
        let events = drain(&mut handle);
        let queued_output: usize = events
            .iter()
            .filter_map(|e| match &e.kind {
                ExecutionEventKind::Stdout(b) | ExecutionEventKind::Stderr(b) => Some(b.len()),
                _ => None,
            })
            .sum();
        assert!(
            queued_output <= limits.max_queued_output_bytes(),
            "queued {queued_output} bytes exceeds the {} byte bound",
            limits.max_queued_output_bytes()
        );
        // Count only the events the queue actually held back — the terminal and the
        // pre-start events ride outside the output bound.
        let held = events
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    ExecutionEventKind::Stdout(_)
                        | ExecutionEventKind::Stderr(_)
                        | ExecutionEventKind::OutputTruncated(_)
                )
            })
            .count();
        assert!(
            held <= limits.max_queued_events(),
            "queued {held} events exceeds the {} event bound",
            limits.max_queued_events()
        );

        // The truncation notice is on the stream, and the terminal survived a full
        // queue because it is held out of band.
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, ExecutionEventKind::OutputTruncated(_))),
            "a stalled stream must carry an explicit OutputTruncated notice"
        );
        assert!(
            events.last().expect("events").is_terminal(),
            "a full output queue must not be able to lose the terminal"
        );

        // Exactness: what arrived plus what was dropped is what the child wrote.
        let total = queued_output as u64 + evidence.dropped.stdout_bytes;
        assert_eq!(
            total,
            512 * 1024,
            "dropped-byte accounting must be exact, not approximate"
        );
    }

    fn short_grace() -> ExecutionLimits {
        ExecutionLimits::new(
            4096,
            1024 * 1024,
            64 * 1024,
            Duration::from_millis(200),
            Duration::from_millis(300),
        )
        .expect("within ceilings")
    }

    // ── limits are refused, not clamped ─────────────────────────────────────────

    // ── transport shape ─────────────────────────────────────────────────────────

    // ── stdin ───────────────────────────────────────────────────────────────────

    #[test]
    fn explicit_stdin_bytes_reach_the_child_and_then_eof() {
        let Some(request) = shell_request("cat") else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let mut handle = backend()
            .start(
                &open_ctx(),
                request.stdin(ExecutionStdin::Bytes(b"hello stdin\n".to_vec())),
            )
            .expect("start");
        let events = drain(&mut handle);
        assert!(
            text(&events, true).contains("hello stdin"),
            "stdin must reach the child and the child must see EOF so `cat` exits"
        );
    }

    // ── async host ──────────────────────────────────────────────────────────────

    /// #370 acceptance: Tokio integration must not block the runtime reactor.
    ///
    /// A single-worker runtime is the sharpest form of the claim: if starting an
    /// execution parked any blocking work on the reactor, the concurrently spawned
    /// task could not make progress while the child ran.
    #[cfg(feature = "spawn-tokio")]
    #[test]
    fn starting_an_execution_does_not_block_the_tokio_reactor() {
        let Some(request) = shell_request("sleep 1; echo done") else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        runtime.block_on(async {
            let handle = backend().start(&open_ctx(), request).expect("start");
            // The reactor must stay live while the child runs: this timer only
            // fires if nothing blocking was parked on the single worker thread.
            let ticks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = std::sync::Arc::clone(&ticks);
            let ticker = tokio::spawn(async move {
                for _ in 0..10 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });
            ticker.await.expect("the reactor kept running");
            assert_eq!(
                ticks.load(std::sync::atomic::Ordering::Relaxed),
                10,
                "the reactor must not have been blocked by the execution"
            );
            // The blocking wait belongs off-reactor, which is exactly what the
            // owner's dedicated threads make possible.
            let terminal = tokio::task::spawn_blocking(move || handle.wait())
                .await
                .expect("join")
                .expect("terminal");
            assert!(terminal.is_exited());
        });
    }

    // ── proxy quiescence (#374 integration) ─────────────────────────────────────

    /// #374: a proxy that could not be finalized must become `Failed`, never an
    /// apparently successful exit with provisional egress evidence.
    ///
    /// Proved against the pure composer so it holds on every platform, including
    /// the many hosts where the egress proxy never engages at all.
    #[test]
    fn proxy_finalization_failure_becomes_execution_failure() {
        // A *real* admitted fence identity, taken from an actual execution — the
        // composer is being proved against the same evidence shape production uses.
        let Some(probe) = shell_request("true") else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let fence = backend()
            .start(&open_ctx(), probe)
            .expect("start")
            .wait()
            .expect("terminal")
            .exit_evidence()
            .expect("exited")
            .fence
            .clone();

        let failed = super::local::compose_terminal(
            Ok((Some(0), None)),
            None,
            Some("workers did not join".to_string()),
            ExitDisposition::Natural,
            fence.clone(),
            DroppedEvidence::default(),
        );
        assert!(
            matches!(failed, ExecutionTerminal::Failed { .. }),
            "an unfinalized proxy must not report a successful exit: {failed:?}"
        );
        assert!(
            failed.exit_evidence().is_none(),
            "a failed finalization exposes no exit evidence at all"
        );

        let ok = super::local::compose_terminal(
            Ok((Some(0), None)),
            None,
            None,
            ExitDisposition::Natural,
            fence,
            DroppedEvidence::default(),
        );
        assert!(ok.is_exited());
    }

    /// What a proxy-backed execution does on this host — proved, not assumed.
    ///
    /// The egress proxy engages only where the backend can kernel-fence the child
    /// to loopback (`egress_proxy_plan`). Where it does engage, admission may still
    /// refuse: the macOS Seatbelt projection resolves EVERY restricted `net` scope
    /// as `Unknown`, including the loopback-fenced scope the proxy plan itself
    /// produces, and an `Unknown` axis is refused before spawn pending the
    /// deputy-complete native proof (E3/E4).
    ///
    /// So there are exactly two legal outcomes today, and this test names which one
    /// it observed instead of asserting a proof the host cannot run:
    ///
    /// 1. the plan does not engage (Landlock: cannot address-fence) — nothing to
    ///    prove here, and the child is honestly unproxied;
    /// 2. the plan engages and admission refuses — the path fails CLOSED, which is
    ///    the property that actually matters when the fence cannot be proven;
    /// 3. the plan engages and admission succeeds — then, and only then, the
    ///    terminal must carry FROZEN proxy evidence, which is what proves
    ///    `shutdown_and_join` ran before the terminal became observable.
    ///
    /// The end-to-end quiescence proof is therefore currently reachable only on a
    /// host where a loopback-fenced scope resolves bounded. The rule that a failed
    /// finalization becomes `Failed` is proved unconditionally above, against the
    /// pure composer.
    #[test]
    fn a_proxy_backed_execution_either_refuses_closed_or_proves_quiescence() {
        let granted = Caveats {
            net: Scope::Only(["example.com".to_string()].into_iter().collect()),
            ..Caveats::top()
        };
        let policy = Arc::new(crate::SandboxPolicy::default());
        if crate::egress_proxy_plan(&granted, &policy).is_none() {
            eprintln!(
                "outcome 1: this host's backend cannot kernel-fence loopback, so the \
                 egress proxy stays inert by design (egress_proxy_plan == None)"
            );
            return;
        }

        let Some(request) = shell_request("echo hi") else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let mut handle = backend().start(&ctx(granted), request).expect("start");
        let events = drain(&mut handle);
        let started = events.iter().find_map(|e| match &e.kind {
            ExecutionEventKind::Started { fence, .. } => Some(fence.clone()),
            _ => None,
        });
        let terminal = handle.wait().expect("terminal");

        match started {
            None => {
                // Outcome 2: the fence could not be proven, so nothing ran. The
                // failure mode this guards against is the opposite — a child that
                // runs unproxied while the caller believes a fence is in force.
                assert!(
                    matches!(terminal, ExecutionTerminal::Denied { .. }),
                    "a proxy-backed execution that never started must be an explicit \
                     denial, not a silent success: {terminal:?}"
                );
                eprintln!(
                    "outcome 2: the proxy plan engaged but admission refused the \
                     loopback-fenced scope — the path fails closed"
                );
            }
            Some(started) => {
                // Outcome 3: the real end-to-end proof.
                assert!(
                    started.egress_proxied,
                    "the plan engaged, so the child must be proxied"
                );
                let evidence = terminal.exit_evidence().expect("exited");
                assert!(
                    evidence.proxy.is_some(),
                    "a proxied execution's terminal must carry FROZEN proxy evidence — \
                     its presence is what proves shutdown_and_join ran before the terminal"
                );
                assert_eq!(evidence.fence, started);
                eprintln!("outcome 3: proxy quiescence proved end to end");
            }
        }
    }

    // ── platform containment, stated rather than assumed ────────────────────────
    // ── owner hardening: no hang, no leak, no wedge ─────────────────────────

    /// A consumer that never reads, with a child that writes far more than the
    /// queue holds, must still run to completion — the drainer keeps emptying
    /// the pipe even when the sink refuses every chunk.
    ///
    /// This is the wedge the bounded queue exists to prevent: a drainer that
    /// stopped reading on a sink fault would let the child block on a full pipe
    /// buffer and never exit, turning a reporting problem into a hang.
    #[test]
    fn a_child_never_wedges_on_a_full_queue() {
        let limits = ExecutionLimits::new(
            1,
            64,
            64,
            Duration::from_secs(5),
            Duration::from_millis(500),
        )
        .expect("within ceilings");
        let Some(request) =
            shell_request("i=0; while [ $i -lt 256 ]; do printf '%01024d' $i; i=$((i+1)); done")
        else {
            eprintln!("skipped: no POSIX shell on this host");
            return;
        };
        let started = Instant::now();
        let handle = backend()
            .start(&open_ctx(), request.limits(limits))
            .expect("start");
        let terminal = handle.wait().expect("the child must not wedge");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the child wedged on a full queue: {:?}",
            started.elapsed()
        );
        let evidence = terminal.exit_evidence().expect("exited");
        assert_eq!(evidence.code, Some(0));
        assert!(
            evidence.dropped.stdout_bytes > 0,
            "the tiny queue must have forced drops"
        );
    }
}
