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
  parity  --model M [--tolerance T]   fail (exit 3) if OCAP-on trails OCAP-off by
                                    more than T; exit 2 while a lane is unmeasured.
  render  --readme README.md        rewrite the scoreboard (each model's off/on
                                    champions + parity Δ) between the README markers.

The manifest is the source of truth (one JSON object per line, git-tracked).
Records with no ``ocap`` field are OCAP-off (they predate the lane split). The
scoreboard shows each model's CHAMPION per lane; the gate blocks any release that
would lower a champion, and parity blocks 0.7.6 until on≈off.

Pure helpers are unit-tested via ``--self-test`` (no third-party deps).
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import sys

MANIFEST_DEFAULT = os.path.join(os.path.dirname(__file__), "bench-results.jsonl")
ROSTER_DEFAULT = os.path.join(os.path.dirname(__file__), "bench-roster.json")
START_MARKER = "<!-- BENCH-SCOREBOARD:START -->"
END_MARKER = "<!-- BENCH-SCOREBOARD:END -->"


# ── run parsing ─────────────────────────────────────────────────────────────
def _task_reward(task_dir: str) -> float | None:
    """The reward for one Harbor task dir: verifier/reward.txt (a float), else
    dig result.json for a numeric ``reward``. None when neither is present."""
    rt = os.path.join(task_dir, "verifier", "reward.txt")
    if os.path.exists(rt):
        try:
            return float(open(rt).read().strip())
        except ValueError:
            pass
    rj = os.path.join(task_dir, "result.json")
    if os.path.exists(rj):
        found: list[float] = []

        def dig(o: object) -> None:
            if isinstance(o, dict):
                r = o.get("reward")
                if isinstance(r, (int, float)):
                    found.append(float(r))
                for v in o.values():
                    dig(v)
            elif isinstance(o, list):
                for v in o:
                    dig(v)

        try:
            dig(json.load(open(rj)))
        except (ValueError, OSError):
            return None
        if found:
            return found[0]
    return None


def parse_run(run_dir: str) -> dict:
    """Aggregate a Harbor run dir into ``{total, passed, mean_reward,
    passed_tasks}``. A task 'passes' at reward >= 1.0; ``mean_reward`` matches
    Harbor's own Mean. Only immediate ``*__*`` task subdirs are counted."""
    rewards: dict[str, float] = {}
    for d in sorted(glob.glob(os.path.join(run_dir, "*__*"))):
        if not os.path.isdir(d):
            continue
        task = os.path.basename(d).split("__")[0]
        r = _task_reward(d)
        if r is not None:
            rewards[task] = r
    total = len(rewards)
    passed = sorted(t for t, r in rewards.items() if r >= 1.0)
    mean = (sum(rewards.values()) / total) if total else 0.0
    return {
        "total": total,
        "passed": len(passed),
        "mean_reward": round(mean, 4),
        "passed_tasks": passed,
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


def render_table(records: list[dict], roster: list[dict] | None = None) -> str:
    """The parity scoreboard: one row per model with its OCAP-off and OCAP-on
    champions side by side and the parity delta (on − off) between them. Each
    lane is a monotonic ratchet; the release gate for 0.7.6 is per-model on≈off
    (delta ≥ 0). Roster models with no run yet render as queued rows, so the
    table doubles as the whole-matrix parity tracker."""
    champs = champions(records)
    # Every model that appears in the data or the roster gets one row.
    families: dict[str, str] = {}
    for (m, _lane), r in champs.items():
        families.setdefault(m, r.get("family", "?"))
    for e in roster or []:
        if e.get("model"):
            families.setdefault(e["model"], e.get("family", "?"))

    def sort_key(m: str) -> tuple[float, str]:
        off, on = champs.get((m, "off")), champs.get((m, "on"))
        best = max(
            (score_of(r) for r in (off, on) if r is not None),
            default=-1.0,
        )
        return (-best, m)

    header = (
        "_Per-model Terminal-Bench champions, **OCAP off vs on**. Each lane is a "
        "monotonic ratchet (a score never goes down). 0.7.6 establishes the honesty-classified, digest-pinned confined (OCAP-on) baseline; OCAP-on within reach of OCAP-off (parity) is pursued forward via pre-granted permissions, not gated here. Auto-generated; do "
        "not edit by hand._\n\n"
        "| Model | Family | OCAP off | OCAP on | Parity Δ | Suite | Window | Version | Date |\n"
        "|-------|--------|----------|---------|----------|-------|--------|---------|------|\n"
    )
    if not families:
        return header + "| _(no runs recorded yet)_ | | | | | | | | |\n"
    body = ""
    for m in sorted(families, key=sort_key):
        off, on = champs.get((m, "off")), champs.get((m, "on"))
        p = parity(records, m)
        # Prefer the OCAP-on run's metadata for the row (the 0.7.6 focus); fall
        # back to OCAP-off, then to nothing.
        meta = on or off or {}
        body += (
            f"| `{m}` | {families[m]} | "
            f"{_lane_cell(off, pending=on is not None)} | "
            f"{_lane_cell(on, pending=off is not None)} | "
            f"{_pp(p['delta'])} | "
            f"{meta.get('suite', 'tb-30')} | {meta.get('window', '—')} | "
            f"{meta.get('version', '—')} | {meta.get('date', '—')} |\n"
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


# ── CLI ─────────────────────────────────────────────────────────────────────
def _cmd_ingest(a: argparse.Namespace) -> int:
    agg = parse_run(a.run_dir)
    if agg["total"] == 0:
        print(f"error: no task results found under {a.run_dir}", file=sys.stderr)
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
    table = render_table(records, load_roster(a.roster))
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
    pr.set_defaults(fn=_cmd_render)

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
    assert "0.0 pp" in table, table  # qwen parity delta
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
    assert rt.count("| `qwen` |") == 1, "measured roster model must not duplicate"
    assert "`nemotron` | nemotron | _queued_ | _queued_" in rt, rt
    # roster-only (no runs at all) still renders rows, not the empty placeholder.
    only = render_table([], [{"model": "m1", "family": "f"}])
    assert "_queued_" in only and "no runs recorded" not in only
    # missing roster file → empty list, never a crash.
    assert load_roster("/nonexistent/roster.json") == []

    # tie on score → later date wins (within a lane).
    tie = [
        {"model": "m", "date": "2026-07-01", "mean_reward": 0.1},
        {"model": "m", "date": "2026-07-02", "mean_reward": 0.1},
    ]
    assert champions(tie)[("m", "off")]["date"] == "2026-07-02"

    print("bench_scoreboard self-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
