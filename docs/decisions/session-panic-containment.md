# Proposed: contain a failed session turn without terminating the terminal

Status: proposal; this change fixes the outline panic, not session supervision.

A truncated Rust outline can panic while counting lines at an interior UTF-8
byte. The session runs on the `newt-session` scoped thread. `run_chat` in
`newt-tui/src/chat.rs` drains the session channel and explicitly resumes a panic
returned by `session.join()`. This propagates the session failure into the
terminal owner. Existing input/sink panic guards do not contain an agent turn.

The smallest useful recovery boundary is the turn invocation around
`with_live_spill_watch` and `chat_complete_with_prompt_and_artifacts`, before
`surface.turn_ended()`. A `catch_unwind` there can translate an unwind into the
existing failed-turn result: display an error, persist observed tool effects,
mark the objective resumable, and return to operator input. Do not retry the
turn automatically: tools may already have written files or committed.

That boundary currently borrows mutable session-owned compression, memory,
MCP, permission, and attribution state. Blanket `AssertUnwindSafe` would assert
that all of those are reusable after an arbitrary partial mutation. Before
adding it, separate disposable turn state from persistent session state, audit
held locks for poisoning, and recreate the disposable state on unwind. Preserve
already-observed tool effects and attribution; do not pretend external actions
were rolled back. The terminal owner must stay alive while the failed turn's
spill watcher and cancellation guards are dropped.

A second, outer supervisor should convert any panic outside the turn boundary
into an explicit session-failed UI event, retire that session, and offer a new
session or reload of persisted history. Merely replacing `resume_unwind` with
an error return still exits `run_chat`; it does not meet the survival contract.
The panic hook must also avoid writing raw panic diagnostics through a live
presenter; render a bounded failure through its normal event path instead.

Acceptance tests should inject a panic during compaction after one recorded
tool effect, verify the failed-turn record and unchanged effect ledger, verify
that the terminal accepts the next operator input, and verify that an outer
session panic retires only that session. Run the presenter test on a real PTY.
This is a follow-up supervision change rather than an unaudited unwind-safety
assertion in the crash fix.
