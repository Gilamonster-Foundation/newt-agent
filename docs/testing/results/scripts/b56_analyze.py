#!/usr/bin/env python3
"""BASELINE SCRATCH HARNESS — B5/B6 run analyzer (issue #245).

Not wired into CI or the workspace build.

Reads one capture-proxy JSONL log + the matching scripted-session output log
and reports, per run:

  - number of /api/chat requests and any non-2xx statuses (hard failures),
  - whether the ACTIVE TASK marker string was still present in the LAST
    /api/chat request body (the manual `active_task_retained` check),
  - reported prompt_eval_count of the largest request (how far past num_ctx
    the session pushed),
  - count of "(model returned an empty response" lines, "error:" lines, and
    mid-loop / pre-send trim debug lines in the session log.

Usage:
    python3 b56_analyze.py --capture capture-b6-run1.jsonl \
        --session b6-run1.log --marker GAUNTLET-7f3d9c
"""

import argparse
import json


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--capture", required=True)
    ap.add_argument("--session", required=True)
    ap.add_argument("--marker", required=True)
    args = ap.parse_args()

    chat = []
    non2xx = []
    for line in open(args.capture):
        r = json.loads(line)
        if r.get("path") != "/api/chat":
            continue
        chat.append(r)
        if not (200 <= r.get("status", 0) < 300):
            non2xx.append(r.get("status"))

    marker_in_last = None
    max_prompt = 0
    marker_lost_at = None
    for i, r in enumerate(chat):
        body = json.dumps((r.get("request") or {}).get("messages", []))
        present = args.marker in body
        if present is False and marker_lost_at is None and i > 0:
            marker_lost_at = i
        if i == len(chat) - 1:
            marker_in_last = present
        pec = (r.get("response") or {}).get("prompt_eval_count") or (
            r.get("stream_final") or {}
        ).get("prompt_eval_count") or 0
        max_prompt = max(max_prompt, pec)

    sess = open(args.session, errors="replace").read()
    empty = sess.count("(model returned an empty response")
    errors = sum(1 for l in sess.splitlines() if l.strip().startswith("error:"))
    midloop = sess.count("mid-loop trim:")
    presend = sess.count("pre-send trim:")
    overflow_notice = sess.count("context overflow likely")

    print(json.dumps({
        "capture": args.capture,
        "chat_requests": len(chat),
        "non_2xx": non2xx,
        "marker_in_last_request": marker_in_last,
        "marker_first_lost_at_request": marker_lost_at,
        "max_prompt_eval_count": max_prompt,
        "empty_response_msgs": empty,
        "error_lines": errors,
        "mid_loop_trims": midloop,
        "pre_send_trims": presend,
        "overflow_notices": overflow_notice,
    }))


if __name__ == "__main__":
    main()
