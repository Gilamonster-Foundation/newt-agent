#!/usr/bin/env python3
"""tb_heldout.py — the declared held-out Terminal-Bench measurement set (#2318).

    tb_heldout.py pool <dataset_dir> <exposed.json>... > tb-heldout-pool.json
    tb_heldout.py draw <pool.json> <size> <seed> [<oracle_job_dir>] > tb-heldout.json

`pool` is every task in the dataset minus every task in a tuning-exposed set,
with the difficulty and category the task declares in its own task.toml.
`draw` drops tasks the reference solution failed in an oracle job (a task the
oracle cannot pass measures nothing), then makes a seeded draw stratified by
declared difficulty, allocating by largest remainder so each stratum keeps its
share of the validated pool. Stdlib only.
"""

from __future__ import annotations

import json
import random
import sys
import tomllib
from pathlib import Path


def task_names(job_file):
    return [t for d in json.loads(Path(job_file).read_text())["datasets"] for t in d["task_names"]]


def pool(metadata, exposed):
    """metadata: {task: {difficulty, category}}; exposed: task names seen in tuning."""
    return [{"task": t, **metadata[t]} for t in sorted(metadata) if t not in exposed]


def stratified_draw(tasks, size, seed):
    if size > len(tasks):
        raise ValueError(f"cannot draw {size} from {len(tasks)} tasks")
    strata = {}
    for t in sorted(tasks, key=lambda t: t["task"]):
        strata.setdefault(t["difficulty"], []).append(t)
    quota = {k: size * len(v) / len(tasks) for k, v in strata.items()}
    alloc = {k: int(q) for k, q in quota.items()}
    for k in sorted(quota, key=lambda k: (-(quota[k] - alloc[k]), k))[: size - sum(alloc.values())]:
        alloc[k] += 1
    rng = random.Random(seed)
    return sorted((t for k in sorted(strata) for t in rng.sample(strata[k], alloc[k])), key=lambda t: t["task"])


def oracle_failures(job_dir):
    """Tasks whose reference solution did not reach reward 1 in a Harbor oracle job."""
    failed = set()
    for result in Path(job_dir).glob("*__*/result.json"):
        r = json.loads(result.read_text())
        if ((r.get("verifier_result") or {}).get("rewards") or {}).get("reward") != 1.0:
            failed.add(r.get("task_name") or result.parent.name.split("__")[0])
    return failed


if __name__ == "__main__":
    cmd, *args = sys.argv[1:]
    if cmd == "pool":
        root = Path(args[0])
        meta = {}
        for toml in sorted(root.glob("*/task.toml")):
            m = tomllib.loads(toml.read_text()).get("metadata") or {}
            meta[toml.parent.name] = {"difficulty": m.get("difficulty"), "category": m.get("category")}
        exposed = {t for f in args[1:] for t in task_names(f)}
        print(json.dumps(pool(meta, exposed), indent=1))
    elif cmd == "draw":
        tasks = json.loads(Path(args[0]).read_text())
        dropped = oracle_failures(args[3]) if len(args) > 3 else set()
        drawn = stratified_draw([t for t in tasks if t["task"] not in dropped], int(args[1]), int(args[2]))
        print(json.dumps({"jobs_dir": "/var/tmp/tbench-harbor", "datasets": [
            {"path": "/var/tmp/tbench-tasks/terminal-bench", "task_names": [t["task"] for t in drawn]}]}, indent=2))
    else:
        sys.exit(__doc__)
