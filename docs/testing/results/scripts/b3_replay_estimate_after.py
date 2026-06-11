#!/usr/bin/env python3
"""AFTER SCRATCH HARNESS — B3 token-estimate accuracy replay, 18.1 estimator.

Not wired into CI or the workspace build.

Replays the SAME pinned capture corpus the baseline used
(`context-baseline-f0f4f6e.md` kept the JSONL capture logs precisely so the
18.1 delta could be measured on identical request bodies), but recomputes
newt's estimate as the CURRENT fallback estimator does
(`newt-core/src/agentic/trim.rs` @ cf1aa3e):

    estimate_value_tokens(v)  = ceil(chars(compact_json(v)) / 4)   # per value
    estimate_request_tokens   = sum(estimate_value_tokens(m) for m in messages)
                              + estimate_value_tokens(tools)        # 18.1: schemas counted

vs the baseline estimator (floor-divide of the summed char count, schemas
NOT counted). Ground truth is unchanged: replay the body against the same
Ollama with stream:false, num_predict:1 and a cache-busting UUID prefixed to
the system message so `prompt_eval_count` covers the full prompt.

NOTE on what this measures: the 18.1 *fallback* path (first dispatch of a
turn, no backend report yet). Within a turn the loop anchors on the
backend-reported prompt tokens (`PromptTracker`), whose error is ~0 by
construction; the drift session (capture-b3-drift-after) measures that side.

Usage:
    python3 b3_replay_estimate_after.py --upstream https://REDACTED-HOST \
        --schema-cost /tmp/newt-bench/capture-b3*-*.jsonl
"""

import argparse
import copy
import hashlib
import json
import math
import ssl
import statistics
import urllib.request
import uuid


def estimate_value_tokens(value):
    """Replica of trim.rs::estimate_value_tokens: serde_json compact
    serialization, char count (not bytes), ceil-divided by 4."""
    s = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    return math.ceil(len(s) / 4)


def newt_estimate_after(messages, tools):
    """Replica of trim.rs::estimate_request_tokens (Step 18.1)."""
    est = sum(estimate_value_tokens(m) for m in messages)
    if tools is not None:
        est += estimate_value_tokens(tools)
    return est


def newt_estimate_baseline(messages):
    """The f0f4f6e estimator: floor((sum of per-message chars) / 4), no tools.
    (Baseline summed chars then //4 once per message — floor per message.)"""
    return sum(
        len(json.dumps(m, ensure_ascii=False, separators=(",", ":"))) for m in messages
    ) // 4


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
    ap.add_argument("--upstream", default="https://REDACTED-HOST")
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
            # Hash the FULL messages JSON (baseline harness fix #4).
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
            tools = b.get("tools")
            est = newt_estimate_after(b["messages"], tools)
            est_old = newt_estimate_baseline(b["messages"])
            resp = post(args.upstream, b)
            actual = resp.get("prompt_eval_count")
            if not actual:
                print(f"!! no prompt_eval_count for {log} body {i}")
                continue
            err = abs(est - actual) / actual
            rows.append((b["model"], len(b["messages"]), est, actual, err))
            print(
                f"{b['model']}  msgs={len(b['messages'])}  est={est}  "
                f"(baseline-est={est_old})  actual={actual}  "
                f"err={err:+.1%}  signed={(est - actual) / actual:+.1%}"
            )

            if args.schema_cost and i % 4 == 0 and tools is not None:
                b2 = copy.deepcopy(b)
                b2.pop("tools", None)
                b2["messages"][0]["content"] = (
                    f"[bench-{uuid.uuid4().hex}] " + body["messages"][0]["content"]
                )
                resp2 = post(args.upstream, b2)
                no_tools = resp2.get("prompt_eval_count")
                if no_tools:
                    est_schema = estimate_value_tokens(tools)
                    schema_rows.append(
                        (b["model"], actual, no_tools, actual - no_tools, est_schema)
                    )
                    print(
                        f"   schema cost: with_tools={actual} without={no_tools} "
                        f"delta={actual - no_tools} estimator_counts={est_schema}"
                    )

    print("\n## per-model error |est-actual|/actual (18.1 estimator, same corpus)")
    print("| model | n | median | p95 | mean signed (est-actual)/actual |")
    print("|---|---|---|---|---|")
    models = sorted({m for m, *_ in rows})
    for m in models:
        errs = sorted(e for mm, _, _, _, e in rows if mm == m)
        signed = [(est - act) / act for mm, _, est, act, _ in rows if mm == m]
        n = len(errs)
        p95 = errs[min(n - 1, round(0.95 * (n - 1)))]
        print(
            f"| {m} | {n} | {statistics.median(errs):.1%} | {p95:.1%} "
            f"| {statistics.mean(signed):+.1%} |"
        )

    if schema_rows:
        print("\n## tool-schema cost: measured vs what the 18.1 estimator adds")
        print("| model | prompt w/ tools | w/o tools | schema tokens | estimator adds |")
        print("|---|---|---|---|---|")
        for m, w, wo, d, e in schema_rows:
            print(f"| {m} | {w} | {wo} | {d} | {e} |")


if __name__ == "__main__":
    main()
