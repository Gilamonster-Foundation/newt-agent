#!/usr/bin/env python3
"""tb_campaign.py — trial records and the report table for tb-campaign.sh (#2318).

    tb_campaign.py ingest <job_dir> <cell.json> <out_dir>   # append trials.jsonl + cells.jsonl
    tb_campaign.py table <out_dir>                         # markdown matrix

Every trial directory Harbor created becomes one row, graded or not. A cell's
expected count comes from the task set, so a trial Harbor never produced is
visible as missing. Harbor's reward is the oracle. A harness's own "done" is
recorded only as a claim, and a claim that cannot be read from the harness's
log is ``None`` (unrecoverable), never ``False``.

Weighting: every attempt counts once; tasks carry equal attempts by design.
Resolved means reward == 1.0 (terminal-bench rewards are binary); any other
finite reward is kept in ``reward`` and counts as unresolved.
"""

from __future__ import annotations

import json
import math
import statistics
import sys
from datetime import datetime
from pathlib import Path

from pi_log import inference_failure
from pi_log import records as _records

TIMEOUT = "AgentTimeoutError"


def newt_claim(lines):
    """newt's contract record carries `outcome`; `completed` is its claim."""
    outcome = None
    for o in _records(lines):
        if "outcome" in o:
            outcome = o["outcome"]
    return None if outcome is None else (outcome == "completed", f"contract outcome={outcome}")


def pi_claim(lines):
    """pi emits `agent_end` once per attempt (auto-retry re-runs the agent), so
    the LAST one decides; its final assistant stopReason of `stop` is the claim."""
    ends = [o for o in _records(lines) if o.get("type") == "agent_end"]
    if not ends:
        return None
    msgs = [m for m in ends[-1].get("messages") or [] if m.get("role") == "assistant"]
    reason = msgs[-1].get("stopReason") if msgs else None
    return reason == "stop", f"agent_end stopReason={reason}"


def codex_claim(lines):
    """`codex exec --json` ends a turn with `turn.completed` or `turn.failed`."""
    for o in _records(lines):
        if o.get("type") == "turn.completed":
            return True, "turn.completed"
        if o.get("type") in ("turn.failed", "error"):
            return False, o["type"]
    return None


CLAIM = {
    "newt": (newt_claim, "newt-events.jsonl"),
    "pi": (pi_claim, "pi.txt"),
    "codex": (codex_claim, "codex.txt"),
}


def claim(harness, agent_dir: Path, exception):
    parse, name = CLAIM[harness]
    log = agent_dir / name
    found = parse(log.read_text(errors="replace").splitlines()) if log.exists() else None
    if found:
        return found
    if exception == TIMEOUT:
        return False, "killed by Harbor agent timeout before any claim"
    return None, f"unrecoverable: no claim in {name}" + (f" ({exception})" if exception else "")


def _seconds(span):
    try:
        a, b = (datetime.fromisoformat(span[k].replace("Z", "+00:00")) for k in ("started_at", "finished_at"))
        return (b - a).total_seconds()
    except (TypeError, KeyError, AttributeError, ValueError):
        return None


def trial_row(harness, trial: Path):
    r = json.loads((trial / "result.json").read_text()) if (trial / "result.json").exists() else {}
    exc = (r.get("exception_info") or {}).get("exception_type")
    reward = ((r.get("verifier_result") or {}).get("rewards") or {}).get("reward")
    graded = isinstance(reward, (int, float)) and math.isfinite(reward)
    # pi exits 0 when every model call failed (pi_log); Harbor then grades an
    # untouched workspace. That is an apparatus error, not a model result.
    infra = None
    if harness == "pi":
        log = trial / "agent" / "pi.txt"
        infra = inference_failure(log.read_text(errors="replace").splitlines() if log.exists() else [])
    state = "error" if infra else ("graded" if graded else "ungraded")
    graded = graded and not infra
    claimed, claim_source = claim(harness, trial / "agent", exc)
    agent = r.get("agent_result") or {}
    tokens_in, tokens_out = agent.get("n_input_tokens"), agent.get("n_output_tokens")
    source = "harness log via Harbor agent_result"
    label = ((r.get("agent_info") or {}).get("model_info") or {}).get("name")
    # pi and codex send the label itself; newt sends its profile's model, which
    # can drift from the label (smart-off-pilot6: labelled one model, ran another).
    effective, harness_config = label, None
    if harness == "newt":
        tokens_in, source, effective = None, "newt contract timing.gen_tokens (input not emitted)", None
        timing = {}
        events = trial / "agent/newt-events.jsonl"
        for o in _records(events.read_text(errors="replace").splitlines() if events.exists() else []):
            timing = o.get("timing") or timing
            effective = o.get("effective_model") or effective
            harness_config = o.get("effective_config") or harness_config
        tokens_out = timing.get("gen_tokens")
    return {
        "trial": trial.name,
        "task": r.get("task_name") or trial.name.split("__")[0],
        "task_checksum": r.get("task_checksum"),
        "harness_version": (r.get("agent_info") or {}).get("version"),
        "model_label": label,
        "model_effective": effective,
        "harness_config": harness_config,
        "result": bool(r),
        "exception": exc,
        "reward": reward,
        "state": state,
        "inference_failure": infra,
        "graded": graded,
        "resolved": graded and reward == 1.0,
        "claimed_done": claimed,
        "claim_source": claim_source,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "tokens_source": source,
        "agent_s": _seconds(r.get("agent_execution")),
    }


def ingest(job_dir: Path, cell_json: Path, out: Path):
    cell = json.loads(cell_json.read_text())
    rows = [
        {**{k: cell[k] for k in ("campaign", "model", "harness")}, **trial_row(cell["harness"], t)}
        for t in sorted(p for p in job_dir.iterdir() if p.is_dir())
    ]
    if not cell.get("harness_version") and rows:
        cell["harness_version"] = rows[0]["harness_version"]
    with open(out / "trials.jsonl", "a") as f:
        f.writelines(json.dumps(row) + "\n" for row in rows)
    with open(out / "cells.jsonl", "a") as f:
        f.write(json.dumps({**cell, "observed": len(rows)}) + "\n")


def wilson(k, n, z=1.96):
    if n == 0:
        return None
    p, d = k / n, 1 + z * z / n
    c, h = (p + z * z / (2 * n)) / d, z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return max(0.0, c - h), min(1.0, c + h)


def summarize(cell, rows):
    graded = [r for r in rows if r["graded"]]
    resolved = sum(r["resolved"] for r in graded)
    claimed = [r for r in graded if r["claimed_done"] is True]
    known_in = [r["tokens_in"] for r in rows if r["tokens_in"] is not None]
    known_out = [r["tokens_out"] for r in rows if r["tokens_out"] is not None]
    secs = [r["agent_s"] for r in rows if r["agent_s"] is not None]
    rate_rows = [r for r in rows if r["tokens_out"] is not None and r["agent_s"]]
    return {
        "expected": cell["expected"],
        "observed": len(rows),
        "graded": len(graded),
        "resolved": resolved,
        "interval": wilson(resolved, len(graded)),
        "claimed": len(claimed),
        "false_completions": sum(not r["resolved"] for r in claimed),
        "false_incompletes": sum(r["resolved"] for r in graded if r["claimed_done"] is False),
        "unrecoverable_claims": sum(r["claimed_done"] is None for r in rows),
        "exceptions": sum(r["exception"] is not None for r in rows),
        "inference_errors": sum(r.get("state") == "error" for r in rows),
        "model_mismatches": sum(r["model_effective"] not in (None, cell["model"]) for r in rows),
        "tokens_in": (sum(known_in), len(known_in)),
        "tokens_out": (sum(known_out), len(known_out)),
        "agent_s_median": statistics.median(secs) if secs else None,
        "agent_s_total": sum(secs),
        "out_tok_per_agent_s": (
            sum(r["tokens_out"] for r in rate_rows) / sum(r["agent_s"] for r in rate_rows) if rate_rows else None
        ),
    }


def table(out: Path):
    trials = out / "trials.jsonl"
    rows = list(_records(trials.read_text().splitlines())) if trials.exists() else []
    lines = [
        "| model | harness | expected / observed / graded | resolved | rate [95% Wilson] | claimed done | false completions | false incompletes | unrecoverable claims | exceptions | inference errors (ungraded) | ran another model | tokens in (n known) | tokens out (n known) | agent s median / total | out tok per agent-s |",
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    # The last record per job wins: a cell skipped once and run later shows the run.
    cells = {c["job"]: c for c in _records((out / "cells.jsonl").read_text().splitlines())}
    for cell in cells.values():
        mine = [r for r in rows if (r["model"], r["harness"]) == (cell["model"], cell["harness"])]
        s = summarize(cell, mine)
        g = s["graded"]
        rate = f"{s['resolved'] / g:.2f} [{s['interval'][0]:.2f}, {s['interval'][1]:.2f}]" if g else "—"
        if cell.get("skipped"):
            rate = f"— (skipped: {cell['skipped']})"
        med = f"{s['agent_s_median']:.0f}" if s["agent_s_median"] is not None else "—"
        tps = f"{s['out_tok_per_agent_s']:.1f}" if s["out_tok_per_agent_s"] is not None else "—"
        fc = f"{s['false_completions']}/{s['claimed']}" if s["claimed"] else "0/0"
        tin, tout = (f"{t[0]:,} ({t[1]})" if t[1] else "— (0)" for t in (s["tokens_in"], s["tokens_out"]))
        lines.append(
            f"| {cell['model']} | {cell['harness']} {cell.get('harness_version') or ''} "
            f"| {s['expected']} / {s['observed']} / {g} | {s['resolved']} | {rate} | {s['claimed']} | {fc} "
            f"| {s['false_incompletes']} | {s['unrecoverable_claims']} | {s['exceptions']} | {s['inference_errors']} | {s['model_mismatches']} | {tin} "
            f"| {tout} | {med} / {s['agent_s_total']:.0f} | {tps} |"
        )
    print("\n".join(lines))


if __name__ == "__main__":
    cmd, *args = sys.argv[1:]
    if cmd == "ingest":
        ingest(Path(args[0]), Path(args[1]), Path(args[2]))
    elif cmd == "table":
        table(Path(args[0]))
    else:
        sys.exit(__doc__)
