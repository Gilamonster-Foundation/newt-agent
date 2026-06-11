#!/usr/bin/env python3
"""BASELINE SCRATCH HARNESS — B7 conversation-store seeder (issue #245).

Not wired into CI or the workspace build.

Generates N synthetic conversation records in a sandbox HOME, matching the
CURRENT `ConversationStore` JSON schema (newt-core/src/conversation.rs):

    $HOME/.newt/conversations/<workspace_id>/<id>.json

where workspace_id = UUIDv5(NAMESPACE_URL, canonicalized workspace path) —
the same derivation as `ConversationStore::workspace_id_for_path`.

Usage:
    python3 b7_seed_store.py --home /tmp/newt-bench/home-b7 \
        --workspace /tmp/newt-bench/ws-b7 --count 1000
"""

import argparse
import json
import os
import time
import uuid


def turn(i):
    user = f"turn {i}: please fix the failing test and re-run cargo test until green."
    assistant = ("Read the file, found the bug. " + "context survives across sessions; " * 40 +
                 f"tool_event: {{\"name\":\"run_command\",\"digest\":\"abcd{i:04}\"}}")
    return {"user": user, "assistant": assistant}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--home", required=True)
    ap.add_argument("--workspace", required=True)
    ap.add_argument("--count", type=int, required=True)
    ap.add_argument("--turns", type=int, default=10)
    args = ap.parse_args()

    ws = os.path.realpath(args.workspace)
    ws_id = str(uuid.uuid5(uuid.NAMESPACE_URL, ws.replace("\\", "/")))
    out_dir = os.path.join(args.home, ".newt", "conversations", ws_id)
    os.makedirs(out_dir, exist_ok=True)

    base_nanos = time.time_ns()
    for c in range(args.count):
        cid = f"{base_nanos + c}-{uuid.uuid4()}"
        record = {
            "id": cid,
            "title": f"synthetic conversation {c}",
            "workspace": ws,
            "workspace_id": ws_id,
            "turns": [turn(c * args.turns + t) for t in range(args.turns)],
            "created_at_unix_nanos": base_nanos + c,
            "updated_at_unix_nanos": base_nanos + c,
        }
        with open(os.path.join(out_dir, f"{cid}.json"), "w") as f:
            json.dump(record, f, indent=2)

    total = sum(
        os.path.getsize(os.path.join(out_dir, f)) for f in os.listdir(out_dir)
    )
    print(f"seeded {args.count} conversations ({args.turns} turns each) in {out_dir}: {total} bytes")


if __name__ == "__main__":
    main()
