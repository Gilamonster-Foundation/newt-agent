#!/usr/bin/env python3
"""AFTER SCRATCH HARNESS — B5/B6 run analyzer for the post-17/18/19 loop.

Not wired into CI or the workspace build.

Extends the baseline `b56_analyze.py` for the compression-v2 loop (18.4):
the old "mid-loop trim:" / "pre-send trim:" debug lines no longer exist;
visibility now means the `⧉ context compressed:` notice, the overflow
notice, the anti-thrash notice, or a refused send. Adds:

  - est_tokens of every /api/chat request under the CURRENT estimator
    (messages + tools, ceil-div — trim.rs::estimate_request_tokens) and the
    backend-evaluated count of the LARGEST-estimate request — the silent
    truncation ratio;
  - marker_in_last_reply: whether the ACTIVE TASK marker appears in the
    model's final displayed reply (the B6 "correct answer" score), parsed
    from the `▸` reply block before the turn footer;
  - visible-degradation counters: compressed_notices / overflow_notices /
    antithrash_notices / refused_sends (+ legacy trim counters, expect 0).

Usage:
    python3 b56_analyze_after.py --capture capture-b6-run1.jsonl \
        --session b6-run1.log --marker GAUNTLET-7f3d9c
"""

import argparse
import json
import math
import re


def estimate_value_tokens(value):
    s = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    return math.ceil(len(s) / 4)


def estimate_request_tokens(req):
    est = sum(estimate_value_tokens(m) for m in req.get("messages", []))
    if req.get("tools") is not None:
        est += estimate_value_tokens(req["tools"])
    return est


def reply_blocks(session_text):
    """Model reply blocks: from a `▸`(or `> `) line to the turn footer
    (`… · N in / M out · …`). Streamed tokens print raw, so everything
    between those anchors counts as reply text."""
    blocks = []
    current = None
    footer = re.compile(r"·\s+[\d,]+ in / [\d,]+ out")
    for line in session_text.splitlines():
        if current is None and (line.startswith("▸") or line.startswith(">  ")):
            current = [line]
        elif current is not None:
            if footer.search(line):
                blocks.append("\n".join(current))
                current = None
            else:
                current.append(line)
    if current:
        blocks.append("\n".join(current))
    return blocks


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
    biggest_est = 0
    biggest_est_evaluated = None
    for i, r in enumerate(chat):
        req = r.get("request") or {}
        body = json.dumps(req.get("messages", []))
        if i == len(chat) - 1:
            marker_in_last = args.marker in body
        pec = (r.get("response") or {}).get("prompt_eval_count") or (
            r.get("stream_final") or {}
        ).get("prompt_eval_count") or 0
        max_prompt = max(max_prompt, pec)
        est = estimate_request_tokens(req)
        if est > biggest_est:
            biggest_est = est
            biggest_est_evaluated = pec

    sess = open(args.session, errors="replace").read()
    blocks = reply_blocks(sess)
    last_reply = blocks[-1] if blocks else ""

    print(json.dumps({
        "capture": args.capture,
        "chat_requests": len(chat),
        "non_2xx": non2xx,
        "marker_in_last_request": marker_in_last,
        "max_prompt_eval_count": max_prompt,
        "biggest_request_est_tokens": biggest_est,
        "biggest_request_evaluated": biggest_est_evaluated,
        "marker_in_last_reply": args.marker in last_reply,
        "empty_response_msgs": sess.count("(model returned an empty response"),
        "error_lines": sum(
            1 for l in sess.splitlines() if l.strip().startswith("error:")
        ),
        # Legacy 18.4-removed paths — expect 0 forever; nonzero means drift.
        "mid_loop_trims": sess.count("mid-loop trim:"),
        "pre_send_trims": sess.count("pre-send trim:"),
        # Visible-degradation events (the B6 acceptance currency).
        "compressed_notices": sess.count("context compressed:"),
        "compression_debug_lines": sess.count("[debug] compression:"),
        "overflow_notices": sess.count("context overflow likely"),
        "antithrash_notices": sess.count("auto-compression is disabled"),
        "refused_sends": sess.count("exceeds the model's input budget"),
    }))


if __name__ == "__main__":
    main()
