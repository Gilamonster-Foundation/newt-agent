#!/usr/bin/env python3
"""Score the IDE-for-LLMs navigation bench (#1286) from raw per-session tallies.

Reads a `results.csv` (schema in README.md), and prints:
  - a per-(arm, model) table: N, rounds-to-first-correct-file (mean ± se),
    total rounds, first-call-correct-crate rate, confabulated-path rate,
    rounds-to-honest-miss, prefill latency;
  - paired Δ on the PRIMARY metric (rounds-to-first-correct-file) vs the prior
    arm, per model — the "did this mechanism help" read;
  - the D1 ceiling pick over the map-size arms (A4a/b/c): the SMALLEST arm whose
    mean is within one standard error of the best (spec §6.3).

Pure stdlib — no dependencies. Blank cells are MISSING (excluded), never 0.

    python3 score.py results.csv
"""
import csv
import math
import statistics
import sys
from collections import defaultdict

# Canonical arm order (matches README) for the paired-Δ walk. Map-size arms are
# compared among themselves for the ceiling pick, not in the linear Δ walk.
ARM_ORDER = ["A0", "A1", "A2", "A3", "A4a", "A4b", "A4c", "A5", "A6", "A7"]
MAP_SIZE_ARMS = {"A4a": 16000, "A4b": 32000, "A4c": 64000}
PRIMARY = "rounds_to_first_correct_file"


def num(row, key):
    v = (row.get(key) or "").strip()
    if v == "":
        return None
    try:
        return float(v)
    except ValueError:
        return None


def mean_se(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return (None, None, 0)
    m = statistics.mean(xs)
    se = statistics.stdev(xs) / math.sqrt(len(xs)) if len(xs) > 1 else 0.0
    return (m, se, len(xs))


def fmt(m, se=None):
    if m is None:
        return "  —  "
    return f"{m:.2f} ± {se:.2f}" if se is not None else f"{m:.2f}"


def load(path):
    with open(path, newline="") as f:
        rows = list(csv.DictReader(f))
    groups = defaultdict(list)  # (arm, model) -> [row, ...]
    for r in rows:
        arm = (r.get("arm") or "").strip()
        model = (r.get("model") or "").strip()
        if arm and model:
            groups[(arm, model)].append(r)
    return groups


def confab_rate(rows):
    conf = sum((num(r, "confabulated_paths") or 0) for r in rows)
    tot = sum((num(r, "total_path_refs") or 0) for r in rows)
    return (conf / tot) if tot else None


def rate(rows, key):
    vals = [num(r, key) for r in rows]
    vals = [v for v in vals if v is not None]
    return (sum(1 for v in vals if v >= 0.5) / len(vals)) if vals else None


def main():
    if len(sys.argv) != 2:
        print(__doc__)
        sys.exit(2)
    groups = load(sys.argv[1])
    models = sorted({m for (_, m) in groups})
    arms_present = [a for a in ARM_ORDER if any(a == arm for (arm, _) in groups)]

    for model in models:
        print(f"\n=== model: {model} ===")
        hdr = ("arm", "N", "first-correct (mean±se)", "total", "crate@1",
               "confab", "honest-miss", "prefill_ms")
        print("{:<5} {:>3} {:>24} {:>7} {:>8} {:>7} {:>12} {:>9}".format(*hdr))
        prev = None
        for arm in arms_present:
            rows = groups.get((arm, model), [])
            if not rows:
                continue
            m, se, n = mean_se([num(r, PRIMARY) for r in rows])
            total, _, _ = mean_se([num(r, "total_rounds") for r in rows])
            crate = rate(rows, "first_call_correct_crate")
            confab = confab_rate(rows)
            miss, _, _ = mean_se([num(r, "rounds_to_honest_miss") for r in rows])
            prefill, _, _ = mean_se([num(r, "prefill_ms") for r in rows])
            delta = ""
            if arm not in MAP_SIZE_ARMS and prev is not None and m is not None:
                d = m - prev
                delta = f"  (Δ {d:+.2f})"
            if arm not in MAP_SIZE_ARMS and m is not None:
                prev = m
            print("{:<5} {:>3} {:>24} {:>7} {:>8} {:>7} {:>12} {:>9}{}".format(
                arm, n, fmt(m, se), fmt(total),
                fmt(crate) if crate is not None else "  —  ",
                f"{confab:.1%}" if confab is not None else "  —  ",
                fmt(miss), fmt(prefill), delta))

        # D1 ceiling pick over the map-size arms.
        picks = []
        for arm, chars in sorted(MAP_SIZE_ARMS.items(), key=lambda kv: kv[1]):
            rows = groups.get((arm, model), [])
            m, se, n = mean_se([num(r, PRIMARY) for r in rows])
            if m is not None:
                picks.append((chars, m, se, arm))
        if picks:
            best_chars, best_m, best_se, best_arm = min(picks, key=lambda p: p[1])
            tol = best_se or 0.0
            within = [p for p in picks if p[1] <= best_m + tol]
            pin = min(within, key=lambda p: p[0])  # smallest chars within 1 se
            print(f"  D1 ceiling pick: {pin[3]} = {pin[0]} chars "
                  f"(best={best_arm}@{best_m:.2f}; smallest within 1 se)")
        else:
            print("  D1 ceiling pick: (no map-size arm data yet)")


if __name__ == "__main__":
    main()
