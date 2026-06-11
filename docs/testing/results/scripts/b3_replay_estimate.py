#!/usr/bin/env python3
"""BASELINE SCRATCH HARNESS — B3 token-estimate accuracy replay (issue #245).

Not wired into CI or the workspace build.

Takes capture logs produced by ollama_capture_proxy.py during scripted live
`newt code` sessions, and for each captured /api/chat request body:

1. Recomputes newt's CURRENT token estimate exactly as
   `newt-tui/src/lib.rs::estimate_tokens` does:
       sum(chars(compact_json(message)) for message in messages) // 4
   (chars of the JSON serialization of each message, summed, floor-divided
   by 4 — tool schemas NOT counted, chat template NOT counted).

2. Replays the body (stream:false) directly against the upstream Ollama with
   a cache-busting UUID prefixed to the first message's content, so
   `prompt_eval_count` reports the FULL prompt token count instead of only
   the non-KV-cached suffix (verified: without the buster, a strictly larger
   follow-up request reports FEWER prompt tokens than its predecessor).
   The buster adds ~20 tokens to a multi-thousand-token prompt; the estimate
   is recomputed on the mutated body so both sides see the same input.

3. Reports per-model |est - actual| / actual (median, p95) plus the raw table.

Optionally (--schema-cost) also replays each Nth body with the `tools` array
removed, to measure how many prompt tokens the uncounted tool schemas cost.

Usage:
    python3 b3_replay_estimate.py --upstream https://gnuc-ollama.home.lab \
        --schema-cost capture-b3-*.jsonl
"""

import argparse
import copy
import hashlib
import json
import ssl
import statistics
import urllib.request
import uuid


def newt_estimate(messages):
    """Replica of newt-tui estimate_tokens(): serde_json compact serialization,
    char count (not bytes), summed across messages, floor-divided by 4.
    Key order doesn't affect length; serde_json doesn't \\u-escape non-ASCII,
    so ensure_ascii=False matches."""
    return sum(len(json.dumps(m, ensure_ascii=False, separators=(",", ":"))) for m in messages) // 4


def post(upstream, body):
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    req = urllib.request.Request(
        upstream.rstrip("/") + "/api/chat",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, context=ctx, timeout=600) as resp:
        return json.loads(resp.read())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("logs", nargs="+")
    ap.add_argument("--upstream", default="https://gnuc-ollama.home.lab")
    ap.add_argument("--schema-cost", action="store_true")
    ap.add_argument("--max-per-log", type=int, default=0, help="0 = all")
    args = ap.parse_args()

    rows = []
    schema_rows = []
    for log in args.logs:
        bodies = []
        seen = set()
        for line in open(log):
            r = json.loads(line)
            req = r.get("request") or {}
            if r.get("path") != "/api/chat" or req.get("stream") is not False:
                continue  # replay only the non-streaming probe rounds
            # Hash the FULL messages JSON: every request in a session shares a
            # multi-KB system-prompt prefix, so any truncated key collapses
            # all bodies into one.
            key = hashlib.sha256(
                json.dumps(req.get("messages", []), sort_keys=True).encode()
            ).hexdigest()
            if key in seen:
                continue
            seen.add(key)
            bodies.append(req)
        if args.max_per_log:
            bodies = bodies[: args.max_per_log]

        for i, body in enumerate(bodies):
            b = copy.deepcopy(body)
            b["stream"] = False
            # Cache-buster: unique prefix on the first (system) message.
            buster = f"[bench-{uuid.uuid4().hex}] "
            b["messages"][0]["content"] = buster + b["messages"][0]["content"]
            # Keep generation cost negligible; we only need prompt_eval_count.
            b.setdefault("options", {})["num_predict"] = 1
            est = newt_estimate(b["messages"])
            resp = post(args.upstream, b)
            actual = resp.get("prompt_eval_count")
            if not actual:
                print(f"!! no prompt_eval_count for {log} body {i}")
                continue
            err = abs(est - actual) / actual
            rows.append((b["model"], len(b["messages"]), est, actual, err))
            print(f"{b['model']}  msgs={len(b['messages'])}  est={est}  actual={actual}  err={err:+.1%}  signed={(est-actual)/actual:+.1%}")

            if args.schema_cost and i % 4 == 0:
                b2 = copy.deepcopy(b)
                b2.pop("tools", None)
                b2["messages"][0]["content"] = f"[bench-{uuid.uuid4().hex}] " + body["messages"][0]["content"]
                resp2 = post(args.upstream, b2)
                no_tools = resp2.get("prompt_eval_count")
                if no_tools:
                    schema_rows.append((b["model"], actual, no_tools, actual - no_tools))
                    print(f"   schema cost: with_tools={actual} without={no_tools} delta={actual - no_tools}")

    print("\n## per-model error |est-actual|/actual")
    print("| model | n | median | p95 | mean signed (est-actual)/actual |")
    print("|---|---|---|---|---|")
    models = sorted({m for m, *_ in rows})
    for m in models:
        errs = sorted(e for mm, _, _, _, e in rows if mm == m)
        signed = [(est - act) / act for mm, _, est, act, _ in rows if mm == m]
        n = len(errs)
        p95 = errs[min(n - 1, round(0.95 * (n - 1)))]
        print(f"| {m} | {n} | {statistics.median(errs):.1%} | {p95:.1%} | {statistics.mean(signed):+.1%} |")

    if schema_rows:
        print("\n## tool-schema cost (uncounted by the estimator today)")
        print("| model | prompt w/ tools | w/o tools | schema tokens |")
        print("|---|---|---|---|")
        for m, w, wo, d in schema_rows:
            print(f"| {m} | {w} | {wo} | {d} |")


if __name__ == "__main__":
    main()
