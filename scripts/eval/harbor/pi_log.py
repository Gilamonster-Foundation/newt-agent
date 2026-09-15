"""pi_log.py — how a pi `--print --mode json` run ended, read from its log (#2318).

Stdlib only, so both the Harbor agent and the campaign ingest (plain python3)
can use it. pi 0.85.1 exits 0 even when every model call failed: against an
unreachable endpoint it retries three times, logs
`auto_retry_end success:false`, and leaves every assistant message at
`stopReason: error` with zero usage. Harbor would then grade the untouched
workspace as the model's failure.
"""

from __future__ import annotations

import json


def records(lines):
    for line in lines:
        line = line.strip()
        if line.startswith("{"):
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


# ── pi's session JSONL ───────────────────────────────────────────────────────
# Harbor runs `pi … | grep -v message_update | stdbuf -oL tee pi.txt`: stdbuf
# applies only to tee, so grep block-buffers, and a KILLED trial's pi.txt loses up
# to 4 KiB of its final events (a 2026-09-14 trial stopped at exactly 81920 bytes,
# mid-line). pi's own session file (--session-dir) is unbuffered: it decides the
# last events. pi.txt is whole after a normal exit and still carries the
# auto-retry events the session file does not.


def session_messages(lines):
    return [o["message"] for o in records(lines) if o.get("type") == "message" and isinstance(o.get("message"), dict)]


def _calls_a_tool(message):
    return any(isinstance(c, dict) and c.get("type") == "toolCall" for c in message.get("content") or [])


def session_awaiting(lines) -> str | None:
    """What pi was waiting on when its session ended: the model (the last entry is a
    tool result or the task), its own tool (an assistant tool call with no result),
    or nothing (a finished assistant turn)."""
    messages = session_messages(lines)
    if not messages:
        return None
    last = messages[-1]
    if last.get("role") in ("toolResult", "user"):
        return "model"
    return "tool" if last.get("role") == "assistant" and _calls_a_tool(last) else None


def txt_awaiting(lines) -> str | None:
    """The same question from pi.txt events. Only trustworthy when the file is whole."""
    state = None
    for o in records(lines):
        kind, message = o.get("type"), o.get("message") or {}
        if kind == "message_start" and message.get("role") == "assistant":
            state = "model"
        elif kind == "message_end" and message.get("role") == "assistant":
            state = "tool" if _calls_a_tool(message) else None
        elif kind == "tool_execution_start":
            state = "tool"
        elif kind == "tool_execution_end":
            state = "model"
    return state


def pi_awaiting(txt_lines, session_lines) -> str | None:
    return session_awaiting(session_lines) if session_lines else txt_awaiting(txt_lines)


def pi_session_claim(lines):
    """pi's done-claim from its session: the run ended on an assistant turn whose
    stopReason is `stop`. None when the session did not end on an assistant turn."""
    messages = session_messages(lines)
    if not messages or messages[-1].get("role") != "assistant":
        return None
    reason = messages[-1].get("stopReason")
    return reason == "stop", f"session last assistant stopReason={reason}"


def pi_inference_failure(txt_lines, session_lines) -> str | None:
    """inference_failure with the session file preferred for the final turn;
    pi.txt still decides auto-retry exhaustion, which only it records."""
    for o in records(txt_lines):
        if o.get("type") == "auto_retry_end" and o.get("success") is False:
            return f"auto_retry_end success=false: {o.get('finalError')}"
    if not session_lines:
        return inference_failure(txt_lines)
    assistant = [m for m in session_messages(session_lines) if m.get("role") == "assistant"]
    if not assistant:
        return "no assistant message"
    last = assistant[-1]
    if last.get("stopReason") in ("error", "aborted"):
        return f"final stopReason={last['stopReason']}: {last.get('errorMessage')}"
    return None


def inference_failure(lines) -> str | None:
    """Why the run never got a usable model reply, or None if it did."""
    last = None
    for o in records(lines):
        if o.get("type") == "auto_retry_end" and o.get("success") is False:
            return f"auto_retry_end success=false: {o.get('finalError')}"
        if o.get("type") == "message_end" and (o.get("message") or {}).get("role") == "assistant":
            last = o["message"]
    if last is None:
        return "no assistant message"
    if last.get("stopReason") in ("error", "aborted"):
        return f"final stopReason={last['stopReason']}: {last.get('errorMessage')}"
    return None
