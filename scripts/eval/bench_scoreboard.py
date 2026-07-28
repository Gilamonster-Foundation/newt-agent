#!/usr/bin/env python3
"""bench_scoreboard.py — publish Terminal-Bench results + enforce the per-model
release gate.

The release gate (Shawn 2026-07-28) is a **per-model monotonic ratchet**: a
model's score never goes down across releases. Establish a starting number, then
keep beating it. Beating little-coder is aspirational, not required.

Three jobs, one durable record:

  ingest  <run-dir> --model M ...   parse a Harbor run's per-task rewards and
                                    APPEND one record to the results manifest
                                    (scripts/eval/bench-results.jsonl).
  gate    --model M --score S       fail (exit 3) if S is below the model's
                                    recorded champion — the release gate.
  render  --readme README.md        rewrite the scoreboard table (champion per
                                    model) between the README markers.

The manifest is the source of truth (one JSON object per line, git-tracked). The
scoreboard shows each model's CHAMPION (best score to date); the gate blocks any
release that would lower a champion.

Pure helpers are unit-tested via ``--self-test`` (no third-party deps).
"""
from __future__ import annotations

import argparse
import glob
import json
import os
import sys

MANIFEST_DEFAULT = os.path.join(os.path.dirname(__file__), "bench-results.jsonl")
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


def champions(records: list[dict]) -> dict[str, dict]:
    """Best record per model: highest score; ties broken by the later date, then
    later manifest position (records are in insertion order)."""
    best: dict[str, dict] = {}
    for i, rec in enumerate(records):
        model = rec.get("model")
        if not model:
            continue
        cur = best.get(model)
        if cur is None:
            best[model] = {**rec, "_i": i}
            continue
        better = score_of(rec) > score_of(cur) or (
            score_of(rec) == score_of(cur)
            and (rec.get("date", ""), i) >= (cur.get("date", ""), cur["_i"])
        )
        if better:
            best[model] = {**rec, "_i": i}
    return {m: {k: v for k, v in r.items() if k != "_i"} for m, r in best.items()}


# ── the per-model release gate ──────────────────────────────────────────────
def gate(records: list[dict], model: str, new_score: float) -> tuple[bool, float]:
    """Return (ok, champion_score). ok is False when ``new_score`` is below the
    model's existing champion — the monotonic ratchet. A model with no prior
    record always passes (establishes the starting number)."""
    prior = [score_of(r) for r in records if r.get("model") == model]
    champ = max(prior) if prior else 0.0
    # Float tolerance so an identical re-run doesn't spuriously fail.
    return (new_score + 1e-9 >= champ, champ)


# ── scoreboard rendering ────────────────────────────────────────────────────
def _pct(x: float) -> str:
    return f"{x * 100:.1f}%"


def render_table(records: list[dict]) -> str:
    champs = champions(records)
    rows = sorted(champs.values(), key=lambda r: (-score_of(r), r.get("model", "")))
    header = (
        "_Per-model **champion** scores — the release gate is a monotonic ratchet: "
        "a model's score never goes down. Auto-generated; do not edit by hand._\n\n"
        "| Model | Family | Score | Passed | Suite | Window | Version | Date |\n"
        "|-------|--------|-------|--------|-------|--------|---------|------|\n"
    )
    if not rows:
        return header + "| _(no runs recorded yet)_ | | | | | | | |\n"
    body = ""
    for r in rows:
        body += (
            f"| `{r.get('model','?')}` | {r.get('family','?')} | "
            f"{_pct(score_of(r))} | {r.get('passed','?')}/{r.get('total','?')} | "
            f"{r.get('suite','?')} | {r.get('window','?')} | "
            f"{r.get('version','?')} | {r.get('date','?')} |\n"
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
        "total": agg["total"],
        "passed": agg["passed"],
        "mean_reward": agg["mean_reward"],
        "passed_tasks": agg["passed_tasks"],
    }
    append_manifest(a.manifest, rec)
    print(
        f"recorded {a.model}: {_pct(agg['mean_reward'])} "
        f"({agg['passed']}/{agg['total']}) → {a.manifest}"
    )
    return 0


def _cmd_gate(a: argparse.Namespace) -> int:
    records = load_manifest(a.manifest)
    score = a.score
    if score is None:
        agg = parse_run(a.run_dir)
        score = agg["mean_reward"]
    ok, champ = gate(records, a.model, score)
    verb = "OK" if ok else "REGRESSION"
    print(
        f"[{verb}] {a.model}: new {_pct(score)} vs champion {_pct(champ)}",
        file=sys.stderr if not ok else sys.stdout,
    )
    return 0 if ok else 3


def _cmd_render(a: argparse.Namespace) -> int:
    records = load_manifest(a.manifest)
    table = render_table(records)
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
    pi.add_argument("--date", required=True)
    pi.add_argument("--manifest", default=MANIFEST_DEFAULT)
    pi.set_defaults(fn=_cmd_ingest)

    pg = sub.add_parser("gate", help="per-model no-regression check (exit 3 on regression)")
    pg.add_argument("--model", required=True)
    g = pg.add_mutually_exclusive_group(required=True)
    g.add_argument("--score", type=float, help="the new score (mean reward, 0..1)")
    g.add_argument("--run-dir", dest="run_dir", help="parse the new score from a run dir")
    pg.add_argument("--manifest", default=MANIFEST_DEFAULT)
    pg.set_defaults(fn=_cmd_gate, score=None, run_dir=None)

    pr = sub.add_parser("render", help="rewrite the README scoreboard table")
    pr.add_argument("--readme", default="README.md")
    pr.add_argument("--manifest", default=MANIFEST_DEFAULT)
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
    recs = [
        {"model": "qwen", "date": "2026-07-28", "mean_reward": 0.10, "passed": 3, "total": 30},
        {"model": "glm", "date": "2026-07-28", "mean_reward": 0.20, "passed": 6, "total": 30},
        {"model": "qwen", "date": "2026-07-29", "mean_reward": 0.13, "passed": 4, "total": 30},
    ]
    # champions: qwen -> best (0.13), glm -> 0.20
    ch = champions(recs)
    assert score_of(ch["qwen"]) == 0.13, ch
    assert score_of(ch["glm"]) == 0.20, ch

    # gate: a new qwen run must not drop below the 0.13 champion.
    ok, champ = gate(recs, "qwen", 0.13)
    assert ok and champ == 0.13, (ok, champ)
    ok, champ = gate(recs, "qwen", 0.12)
    assert not ok, "0.12 < champion 0.13 must REGRESS"
    ok, champ = gate(recs, "qwen", 0.30)
    assert ok, "beating the champion passes"
    # a brand-new model always establishes its starting number.
    ok, champ = gate(recs, "nemotron", 0.0)
    assert ok and champ == 0.0, (ok, champ)

    # render is deterministic + contains both models, champion-ordered (glm first).
    table = render_table(recs)
    assert "glm" in table and "qwen" in table
    assert table.index("glm") < table.index("qwen"), "higher score first"
    assert "13.0%" in table and "20.0%" in table, table

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

    # tie on score → later date wins.
    tie = [
        {"model": "m", "date": "2026-07-01", "mean_reward": 0.1},
        {"model": "m", "date": "2026-07-02", "mean_reward": 0.1},
    ]
    assert champions(tie)["m"]["date"] == "2026-07-02"

    print("bench_scoreboard self-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
