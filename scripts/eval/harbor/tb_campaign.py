#!/usr/bin/env python3
"""tb_campaign.py — trial records and the report table for tb-campaign.sh (#2318).

    tb_campaign.py ingest <job_dir> <cell.json> <out_dir>   # append trials.jsonl + cells.jsonl
    tb_campaign.py table <out_dir>                         # markdown matrix
    tb_campaign.py cell <cell.json> <treatment|none> key value ...   # write a cell binding
    tb_campaign.py profile <treatment|none> <model> <in> <out>       # newt profile + env lines
    tb_campaign.py pin-check <out_dir> <cell.json>         # exit 3 naming the fields that differ
    tb_campaign.py pinned-version <out_dir> <harness>      # the version pi/codex must install

Every trial directory Harbor created becomes one row, graded or not. A cell's
expected count comes from the task set, so a trial Harbor never produced is
visible as missing. Harbor's reward is the oracle. A harness's own "done" is
recorded only as a claim, and a claim that cannot be read from the harness's
log is ``None`` (unrecoverable), never ``False``.

A trial's state, reward and verdict come from bench_scoreboard's shared
``trial_record`` / ``verdict`` (#2316), so this report and the scoreboard read a
trial the same way; this module adds only what is harness-specific. Every
attempt counts once; tasks carry equal attempts by design.
"""

from __future__ import annotations

import hashlib
import json
import math
import re
import statistics
import sys
import tomllib
from datetime import datetime
from pathlib import Path

from pi_log import inference_failure
from pi_log import records as _records

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))  # scripts/eval: the shared trial reader
from bench_scoreboard import trial_record, verdict  # noqa: E402

SUITE = "terminal-bench"

TIMEOUT = "AgentTimeoutError"

# Why a trial ended in an exception, from evidence only (never guessed):
#   infra   - the apparatus failed: endpoint unreachable, connection/rate/5xx,
#             setup or environment start, container kill (exit 137), pi's
#             exit-0 inference failure;
#   agent   - model replies were received and the harness then ended (truncated
#             stream, round cap, timeout while working, harness_error after
#             completions), or Harbor named a model-side limit;
#   unknown - neither is provable.
INFRA_EXCEPTIONS = {
    "NetworkConnectionError", "ApiConnectionError", "ApiConnectionClosedError", "ApiRateLimitError",
    "ApiInternalServerError", "ApiOverloadedError", "ModelNotFoundError", "AgentAuthenticationError",
    "AgentSetupTimeoutError", "EnvironmentStartTimeoutError", "SandboxBuildFailedError",
    "HealthcheckError", "MemoryLimitExceededError",
}
AGENT_EXCEPTIONS = {
    "OutputTokenExceededError", "OutputLengthExceededError", "ContextWindowExceededError",
    "ContextLengthExceededError", "AgentSafetyRefusalError",
}
INFRA_TEXT = re.compile(
    r"connection|error sending request|unreachable|refused|reset by peer|rate.?limit|\b429\b|"
    r"\b50[0234]\b|service unavailable|name resolution|broken pipe|stream disconnected",
    re.IGNORECASE,
)


def error_cause(exception, exit_code, harness_error, pi_failure, responses):
    """infra / agent / unknown for a trial that raised or failed inference; None otherwise.
    `harness_error` is the harness's own terminal error text (newt solve_result,
    codex turn.failed), never Harbor's stdout tail, which carries model chatter."""
    if not exception and not pi_failure:
        return None
    if pi_failure or exception in INFRA_EXCEPTIONS or exit_code == 137 or INFRA_TEXT.search(harness_error or ""):
        return "infra"
    if exception in ("unreadable result.json", "malformed reward"):  # trial_record's own: not the agent's
        return "unknown"
    if exception in AGENT_EXCEPTIONS or responses:
        return "agent"
    return "unknown"


def harness_evidence(harness, lines):
    """(model replies received, the harness's own terminal error text) from its log."""
    records = list(_records(lines))
    if harness == "newt":
        replies = sum(o.get("kind") == "chat_completion_finish" for o in records)
        errors = [o.get("error") for o in records if o.get("kind") == "solve_result"]
    elif harness == "pi":
        replies = sum(
            o.get("type") == "message_end"
            and (o.get("message") or {}).get("role") == "assistant"
            and (o.get("message") or {}).get("stopReason") not in ("error", "aborted")
            for o in records
        )
        errors = []
    else:
        replies = sum(o.get("type") == "item.completed" for o in records)
        errors = [
            o.get("message") or (o.get("error") or {}).get("message")
            for o in records
            if o.get("type") in ("error", "turn.failed")
        ]
    return replies, "; ".join(e for e in errors if e) or None


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


def max_request_output(harness, agent_dir: Path):
    """Largest output a single model request produced, where the log shows it.
    pi logs usage per assistant message; codex logs last_token_usage per request
    in its session; newt's contract only totals, so newt is None (not logged)."""
    if harness == "pi":
        log = agent_dir / "pi.txt"
        lines = log.read_text(errors="replace").splitlines() if log.exists() else []
        outs = [
            ((o.get("message") or {}).get("usage") or {}).get("output")
            for o in _records(lines)
            if o.get("type") == "message_end" and (o.get("message") or {}).get("role") == "assistant"
        ]
    elif harness == "codex":
        outs = [
            ((((o.get("payload") or {}).get("info") or {}).get("last_token_usage")) or {}).get("output_tokens")
            for f in sorted((agent_dir / "sessions").rglob("*.jsonl"))
            for o in _records(f.read_text(errors="replace").splitlines())
        ]
    else:
        return None
    outs = [n for n in outs if isinstance(n, int)]
    return max(outs) if outs else None


def _seconds(span):
    try:
        a, b = (datetime.fromisoformat(span[k].replace("Z", "+00:00")) for k in ("started_at", "finished_at"))
        return (b - a).total_seconds()
    except (TypeError, KeyError, AttributeError, ValueError):
        return None


# ── treatments ──────────────────────────────────────────────────────────────
# A treatment is ONE declared file, treatments/<name>.toml: a description, a
# newt profile fragment appended to the injected profile ({{MODEL}} and
# {{ENDPOINT}} substituted from the local profile, so no host is committed),
# adapter env limited to the knobs newt_agent.py reads, and dotted contract
# paths the run must be OBSERVED at ("*" = present). Its sha256 is its identity.
# "none" is the baseline. pi and codex take no treatment.
TREATMENT_KEYS = {"description", "profile", "env", "expect"}
TREATMENT_ENV = {
    "NEWT_BENCH_SMART", "NEWT_BENCH_SELF_VERIFY", "NEWT_BENCH_MAX_ROUNDS",
    "NEWT_BENCH_TENACITY", "NEWT_BENCH_CONTEXT_WINDOW", "NEWT_BENCH_OCAP",
}
NONE = {"name": "none", "sha256": None, "profile": "", "env": {}, "expect": {}}


def load_treatment(path):
    if str(path) in ("", "none"):
        return dict(NONE)
    raw = Path(path).read_bytes()
    t = tomllib.loads(raw.decode())
    if set(t) - TREATMENT_KEYS or not t.get("description"):
        raise ValueError(f"{path}: keys must be {sorted(TREATMENT_KEYS)} with a description")
    env = {k: str(v) for k, v in (t.get("env") or {}).items()}
    bad = sorted(k for k, v in env.items() if k not in TREATMENT_ENV or not re.fullmatch(r"[A-Za-z0-9._-]*", v))
    if bad:
        raise ValueError(f"{path}: env not allowed: {bad}")
    name = Path(path).stem
    return {"name": name, "sha256": hashlib.sha256(raw).hexdigest(), "profile": t.get("profile", ""),
            "env": env, "expect": t.get("expect") or {}}


def observed(expect, record):
    """Whether a newt contract record shows every declared path; None when the
    treatment declares nothing observable or there is no record to read."""
    if not expect or record is None:
        return None

    def at(path):
        o = record
        for part in path.split("."):
            if not isinstance(o, dict) or part not in o:
                return None
            o = o[part]
        return o

    return all(at(p) is not None if v == "*" else at(p) == v for p, v in expect.items())


# ── pinning ─────────────────────────────────────────────────────────────────
# Cells pool or pair only under one pin. The first cell writes it; a later cell
# that differs is refused. The model fingerprint is the server's own metadata
# (size, params, ftype, ...) plus the GGUF basename: a fingerprint, not a content
# digest — the router exposes no weights hash.
PINNED = ("task_set_sha256", "engine", "ctx_served", "newt_binary_sha256", "instrument_commit")


def pin_mismatch(pin, cell):
    bad = [k for k in PINNED if pin.get(k) != cell.get(k)]
    fp = (pin.get("models") or {}).get(cell["model"])
    if fp is not None and fp != cell.get("model_fingerprint"):
        bad.append("model_fingerprint")
    version = (pin.get("harness_versions") or {}).get(cell["harness"])
    if version and cell.get("harness_version") and version != cell["harness_version"]:
        bad.append("harness_version")
    return bad


def pin_extend(pin, cell):
    for k in PINNED:
        pin.setdefault(k, cell.get(k))
    pin.setdefault("models", {}).setdefault(cell["model"], cell.get("model_fingerprint"))
    if cell.get("harness_version"):
        pin.setdefault("harness_versions", {}).setdefault(cell["harness"], cell["harness_version"])
    return pin


def pin_check(out: Path, cell_json: Path):
    path, cell = out / "campaign.pin.json", json.loads(cell_json.read_text())
    pin = json.loads(path.read_text()) if path.exists() else {}
    bad = pin_mismatch(pin, cell) if pin else []
    if bad:  # the cell is recorded as skipped, so a re-run under the right pin retries it
        cell["skipped"] = f"pin mismatch: {','.join(bad)}"
        cell_json.write_text(json.dumps(cell))
        print(",".join(bad))
        return 3
    path.write_text(json.dumps(pin_extend(pin, cell), indent=1, sort_keys=True))
    return 0


def build_cell(t, pairs):
    """A cell binding from key/value pairs; `*_json` values are parsed, counts are ints."""
    cell = {}
    for k, v in zip(pairs[::2], pairs[1::2]):
        if k.endswith("_json"):
            cell[k[: -len("_json")]] = json.loads(v) if v else None
        else:
            cell[k] = int(v) if k in ("expected", "trials", "n_tasks") else (v or None)
    cell.update(treatment=t["name"], treatment_sha256=t["sha256"], treatment_env=t["env"],
                treatment_expect=t["expect"])
    return cell


def render_profile(t, model, base):
    """The newt profile for one model under one treatment. A fragment that breaks
    the TOML fails here, before any container starts."""
    endpoint = re.search(r'(?m)^endpoint *= *"(.*)"', base)
    text = re.sub(r"(?m)^model *=.*$", f'model = "{model}"', base, count=1)
    fragment = t["profile"].replace("{{MODEL}}", model).replace("{{ENDPOINT}}", endpoint.group(1) if endpoint else "")
    text += "\n" + fragment if fragment else ""
    tomllib.loads(text)
    return text


def trial_row(harness, trial: Path, expect=None):
    rec = trial_record(str(trial))
    exc, reward = rec["exception"], rec["reward"]
    try:  # harness-specific fields only; state and reward are trial_record's
        r = json.loads((trial / "result.json").read_text())
        r = r if isinstance(r, dict) else {}
    except (OSError, ValueError):
        r = {}
    # pi exits 0 when every model call failed (pi_log); Harbor then grades an
    # untouched workspace. That is an apparatus error, not a model result.
    log = trial / "agent" / CLAIM[harness][1]
    lines = log.read_text(errors="replace").splitlines() if log.exists() else []
    infra = inference_failure(lines) if harness == "pi" else None
    replies, harness_error = harness_evidence(harness, lines)
    exit_code = re.search(r"exit (\d+)", (r.get("exception_info") or {}).get("exception_message") or "")
    cause = error_cause(exc, exit_code and int(exit_code.group(1)), harness_error, infra, replies)
    state = "error" if infra else rec["state"]
    claimed, claim_source = claim(harness, trial / "agent", exc)
    agent = r.get("agent_result") or {}
    tokens_in, tokens_out = agent.get("n_input_tokens"), agent.get("n_output_tokens")
    source = "harness log via Harbor agent_result"
    label = ((r.get("agent_info") or {}).get("model_info") or {}).get("name")
    # pi and codex send the label itself; newt sends its profile's model, which
    # can drift from the label (smart-off-pilot6: labelled one model, ran another).
    effective, harness_config, contract = label, None, None
    if harness == "newt":
        tokens_in, source, effective = None, "newt contract timing.gen_tokens (input not emitted)", None
        timing = {}
        events = trial / "agent/newt-events.jsonl"
        for o in _records(events.read_text(errors="replace").splitlines() if events.exists() else []):
            timing = o.get("timing") or timing
            effective = o.get("effective_model") or effective
            harness_config = o.get("effective_config") or harness_config
            contract = o if "outcome" in o else contract
        tokens_out = timing.get("gen_tokens")
    return {
        "trial": rec["trial"],
        "task": rec["task"],
        "task_checksum": r.get("task_checksum"),
        "harness_version": (r.get("agent_info") or {}).get("version"),
        "model_label": label,
        "model_effective": effective,
        "harness_config": harness_config,
        "treatment_observed": observed(expect, contract),
        "result": bool(r),
        "exception": exc,
        "reward": reward,
        "raw_reward": rec["raw"],
        "state": state,
        "inference_failure": infra,
        "error_cause": cause,
        "model_replies": replies,
        "harness_error": harness_error,
        "resolved": verdict(reward, SUITE) == "resolved",
        "claimed_done": claimed,
        "claim_source": claim_source,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "tokens_source": source,
        "max_request_output_tokens": max_request_output(harness, trial / "agent"),
        "agent_s": _seconds(r.get("agent_execution")),
    }


def ingest(job_dir: Path, cell_json: Path, out: Path):
    cell = json.loads(cell_json.read_text())
    rows = [
        {**{k: cell.get(k) for k in ("campaign", "model", "harness", "job")},
         **trial_row(cell["harness"], t, cell.get("treatment_expect"))}
        for t in sorted(p for p in job_dir.iterdir() if p.is_dir())
    ]
    if not cell.get("harness_version") and rows:
        cell["harness_version"] = rows[0]["harness_version"]
    pin_path = out / "campaign.pin.json"
    if pin_path.exists():  # a version installed at setup can still differ from the pin
        pin = json.loads(pin_path.read_text())
        cell["pin_mismatch"] = pin_mismatch(pin, cell) or None
        if not cell["pin_mismatch"]:
            pin_path.write_text(json.dumps(pin_extend(pin, cell), indent=1, sort_keys=True))
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
    # graded is trial_record's: a finite reward and no exception. A reward that
    # arrived alongside an exception still says what the workspace held, so the
    # agent-failure rate and the claim analysis use it unless the cause is infra.
    graded = [r for r in rows if r["state"] == "graded"]
    scored = [r for r in rows if r["reward"] is not None and r.get("error_cause") != "infra"]
    with_agent = graded + [r for r in scored if r["state"] == "error" and r.get("error_cause") == "agent"]
    claimed = [r for r in scored if r["claimed_done"] is True]
    known_in = [r["tokens_in"] for r in rows if r["tokens_in"] is not None]
    known_out = [r["tokens_out"] for r in rows if r["tokens_out"] is not None]
    secs = [r["agent_s"] for r in rows if r["agent_s"] is not None]
    rate_rows = [r for r in rows if r["tokens_out"] is not None and r["agent_s"]]
    return {
        "expected": cell["expected"],
        "observed": len(rows),
        "graded": len(graded),
        "errors": sum(r["state"] == "error" for r in rows),
        "rate_excl": (sum(r["resolved"] for r in graded), len(graded)),
        "rate_agent_fail": (sum(r["resolved"] for r in with_agent), len(with_agent)),
        "causes": tuple(sum(r.get("error_cause") == c for r in rows) for c in ("infra", "agent", "unknown")),
        "claimed": len(claimed),
        "false_completions": sum(not r["resolved"] for r in claimed),
        "false_incompletes": sum(r["resolved"] for r in scored if r["claimed_done"] is False),
        "unrecoverable_claims": sum(r["claimed_done"] is None for r in rows),
        "exceptions": sum(r["exception"] is not None for r in rows),
        "inference_errors": sum(bool(r.get("inference_failure")) for r in rows),
        "agent_timeouts": sum(r["exception"] == TIMEOUT for r in rows),
        "max_request_output": max(
            (r["max_request_output_tokens"] for r in rows if r.get("max_request_output_tokens") is not None), default=None
        ),
        "model_mismatches": sum(r["model_effective"] not in (None, cell["model"]) for r in rows),
        "not_observed": sum(r.get("treatment_observed") is False for r in rows),
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
        "| model | harness | expected / observed / graded / error | resolved / n, rate [95% Wilson]: trials with any exception excluded | resolved / n, rate [95% Wilson]: agent-caused exceptions counted as failures (infra, unknown excluded) | claimed done | false completions | false incompletes | unrecoverable claims | exceptions: infra / agent / unknown | inference errors (ungraded) | agent timeouts | largest single-request output | ran another model | treatment declared but not observed | tokens in (n known) | tokens out (n known) | agent s median / total | out tok per agent-s |",
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    # The last record per job wins: a cell skipped once and run later shows the run.
    cells = {c["job"]: c for c in _records((out / "cells.jsonl").read_text().splitlines())}
    for cell in cells.values():
        mine = [r for r in rows if r.get("job", cell["job"]) == cell["job"]
                and (r["model"], r["harness"]) == (cell["model"], cell["harness"])]
        s = summarize(cell, mine)
        g = s["graded"]
        def rate(k_n):
            k, n = k_n
            if cell.get("skipped"):
                return f"— (skipped: {cell['skipped']})"
            if cell.get("pin_mismatch"):
                return f"— (pin mismatch: {','.join(cell['pin_mismatch'])})"
            return f"{k}/{n}, {k / n:.2f} [{wilson(k, n)[0]:.2f}, {wilson(k, n)[1]:.2f}]" if n else "—"

        med = f"{s['agent_s_median']:.0f}" if s["agent_s_median"] is not None else "—"
        tps = f"{s['out_tok_per_agent_s']:.1f}" if s["out_tok_per_agent_s"] is not None else "—"
        fc = f"{s['false_completions']}/{s['claimed']}" if s["claimed"] else "0/0"
        big = f"{s['max_request_output']:,}" if s["max_request_output"] is not None else "not logged"
        treat = cell.get("treatment") or "none"
        seen = "n/a" if treat == "none" or not cell.get("treatment_expect") else s["not_observed"]
        tin, tout = (f"{t[0]:,} ({t[1]})" if t[1] else "— (0)" for t in (s["tokens_in"], s["tokens_out"]))
        lines.append(
            f"| {cell['model']} | {cell['harness']} {cell.get('harness_version') or ''} [{treat}] "
            f"| {s['expected']} / {s['observed']} / {g} / {s['errors']} | {rate(s['rate_excl'])} | {rate(s['rate_agent_fail'])} | {s['claimed']} | {fc} "
            f"| {s['false_incompletes']} | {s['unrecoverable_claims']} | {s['exceptions']}: {'/'.join(map(str, s['causes']))} | {s['inference_errors']} | {s['agent_timeouts']} | {big} | {s['model_mismatches']} | {seen} | {tin} "
            f"| {tout} | {med} / {s['agent_s_total']:.0f} | {tps} |"
        )
    print("\n".join(lines))


if __name__ == "__main__":
    cmd, *args = sys.argv[1:]
    if cmd == "ingest":
        ingest(Path(args[0]), Path(args[1]), Path(args[2]))
    elif cmd == "table":
        table(Path(args[0]))
    elif cmd == "cell":
        Path(args[0]).write_text(json.dumps(build_cell(load_treatment(args[1]), args[2:])))
    elif cmd == "profile":  # writes the profile; prints the treatment's adapter env as KEY=VALUE lines
        t = load_treatment(args[0])
        Path(args[3]).write_text(render_profile(t, args[1], Path(args[2]).read_text()))
        print("\n".join(f"{k}={v}" for k, v in sorted(t["env"].items())))
    elif cmd == "pin-check":
        sys.exit(pin_check(Path(args[0]), Path(args[1])))
    elif cmd == "pinned-version":
        path = Path(args[0]) / "campaign.pin.json"
        print(((json.loads(path.read_text()) if path.exists() else {}).get("harness_versions") or {}).get(args[1], ""))
    else:
        sys.exit(__doc__)
