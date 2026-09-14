#!/usr/bin/env python3
"""bench_scoreboard.py — publish Terminal-Bench results + enforce the per-model
release gate.

The release gate (Shawn 2026-07-28) is a **per-model monotonic ratchet**: a
model's score never goes down across releases. Establish a starting number, then
keep beating it. Beating little-coder is aspirational, not required. Each model
is tracked on two independent lanes — **OCAP off** (the ``--yolo`` full-access
bench) and **OCAP on** (the confined bench) — and the 0.7.6 gate adds a
**parity** requirement: OCAP-on must come within reach of OCAP-off.

Four jobs, one durable record:

  ingest  <run-dir> --model M --ocap off|on   parse a Harbor run's per-task
                                    rewards and APPEND one record to the results
                                    manifest (scripts/eval/bench-results.jsonl).
  gate    --model M --ocap L --score S   fail (exit 3) if S is below the model's
                                    champion ON THAT LANE — the ratchet.
          ingest and ``gate --run-dir`` exit 2 on incomplete reward coverage
          (see ``coverage_gap``) unless ``--allow-incomplete`` is passed.
  parity  --model M [--tolerance T]   fail (exit 3) if OCAP-on trails OCAP-off by
                                    more than T; exit 2 while a lane is unmeasured.
  render  --readme README.md        rewrite the scoreboard (each model's off/on
                                    champions + parity Δ) between the README markers.
                                    ``--no-queued`` trims never-run roster rows for
                                    the README; the full roster-tracking table is
                                    published by gilamonster-bench.

The manifest is the source of truth (one JSON object per line, git-tracked).
Records with no ``ocap`` field are OCAP-off (they predate the lane split). The
scoreboard shows each model's CHAMPION per lane; the gate blocks any release that
would lower a champion, and parity blocks 0.7.6 until on≈off.

Pure helpers are unit-tested via ``--self-test`` (no third-party deps).
"""

from __future__ import annotations

import argparse
import collections
import glob
import json
import math
import os
import sys

MANIFEST_DEFAULT = os.path.join(os.path.dirname(__file__), "bench-results.jsonl")
ROSTER_DEFAULT = os.path.join(os.path.dirname(__file__), "bench-roster.json")
START_MARKER = "<!-- BENCH-SCOREBOARD:START -->"
END_MARKER = "<!-- BENCH-SCOREBOARD:END -->"


# ── run parsing ─────────────────────────────────────────────────────────────
# A suite's verdict contract, as data: reward -> verdict. Terminal-Bench's
# verifier writes 1 when every test passes and 0 otherwise, so any other reward
# (partial credit, no reward, a suite with no contract here) is "unknown" —
# never a pass. The raw reward stays on the trial record either way.
VERDICT_CONTRACTS = {
    suite: {1.0: "resolved", 0.0: "failed"} for suite in ("tb-30", "terminal-bench")
}
# How attempts weigh into ``mean_reward``: every attempt that carries a finite
# reward counts once — including a verifier-graded errored trial, since dropping
# a timeout from the denominator would reward timing out. One attempt per task
# reduces to the historical per-task mean.
POLICY = "attempt-mean"
# Added to each manifest record beside the historical fields (add-only schema).
COVERAGE_FIELDS = (
    "policy",
    "expected",
    "observed",
    "graded",
    "missing",
    "errors_by_class",
)


def verdict(reward: float | None, suite: str) -> str:
    """``resolved`` / ``failed`` / ``unknown`` for one reward under ``suite``."""
    return VERDICT_CONTRACTS.get(suite, {}).get(reward, "unknown")


def load_job(run_dir: str) -> dict:
    """Harbor's job-level ``config.json`` (the roster) and ``result.json`` (trial
    stats, token and cost totals). A file that is absent or unreadable is None."""
    job: dict = {}
    for name in ("config", "result"):
        try:
            job[name] = json.load(open(os.path.join(run_dir, f"{name}.json")))
        except (OSError, ValueError):
            job[name] = None
    return job


def expected_trials(config: dict | None) -> int | None:
    """Trials the job declared: roster tasks x ``n_attempts`` x agents. None when
    the roster is not a literal list (globs, exclusions, ``n_tasks``, no config):
    an unknown expectation must not be reported as the observed count."""
    if not config:
        return None
    names = list(config.get("tasks") or [])
    for ds in config.get("datasets") or []:
        literal = ds.get("task_names") and not any(
            set("*?[") & set(n) for n in ds["task_names"]
        )
        if not literal or ds.get("exclude_task_names") or ds.get("n_tasks") is not None:
            return None
        names += ds["task_names"]
    agents = max(1, len(config.get("agents") or []))
    return len(names) * (config.get("n_attempts") or 1) * agents if names else None


def trial_record(trial_dir: str) -> dict:
    """One Harbor trial: ``{trial, task, source, state, reward, raw, exception}``.

    ``state`` is ``graded`` (a finite reward, no exception), ``ungraded`` (no
    reward at all) or ``error`` (a malformed or non-finite reward, an unreadable
    result.json, or any ``exception_info`` — a verifier reward alongside an
    exception is kept in ``reward``). ``raw`` is the reward exactly as found."""
    name = os.path.basename(trial_dir)
    rec = {"trial": name, "task": name.split("__")[0], "source": None}
    rec.update(state="ungraded", reward=None, raw=None, exception=None)
    res: object = {}
    rj = os.path.join(trial_dir, "result.json")
    try:
        res = json.load(open(rj)) if os.path.exists(rj) else {}
        if not isinstance(res, dict):
            raise ValueError("not a JSON object")
    except (OSError, ValueError):
        return {**rec, "state": "error", "exception": "unreadable result.json"}
    rec.update(task=res.get("task_name") or rec["task"], source=res.get("source"))
    rec["trial"] = res.get("trial_name") or name
    rt = os.path.join(trial_dir, "verifier", "reward.txt")
    if os.path.exists(rt):
        rec["raw"] = open(rt).read().strip()
    else:
        found: list[object] = []

        def dig(o: object) -> None:
            if isinstance(o, dict):
                r = o.get("reward")
                if isinstance(r, (int, float)):
                    found.append(r)
                for v in o.values():
                    dig(v)
            elif isinstance(o, list):
                for v in o:
                    dig(v)

        dig(res)
        rec["raw"] = str(found[0]) if found else None
    if rec["raw"] is not None:
        try:
            reward = float(rec["raw"])
        except ValueError:
            reward = math.nan
        if math.isfinite(reward):
            rec.update(state="graded", reward=reward)
        else:
            rec.update(state="error", exception="malformed reward")
    exc = res.get("exception_info")
    if exc:
        exc_type = exc.get("exception_type") if isinstance(exc, dict) else None
        rec.update(state="error", exception=exc_type or str(exc))
    return rec


def parse_run(run_dir: str, suite: str = "tb-30") -> dict:
    """Aggregate a Harbor run dir. Only immediate ``*__*`` trial subdirs count.

    Historical fields: ``total`` (attempts with a finite reward), ``passed`` /
    ``passed_tasks`` (attempts the suite verdict marks resolved) and
    ``mean_reward`` under ``policy``. Coverage: ``expected`` (None when the roster
    is unknown), ``observed``, ``graded``, ``missing`` (expected minus graded) and
    ``errors_by_class`` (error-state trials counted by ``exception``).
    ``trials`` holds every per-trial record."""
    trials = [
        trial_record(d)
        for d in sorted(glob.glob(os.path.join(run_dir, "*__*")))
        if os.path.isdir(d)
    ]
    scored = [t for t in trials if t["reward"] is not None]
    passed = sorted(
        t["task"] for t in scored if verdict(t["reward"], suite) == "resolved"
    )
    total = len(scored)
    mean = math.fsum(t["reward"] for t in scored) / total if total else 0.0
    expected = expected_trials(load_job(run_dir)["config"])
    graded = sum(t["state"] == "graded" for t in trials)
    return {
        "total": total,
        "passed": len(passed),
        "mean_reward": round(mean, 4),
        "passed_tasks": passed,
        "policy": POLICY,
        "expected": expected,
        "observed": len(trials),
        "graded": graded,
        "errors_by_class": dict(
            collections.Counter(t["exception"] for t in trials if t["state"] == "error")
        ),
        "missing": None if expected is None else expected - graded,
        "trials": trials,
    }


# ── manifest ────────────────────────────────────────────────────────────────
def load_manifest(path: str) -> list[dict]:
    if not os.path.exists(path):
        return []
    out = []
    for line in open(path):
        line = line.strip()
        if line:
            out.append(json.loads(line))
    return out


def append_manifest(path: str, record: dict) -> None:
    with open(path, "a") as f:
        f.write(json.dumps(record, sort_keys=True) + "\n")


def score_of(record: dict) -> float:
    """The record's headline score = its mean reward (Harbor's Mean)."""
    return float(record.get("mean_reward", 0.0))


def lane_of(record: dict) -> str:
    """The OCAP lane a record was measured on: ``"on"`` (confined) or ``"off"``
    (the ``--yolo`` full-access lane). Records predating the lane split have no
    ``ocap`` field and are all OCAP-off, so absent → ``"off"``."""
    return "on" if str(record.get("ocap", "off")).lower() == "on" else "off"


def champions(records: list[dict]) -> dict[tuple[str, str], dict]:
    """Best record per ``(model, lane)``: highest score; ties broken by the later
    date, then later manifest position (records are in insertion order). Keying
    by lane keeps each model's OCAP-off and OCAP-on ratchets independent."""
    best: dict[tuple[str, str], dict] = {}
    for i, rec in enumerate(records):
        model = rec.get("model")
        if not model:
            continue
        key = (model, lane_of(rec))
        cur = best.get(key)
        if cur is None:
            best[key] = {**rec, "_i": i}
            continue
        better = score_of(rec) > score_of(cur) or (
            score_of(rec) == score_of(cur)
            and (rec.get("date", ""), i) >= (cur.get("date", ""), cur["_i"])
        )
        if better:
            best[key] = {**rec, "_i": i}
    return {k: {kk: vv for kk, vv in r.items() if kk != "_i"} for k, r in best.items()}


# ── the per-model release gate ──────────────────────────────────────────────
def gate(
    records: list[dict], model: str, new_score: float, ocap: str = "off"
) -> tuple[bool, float]:
    """Return (ok, champion_score). ok is False when ``new_score`` is below the
    model's existing champion **on the same OCAP lane** — the monotonic ratchet.
    The OCAP-off and OCAP-on lanes ratchet independently, so turning confinement
    on can't be blocked by the (typically higher) unconfined champion. A model
    with no prior record on that lane always passes (establishes the number)."""
    lane = "on" if str(ocap).lower() == "on" else "off"
    prior = [
        score_of(r) for r in records if r.get("model") == model and lane_of(r) == lane
    ]
    champ = max(prior) if prior else 0.0
    # Float tolerance so an identical re-run doesn't spuriously fail.
    return (new_score + 1e-9 >= champ, champ)


def parity(records: list[dict], model: str, tolerance: float = 0.0) -> dict:
    """The OCAP off-vs-on parity picture for one model, from champions on each
    lane. Returns ``{off, on, delta, ok, measured}``: ``delta = on - off`` (the
    confinement cost, ≤ 0 means confinement lost tasks); ``ok`` is True when both
    lanes are measured and ``on >= off - tolerance`` (confinement costs no more
    than ``tolerance``) — the 0.7.6 release gate. ``measured`` is False until both
    lanes have a run (parity is undecidable on one lane alone)."""
    champs = champions(records)
    off = champs.get((model, "off"))
    on = champs.get((model, "on"))
    off_s = score_of(off) if off else None
    on_s = score_of(on) if on else None
    measured = off_s is not None and on_s is not None
    delta = (on_s - off_s) if measured else None
    ok = measured and (on_s + 1e-9 >= off_s - tolerance)
    return {"off": off_s, "on": on_s, "delta": delta, "ok": ok, "measured": measured}


# ── scoreboard rendering ────────────────────────────────────────────────────
def _pct(x: float) -> str:
    return f"{x * 100:.1f}%"


def _pp(delta: float | None) -> str:
    """A parity delta (on − off) as signed percentage points; ``—`` when a lane
    is missing. ``0.0 pp`` (unsigned) marks exact parity."""
    if delta is None:
        return "—"
    v = delta * 100
    return "0.0 pp" if abs(v) < 0.05 else f"{v:+.1f} pp"


def _lane_cell(rec: dict | None, *, pending: bool) -> str:
    """A lane's scoreboard cell: its ``score (passed/total)`` when measured, else
    ``_pending_`` (the other lane is measured, this lane's run is owed) or
    ``_queued_`` (no run on either lane yet)."""
    if rec is None:
        return "_pending_" if pending else "_queued_"
    return f"{_pct(score_of(rec))} ({rec.get('passed', '?')}/{rec.get('total', '?')})"


def load_roster(path: str) -> list[dict]:
    """The model matrix the scoreboard tracks. Missing/invalid file → empty
    (roster rows are additive; the champions always render)."""
    try:
        return json.load(open(path)).get("roster", [])
    except (OSError, ValueError):
        return []


def render_table(
    records: list[dict], roster: list[dict] | None = None, *, queued: bool = True
) -> str:
    """The parity scoreboard: one row per model with its OCAP-off and OCAP-on
    champions side by side and the parity delta (on − off) between them. Each
    lane is a monotonic ratchet; the release gate for 0.7.6 is per-model on≈off
    (delta ≥ 0). Roster models with no run yet render as queued rows, so the
    table doubles as the whole-matrix parity tracker.

    ``queued=False`` is the README profile: it drops rows that have never run on
    either lane, because an unrun model is a to-do list rather than a result.
    It never suppresses a ``_pending_`` cell — a half-measured model keeps its
    row, since a blank cell would read as a zero where a label reads as a gap.
    The full roster-tracking table is published by gilamonster-bench."""
    champs = champions(records)
    # Every model that appears in the data — plus, unless trimmed for the README,
    # every model the roster still owes a run.
    families: dict[str, str] = {}
    for (m, _lane), r in champs.items():
        families.setdefault(m, r.get("family", "?"))
    for e in roster or []:
        if queued and e.get("model"):
            families.setdefault(e["model"], e.get("family", "?"))

    def sort_key(m: str) -> tuple[float, str]:
        off, on = champs.get((m, "off")), champs.get((m, "on"))
        best = max(
            (score_of(r) for r in (off, on) if r is not None),
            default=-1.0,
        )
        return (-best, m)

    scope = (
        "Measured models only; the roster's unrun models are in the full table."
        if not queued
        else "0.7.6 establishes the honesty-classified, digest-pinned confined "
        "(OCAP-on) baseline; OCAP-on within reach of OCAP-off (parity) is pursued "
        "forward via pre-granted permissions, not gated here."
    )
    header = (
        "_Per-model Terminal-Bench champions, **OCAP off vs on**. Each lane is a "
        f"monotonic ratchet (a score never goes down). {scope} "
        "Auto-generated; do not edit by hand._\n\n"
        "| Model | OCAP off | OCAP on |\n"
        "|-------|----------|---------|\n"
    )
    if not families:
        return header + "| _(no runs recorded yet)_ | | |\n"
    body = ""
    for m in sorted(families, key=sort_key):
        off, on = champs.get((m, "off")), champs.get((m, "on"))
        # Prefer the OCAP-on run's metadata for the row (the 0.7.6 focus); fall
        # back to OCAP-off, then to nothing.
        meta = on or off or {}
        # Metadata folds onto a second line under the model name; parity Δ is
        # dropped (it is just on − off), leaving three columns instead of nine.
        if meta:
            sub = (
                f"{families[m]} · {meta.get('suite', 'tb-30')} · "
                f"ctx {meta.get('window', '—')} · v{meta.get('version', '—')} · "
                f"{meta.get('date', '—')}"
            )
        else:
            sub = f"{families[m]} · queued"
        body += (
            f"| `{m}`<br><sub>{sub}</sub> | "
            f"{_lane_cell(off, pending=on is not None)} | "
            f"{_lane_cell(on, pending=off is not None)} |\n"
        )
    return header + body


def inject(readme_text: str, table: str) -> str:
    """Replace the content between the markers with ``table``. Idempotent. Raises
    if the markers are absent (fail loud rather than silently not publishing)."""
    s, e = readme_text.find(START_MARKER), readme_text.find(END_MARKER)
    if s == -1 or e == -1 or e < s:
        raise ValueError(
            f"README is missing the scoreboard markers "
            f"{START_MARKER!r} … {END_MARKER!r}"
        )
    before = readme_text[: s + len(START_MARKER)]
    after = readme_text[e:]
    return f"{before}\n{table}\n{after}"


def coverage_gap(agg: dict) -> str | None:
    """Why a parsed run's reward coverage is incomplete, or None when every
    declared trial is graded and no trial dir falls outside the roster."""
    expected, observed, graded = agg["expected"], agg["observed"], agg["graded"]
    if graded == 0:
        return "no graded trials"
    if expected is None:
        return "expected trial count unknown (no literal roster in config.json)"
    if observed > expected:
        return f"roster mismatch: {observed} trial dirs, {graded} graded, {expected} declared"
    if graded < expected:
        return f"graded {graded} of {expected} expected"
    return None


# ── CLI ─────────────────────────────────────────────────────────────────────
def _refuse_coverage(a: argparse.Namespace, agg: dict) -> bool:
    """Report and return True when the run's coverage gap blocks ``a``'s command.
    ``--allow-incomplete`` waives a gap, but never a run with no graded trial."""
    gap = coverage_gap(agg)
    if gap is None or (a.allow_incomplete and agg["graded"]):
        return False
    hint = "; pass --allow-incomplete to use it anyway" if agg["graded"] else ""
    print(f"error: {a.run_dir}: {gap}{hint}", file=sys.stderr)
    return True


def _cmd_ingest(a: argparse.Namespace) -> int:
    agg = parse_run(a.run_dir, a.suite)
    if _refuse_coverage(a, agg):
        return 2
    rec = {
        "date": a.date,
        "version": a.version,
        "model": a.model,
        "family": a.family,
        "suite": a.suite,
        "window": a.window,
        "ocap": "on" if str(a.ocap).lower() == "on" else "off",
        "total": agg["total"],
        "passed": agg["passed"],
        "mean_reward": agg["mean_reward"],
        "passed_tasks": agg["passed_tasks"],
        **{k: agg[k] for k in COVERAGE_FIELDS},
        # An accepted --allow-incomplete stays visible downstream.
        "coverage_override": coverage_gap(agg) is not None,
    }
    append_manifest(a.manifest, rec)
    print(
        f"recorded {a.model} [ocap={rec['ocap']}]: {_pct(agg['mean_reward'])} "
        f"({agg['passed']}/{agg['total']}) → {a.manifest}"
    )
    return 0


def _cmd_gate(a: argparse.Namespace) -> int:
    records = load_manifest(a.manifest)
    score = a.score
    if score is None:
        agg = parse_run(a.run_dir)
        if _refuse_coverage(a, agg):
            return 2
        score = agg["mean_reward"]
    ok, champ = gate(records, a.model, score, a.ocap)
    verb = "OK" if ok else "REGRESSION"
    print(
        f"[{verb}] {a.model} [ocap={a.ocap}]: new {_pct(score)} vs champion {_pct(champ)}",
        file=sys.stderr if not ok else sys.stdout,
    )
    return 0 if ok else 3


def _cmd_parity(a: argparse.Namespace) -> int:
    records = load_manifest(a.manifest)
    p = parity(records, a.model, a.tolerance)
    if not p["measured"]:
        have = (
            "off"
            if p["off"] is not None
            else ("on" if p["on"] is not None else "neither")
        )
        print(
            f"[PENDING] {a.model}: parity undecidable — only the {have} lane is measured",
            file=sys.stderr,
        )
        return 2
    verb = "PARITY" if p["ok"] else "GAP"
    print(
        f"[{verb}] {a.model}: off {_pct(p['off'])} → on {_pct(p['on'])} "
        f"(Δ {_pp(p['delta'])}, tolerance {_pp(a.tolerance)})",
        file=sys.stderr if not p["ok"] else sys.stdout,
    )
    return 0 if p["ok"] else 3


def _cmd_render(a: argparse.Namespace) -> int:
    records = load_manifest(a.manifest)
    table = render_table(records, load_roster(a.roster), queued=a.queued)
    text = open(a.readme).read()
    new = inject(text, table)
    if new != text:
        open(a.readme, "w").write(new)
        print(f"updated scoreboard in {a.readme}")
    else:
        print(f"scoreboard already current in {a.readme}")
    return 0


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--self-test", action="store_true", help="run built-in tests")
    sub = p.add_subparsers(dest="cmd")

    pi = sub.add_parser("ingest", help="append a run to the manifest")
    pi.add_argument("run_dir")
    pi.add_argument("--model", required=True)
    pi.add_argument("--family", required=True)
    pi.add_argument("--version", required=True)
    pi.add_argument("--suite", default="tb-30")
    pi.add_argument("--window", type=int, default=0)
    pi.add_argument(
        "--ocap",
        choices=["off", "on"],
        default="off",
        help="the OCAP lane this run was measured on (default off)",
    )
    pi.add_argument("--date", required=True)
    pi.add_argument("--manifest", default=MANIFEST_DEFAULT)
    pi.set_defaults(fn=_cmd_ingest)

    pg = sub.add_parser(
        "gate", help="per-model per-lane no-regression check (exit 3 on regression)"
    )
    pg.add_argument("--model", required=True)
    pg.add_argument(
        "--ocap",
        choices=["off", "on"],
        default="off",
        help="which OCAP lane to ratchet against (default off)",
    )
    g = pg.add_mutually_exclusive_group(required=True)
    g.add_argument("--score", type=float, help="the new score (mean reward, 0..1)")
    g.add_argument(
        "--run-dir", dest="run_dir", help="parse the new score from a run dir"
    )
    pg.add_argument("--manifest", default=MANIFEST_DEFAULT)
    pg.set_defaults(fn=_cmd_gate, score=None, run_dir=None)
    for sp in (pi, pg):
        sp.add_argument(
            "--allow-incomplete",
            action="store_true",
            help="accept a run whose reward coverage is incomplete (recorded as "
            "coverage_override); a run with no graded trial is always refused",
        )

    pp = sub.add_parser(
        "parity",
        help="per-model OCAP off-vs-on parity check (exit 3 on gap, 2 if pending)",
    )
    pp.add_argument("--model", required=True)
    pp.add_argument(
        "--tolerance",
        type=float,
        default=0.0,
        help="max acceptable confinement cost in mean-reward (default 0.0)",
    )
    pp.add_argument("--manifest", default=MANIFEST_DEFAULT)
    pp.set_defaults(fn=_cmd_parity)

    pr = sub.add_parser("render", help="rewrite the README scoreboard table")
    pr.add_argument("--readme", default="README.md")
    pr.add_argument("--manifest", default=MANIFEST_DEFAULT)
    pr.add_argument("--roster", default=ROSTER_DEFAULT)
    pr.add_argument(
        "--no-queued",
        dest="queued",
        action="store_false",
        help="drop never-run roster rows (the README profile); the full "
        "roster-tracking table is published by gilamonster-bench",
    )
    pr.set_defaults(fn=_cmd_render, queued=True)

    args = p.parse_args(argv)
    if args.self_test:
        return _self_test()
    if not getattr(args, "cmd", None):
        p.print_help()
        return 1
    return args.fn(args)


# ── self-test ───────────────────────────────────────────────────────────────
def _self_test() -> int:
    # Records with no `ocap` are OCAP-off (back-compat); `ocap: "on"` is the
    # confined lane. Each (model, lane) ratchets independently.
    recs = [
        {
            "model": "qwen",
            "date": "2026-07-28",
            "mean_reward": 0.10,
            "passed": 3,
            "total": 30,
        },
        {
            "model": "glm",
            "date": "2026-07-28",
            "mean_reward": 0.20,
            "passed": 6,
            "total": 30,
        },
        {
            "model": "qwen",
            "date": "2026-07-29",
            "mean_reward": 0.13,
            "passed": 4,
            "total": 30,
        },
        {
            "model": "qwen",
            "date": "2026-07-29",
            "mean_reward": 0.13,
            "passed": 4,
            "total": 30,
            "ocap": "on",
        },
    ]
    # lane_of: absent → off, explicit on → on.
    assert lane_of(recs[0]) == "off" and lane_of(recs[3]) == "on"

    # champions are keyed by (model, lane): qwen-off best 0.13, qwen-on 0.13, glm-off 0.20.
    ch = champions(recs)
    assert score_of(ch[("qwen", "off")]) == 0.13, ch
    assert score_of(ch[("qwen", "on")]) == 0.13, ch
    assert score_of(ch[("glm", "off")]) == 0.20, ch
    assert ("qwen", "on") in ch and ("glm", "on") not in ch

    # gate ratchets PER LANE: the qwen-off champion is 0.13; the qwen-on champion
    # is also 0.13, but turning OCAP on is gated only against the on-lane, never
    # the (here equal, generally higher) off-lane.
    ok, champ = gate(recs, "qwen", 0.13, "off")
    assert ok and champ == 0.13, (ok, champ)
    ok, champ = gate(recs, "qwen", 0.12, "off")
    assert not ok, "0.12 < off champion 0.13 must REGRESS"
    # a brand-new lane always establishes its starting number.
    ok, champ = gate(recs, "glm", 0.0, "on")
    assert ok and champ == 0.0, "glm has no on-lane run yet → establishes"
    ok, champ = gate(recs, "nemotron", 0.0, "off")
    assert ok and champ == 0.0, (ok, champ)

    # parity: qwen has both lanes (0.13 vs 0.13) → measured, Δ 0, at parity.
    pq = parity(recs, "qwen")
    assert pq["measured"] and pq["ok"] and pq["delta"] == 0.0, pq
    # a confinement cost within tolerance still passes; beyond it fails.
    costly = recs + [
        {"model": "k", "mean_reward": 0.20},
        {"model": "k", "mean_reward": 0.14, "ocap": "on"},
    ]
    assert parity(costly, "k")["delta"] < 0
    assert not parity(costly, "k", tolerance=0.0)["ok"], "−6pp gap fails zero tolerance"
    assert parity(costly, "k", tolerance=0.10)["ok"], "within 10pp tolerance passes"
    # glm has only the off lane → parity undecidable.
    assert parity(recs, "glm")["measured"] is False

    # render: one row per model with both lanes; qwen shows off 13% AND on 13%
    # with a 0.0 pp parity delta; glm's on-lane is _pending_ (off measured, on owed).
    table = render_table(recs)
    assert "glm" in table and "qwen" in table
    assert table.index("glm") < table.index("qwen"), "higher off-score first"
    assert "_pending_" in table, "glm on-lane owed"
    qrow = [ln for ln in table.splitlines() if "`qwen`" in ln][0]
    assert qrow.count("13.0%") == 2, f"both qwen lanes at 13%: {qrow}"

    # inject is idempotent and marker-bounded.
    readme = f"# newt\n\n{START_MARKER}\nold\n{END_MARKER}\n\ntail\n"
    once = inject(readme, table)
    twice = inject(once, table)
    assert once == twice, "inject must be idempotent"
    assert "old" not in once and "tail" in once and table.strip() in once

    # missing markers fail loud.
    try:
        inject("no markers here", table)
        assert False, "expected ValueError on missing markers"
    except ValueError:
        pass

    # roster: unmeasured models render as queued rows, measured ones don't duplicate.
    roster = [
        {"model": "qwen", "family": "qwen"},  # measured → no extra row
        {"model": "nemotron", "family": "nemotron"},  # unmeasured → queued row
    ]
    rt = render_table(recs, roster)
    assert rt.count("`qwen`<br>") == 1, "measured roster model must not duplicate"
    assert (
        "`nemotron`<br><sub>nemotron · queued</sub> | _queued_ | _queued_" in rt
    ), rt
    # roster-only (no runs at all) still renders rows, not the empty placeholder.
    only = render_table([], [{"model": "m1", "family": "f"}])
    assert "_queued_" in only and "no runs recorded" not in only
    # missing roster file → empty list, never a crash.
    assert load_roster("/nonexistent/roster.json") == []

    # queued=False (the README profile): rows with no run on EITHER lane are
    # dropped — a never-run model is a to-do list, not a result. Measured rows
    # survive untouched.
    trimmed = render_table(recs, roster, queued=False)
    assert "`nemotron`<br>" not in trimmed, trimmed
    assert trimmed.count("`qwen`<br>") == 1, trimmed
    # ...but a half-measured model KEEPS its row and its _pending_ cell. A blank
    # would read as a zero; a labelled absence routes the reader to the gap.
    half = [{"model": "solo", "family": "f", "ocap": "on", "mean_reward": 0.5}]
    ht = render_table(half, [{"model": "unrun", "family": "f"}], queued=False)
    assert "`solo`<br>" in ht and "_pending_" in ht, ht
    assert "`unrun`<br>" not in ht, ht
    # dropping every row still yields the honest placeholder, never a bare header.
    empty = render_table([], [{"model": "unrun", "family": "f"}], queued=False)
    assert "no runs recorded" in empty, empty

    # tie on score → later date wins (within a lane).
    tie = [
        {"model": "m", "date": "2026-07-01", "mean_reward": 0.1},
        {"model": "m", "date": "2026-07-02", "mean_reward": 0.1},
    ]
    assert champions(tie)[("m", "off")]["date"] == "2026-07-02"

    _self_test_ingestion()
    print("bench_scoreboard self-test: OK")
    return 0


def _self_test_ingestion() -> None:
    """#2316: every Harbor trial survives parsing, and coverage stays visible.

    The fixture needs a duplicate task prefix with DIFFERENT rewards: without
    one, the old prefix-keyed dict is green here too (the vacuous-green trap).

    More trial dirs than the roster declares is a roster mismatch even when
    graded == expected: Harbor 0.20 never leaves an extra dir of its own. A retry
    reuses the trial name and dir (job.py:459-469, via _on_trial_started; the
    failed dir is removed in trial/queue.py:200-221), and a resume deletes every
    trial dir without result.json (job.py:221-229). An extra dir means a mixed or
    hand-edited run dir."""
    import contextlib
    import io
    import re
    import subprocess
    import tempfile

    def cli(*argv: str) -> tuple[object, str]:
        err = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
            try:
                rc: object = main(list(argv))
            except SystemExit as e:
                rc = e.code
        return rc, err.getvalue()

    def ingest(run_dir: str, *extra: str) -> tuple[object, str]:
        man = os.path.join(run_dir, "manifest.jsonl")
        meta = ("--model", "m", "--family", "f", "--version", "0", "--date", "d")
        return cli("ingest", run_dir, *meta, "--manifest", man, *extra)

    def gate_run(run_dir: str, *extra: str) -> tuple[object, str]:
        man = os.path.join(run_dir, "manifest.jsonl")
        return cli(
            "gate", "--model", "m", "--run-dir", run_dir, "--manifest", man, *extra
        )

    def trial(root: str, name: str, reward: str | None = None, **result) -> None:
        os.makedirs(os.path.join(root, name, "verifier"))
        if reward is not None:
            open(os.path.join(root, name, "verifier", "reward.txt"), "w").write(reward)
        if result:
            task = name.split("__")[0]
            res = {"task_name": task, "trial_name": name, "source": "terminal-bench"}
            open(os.path.join(root, name, "result.json"), "w").write(
                json.dumps({**res, "exception_info": None, **result})
            )

    def job(root: str, first: str, second: str, roster: bool = True) -> dict:
        if roster:  # 4 tasks x 2 attempts = 8 expected trials
            cfg = {"datasets": [{"task_names": ["taskx", "tasky", "taskz", "taskw"]}]}
            open(os.path.join(root, "config.json"), "w").write(
                json.dumps({**cfg, "n_attempts": 2, "agents": [{}]})
            )
        trial(root, "taskx__aaa", first, ok=1)
        trial(root, "taskx__bbb", second, ok=1)
        trial(root, "tasky__ccc", "0.5", ok=1)  # partial credit
        os.makedirs(os.path.join(root, "taskz__ddd"))  # no result.json, no reward
        trial(root, "taskw__eee", "nan", ok=1)  # non-finite
        timeout = {"exception_type": "AgentTimeoutError"}
        trial(root, "taskw__fff", "1", exception_info=timeout)  # errored, verifier ran
        return parse_run(root)

    with tempfile.TemporaryDirectory() as a, tempfile.TemporaryDirectory() as b:
        agg = job(a, "0", "1")
        # A2: swapping which attempt dir holds which reward changes nothing.
        swapped = job(b, "1", "0")
        strip = lambda g: {k: v for k, v in g.items() if k != "trials"}  # noqa: E731
        assert strip(agg) == strip(swapped), (strip(agg), strip(swapped))
        # A3/A4: four distinct integers, expected read from the job's roster.
        counts = [agg[k] for k in ("expected", "observed", "graded", "missing")]
        assert counts == [8, 6, 3, 5], counts
        # A1/A5/A6: one record per trial, identity and raw reward retained.
        by = {t["trial"]: t for t in agg["trials"]}
        assert [by["taskx__aaa"]["reward"], by["taskx__bbb"]["reward"]] == [0.0, 1.0]
        assert by["taskx__aaa"]["task"] == by["taskx__bbb"]["task"] == "taskx"
        assert by["taskz__ddd"]["state"] == "ungraded", by["taskz__ddd"]
        assert (by["taskw__eee"]["state"], by["taskw__eee"]["raw"]) == ("error", "nan")
        assert by["taskw__eee"]["reward"] is None
        fff = by["taskw__fff"]
        assert (fff["state"], fff["exception"], fff["reward"]) == (
            "error",
            "AgentTimeoutError",
            1.0,
        ), fff
        classes = {"AgentTimeoutError": 1, "malformed reward": 1}
        assert agg["errors_by_class"] == classes, agg
        # A7 attempt-mean: rewards 0, 1, 0.5 and the errored-but-verified 1, each
        # attempt weighted once = 0.625 (per-task mean would be 0.6667, any-attempt
        # 0.8333); nan never enters it.
        policy = (agg["policy"], agg["total"], agg["mean_reward"])
        assert policy == ("attempt-mean", 4, 0.625), agg
        assert (agg["passed"], agg["passed_tasks"]) == (2, ["taskw", "taskx"]), agg
        # A9: partial credit on a binary suite is unknown, never resolved.
        assert by["tasky__ccc"]["reward"] == 0.5
        assert [verdict(r, "tb-30") for r in (1.0, 0.0, 0.5, None)] == [
            "resolved",
            "failed",
            "unknown",
            "unknown",
        ]
        assert verdict(1.0, "no-such-suite") == "unknown"
        for raw in ("inf", "-inf", "abc"):
            trial(a, f"taskv__{raw}", raw, ok=1)
            t = trial_record(os.path.join(a, f"taskv__{raw}"))
            assert (t["state"], t["reward"], t["raw"]) == ("error", None, raw), t
        # A10: cross-tab shares the verdict and survives a trial with no result.json.
        # newt claims completion on the attempt Harbor graded 0 (bbb, swapped).
        os.makedirs(os.path.join(b, "taskx__bbb", "agent"))
        open(os.path.join(b, "taskx__bbb", "agent", "newt-events.jsonl"), "w").write(
            '{"outcome": "completed"}\n'
        )
        cross_tab = os.path.join(os.path.dirname(__file__), "harbor", "cross-tab.py")
        run = subprocess.run(
            [sys.executable, cross_tab, b], capture_output=True, text=True
        )
        assert run.returncode == 0, run.stderr
        line = "Harbor resolved 2/6; newt claimed completed 1; FALSE COMPLETIONS 1"
        assert line in run.stdout, run.stdout
        assert re.search(r"harbor=unknown", run.stdout), run.stdout
        # A8: graded 3 of 8 expected is refused by ingest and gate, and the
        # refusal writes nothing; only the explicit override lets it through.
        rc, err = ingest(b)
        assert (rc, "graded 3 of 8 expected" in err) == (2, True), (rc, err)
        assert not os.path.exists(os.path.join(b, "manifest.jsonl")), "refusal wrote"
        rc, err = gate_run(b)
        assert (rc, "graded 3 of 8 expected" in err) == (2, True), (rc, err)
        assert ingest(b, "--allow-incomplete")[0] == 0
        # The override is written down: an incomplete run never reads complete.
        rec = load_manifest(os.path.join(b, "manifest.jsonl"))[0]
        trace = [rec.get(k) for k in ("coverage_override", "expected", "graded")]
        assert (trace, rec["missing"]) == ([True, 8, 3], 5), rec
        assert gate_run(b, "--allow-incomplete")[0] == 0

    with tempfile.TemporaryDirectory() as c:
        # A4: no config.json -> expected coverage is unknown, not `observed`.
        agg = job(c, "0", "1", roster=False)
        unknown = (agg["expected"], agg["missing"], agg["observed"])
        assert unknown == (None, None, 6), agg
        # A8: an unknown expectation is refused too, never read as complete.
        rc, err = ingest(c)
        assert (rc, "unknown" in err) == (2, True), (rc, err)
        rc, err = gate_run(c)
        assert (rc, "unknown" in err) == (2, True), (rc, err)
        assert ingest(c, "--allow-incomplete")[0] == 0
        rec = load_manifest(os.path.join(c, "manifest.jsonl"))[0]
        trace = [rec.get(k) for k in ("coverage_override", "expected", "graded")]
        assert trace == [True, None, 3], rec

    with tempfile.TemporaryDirectory() as d:
        # A11: a complete single-attempt run keeps the historical record values.
        open(os.path.join(d, "config.json"), "w").write(
            json.dumps({"datasets": [{"task_names": ["p", "q"]}]})
        )
        trial(d, "p__1", "1.0", ok=1)
        trial(d, "q__1", "0", ok=1)
        agg = parse_run(d)
        old = {"total": 2, "passed": 1, "mean_reward": 0.5, "passed_tasks": ["p"]}
        assert {k: agg[k] for k in old} == old, agg
        assert (agg["expected"], agg["graded"], agg["missing"]) == (2, 2, 0), agg
        # A8 twin: a complete run needs no override, and says it took none.
        assert ingest(d) == (0, ""), ingest(d)
        rec = load_manifest(os.path.join(d, "manifest.jsonl"))[0]
        trace = [rec.get(k) for k in ("coverage_override", "expected", "graded")]
        assert trace == [False, 2, 2], rec

    with tempfile.TemporaryDirectory() as e:
        # A8: zero graded trials is never a score, override or not — every
        # trial errored (the 30/30 infra-zero run) must not become a champion.
        open(os.path.join(e, "config.json"), "w").write(
            json.dumps({"datasets": [{"task_names": ["p"]}]})
        )
        trial(e, "p__1", "0", exception_info={"exception_type": "ApiRateLimitError"})
        for run in (ingest(e, "--allow-incomplete"), gate_run(e, "--allow-incomplete")):
            assert (run[0], "no graded trials" in run[1]) == (2, True), run

    with tempfile.TemporaryDirectory() as f:
        # A8: more graded than the roster declares (a resume, a roster edit)
        # makes missing negative. That is a roster mismatch, never "complete".
        open(os.path.join(f, "config.json"), "w").write(
            json.dumps({"datasets": [{"task_names": ["p"]}]})
        )
        trial(f, "p__1", "1", ok=1)
        trial(f, "p__2", "0", ok=1)
        assert parse_run(f)["missing"] == -1
        for rc, err in (ingest(f), gate_run(f)):
            assert (rc, "roster mismatch" in err) == (2, True), (rc, err)
        assert ingest(f, "--allow-incomplete")[0] == 0
        rec = load_manifest(os.path.join(f, "manifest.jsonl"))[0]
        trace = [rec.get(k) for k in ("coverage_override", "expected", "graded")]
        assert trace == [True, 1, 2], rec

    with tempfile.TemporaryDirectory() as g:
        # A8: an extra trial dir (observed 2 > expected 1) is a roster mismatch
        # even though graded == expected leaves missing at 0.
        open(os.path.join(g, "config.json"), "w").write(
            json.dumps({"datasets": [{"task_names": ["p"]}]})
        )
        trial(g, "p__1", "1", ok=1)
        os.makedirs(os.path.join(g, "p__2"))  # leftover, no result.json
        agg = parse_run(g)
        counts = [agg[k] for k in ("expected", "observed", "graded", "missing")]
        assert counts == [1, 2, 1, 0], counts
        for rc, err in (ingest(g), gate_run(g)):
            assert (rc, "roster mismatch" in err) == (2, True), (rc, err)
        assert ingest(g, "--allow-incomplete")[0] == 0
        rec = load_manifest(os.path.join(g, "manifest.jsonl"))[0]
        trace = [rec.get(k) for k in ("coverage_override", "expected", "graded")]
        assert trace == [True, 1, 1], rec


if __name__ == "__main__":
    sys.exit(main())
