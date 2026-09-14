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
