#!/usr/bin/env python3
"""Render the #548 autonomous-evaluator A/B/C charts as PNGs.

Reproducible: the run data is inlined below (the source of truth is the per-run
`results/{A,B,C}-*.md` records + EXPERIMENT.md). Re-run to regenerate:

    source ~/venv/bin/activate
    python scripts/eval/charts.py

Writes results/chart-*.png. No network, no args.
"""
from pathlib import Path

import matplotlib

matplotlib.use("Agg")  # headless: write files, never open a window
import matplotlib.pyplot as plt  # noqa: E402

OUT = Path(__file__).resolve().parent / "results"

# --- Run data (mirrors results/{A,B,C,D,E}-*.md) ---------------------------
# A/B/C vary the codebase (Exp.1); C/D/E vary the executor model (Exp.2).
RUNS = ["A", "B", "C", "D", "E"]
SUBTITLE = {
    "A": "68c9b2c\nbaseline",
    "B": "+#661\ncompaction",
    "C": "14b-coder\n(d25662d)",
    "D": "27b local\ngeneral",
    "E": "gpt-4.1\nfrontier",
}
TOP_DGX_SUBS = {"A": 8, "B": 8, "C": 8, "D": 8, "E": 8}  # lower=better, 0=PASS
BASELINE = 8  # unimplemented #548
TARGET = 0  # rolled up => PASS
WALLCLOCK_S = {"A": None, "B": 9103, "C": 7266, "D": 15170, "E": 13558}
LEAVES = {"A": 9, "B": 11, "C": 9, "D": 8, "E": 9}  # D stopped at 8/9
OUTCOME = {  # the distinct non-implementation mode per run
    "A": "orphan module",
    "B": "gutted README",
    "C": "no changes",
    "D": "Python-in-Rust",
    "E": "C#/Py/Go/C++",
}
RED, GREEN, GREY, BLUE = "#c0392b", "#27ae60", "#95a5a6", "#2c6fbb"


def _xlabels():
    return [f"{r}\n{SUBTITLE[r]}" for r in RUNS]


def chart_top_dgx_subs():
    """Headline: the grader metric is flat at the FAIL baseline across A–E."""
    fig, ax = plt.subplots(figsize=(9.5, 4.8))
    vals = [TOP_DGX_SUBS[r] for r in RUNS]
    bars = ax.bar(RUNS, vals, color=RED, width=0.6, zorder=3)
    ax.axhline(
        BASELINE, ls="--", lw=1.2, color=GREY, zorder=2,
        label=f"baseline / FAIL ({BASELINE})",
    )
    ax.axhline(
        TARGET, ls="-", lw=2.0, color=GREEN, zorder=2,
        label="target / PASS (0 = rolled up)",
    )
    for b, r in zip(bars, RUNS):
        ax.text(
            b.get_x() + b.get_width() / 2, b.get_height() - 0.5,
            f"FAIL\n{OUTCOME[r]}", ha="center", va="top",
            color="white", fontsize=8, fontweight="bold",
        )
    ax.set_xticks(range(len(RUNS)))
    ax.set_xticklabels(_xlabels(), fontsize=8.5)
    ax.set_ylabel("top-level /help  ·  /dgx subcommand lines")
    ax.set_ylim(0, 9)
    ax.set_title(
        "#548 grader: /dgx rollup not implemented in any run  (0 / 5)",
        fontweight="bold",
    )
    ax.legend(loc="upper right", framealpha=0.95, fontsize=8.5)
    ax.grid(axis="y", ls=":", alpha=0.4, zorder=0)
    fig.tight_layout()
    p = OUT / "chart-top-dgx-subs.png"
    fig.savefig(p, dpi=144)
    plt.close(fig)
    return p


def chart_wallclock():
    """Cost: wall-clock per run (A not captured). Bigger executors cost ~2x."""
    ORANGE = "#e08e0b"  # D: stopped before completing
    fig, ax = plt.subplots(figsize=(9.5, 4.8))
    mins = [(WALLCLOCK_S[r] / 60.0 if WALLCLOCK_S[r] else 0) for r in RUNS]
    colors = [
        GREY if WALLCLOCK_S[r] is None else (ORANGE if r == "D" else BLUE)
        for r in RUNS
    ]
    bars = ax.bar(RUNS, mins, color=colors, width=0.6, zorder=3)
    for b, r in zip(bars, RUNS):
        if WALLCLOCK_S[r] is None:
            ax.text(
                b.get_x() + b.get_width() / 2, 4, "not\ncaptured",
                ha="center", va="bottom", color="#555", fontsize=8.5,
            )
        else:
            tag = " stopped" if r == "D" else ""
            ax.text(
                b.get_x() + b.get_width() / 2, b.get_height() + 3,
                f"{b.get_height():.0f} min\n({LEAVES[r]} leaves{tag})",
                ha="center", va="bottom", fontsize=8.5, fontweight="bold",
            )
    ax.set_xticks(range(len(RUNS)))
    ax.set_xticklabels(_xlabels(), fontsize=8.5)
    ax.set_ylabel("wall-clock (minutes)")
    ax.set_ylim(0, max(m for m in mins) * 1.25 + 5)
    ax.set_title(
        "Run cost — per-leaf just-check dominates; 27B/frontier ~2x the 14B "
        "for no outcome gain",
        fontweight="bold", fontsize=10.5,
    )
    ax.grid(axis="y", ls=":", alpha=0.4, zorder=0)
    fig.tight_layout()
    p = OUT / "chart-wallclock.png"
    fig.savefig(p, dpi=144)
    plt.close(fig)
    return p


def chart_summary():
    """One-glance summary: loop completes 5/5, feature implemented 0/5."""
    fig, ax = plt.subplots(figsize=(7.5, 4.0))
    cats = ["loop\ncompleted", "wired real\nhelp_lines", "#548\nimplemented"]
    completed = [5, 0, 0]
    bars = ax.bar(cats, completed, color=[GREEN, RED, RED], width=0.5, zorder=3)
    for b, n in zip(bars, completed):
        ax.text(
            b.get_x() + b.get_width() / 2, b.get_height() + 0.1,
            f"{n} / 5", ha="center", va="bottom",
            fontsize=12, fontweight="bold",
        )
    ax.set_ylim(0, 5.6)
    ax.set_yticks([0, 1, 2, 3, 4, 5])
    ax.set_ylabel("runs (of 5)")
    ax.set_title(
        "Mechanically robust, functionally empty: 5/5 complete, 0/5 implement",
        fontweight="bold",
    )
    ax.grid(axis="y", ls=":", alpha=0.4, zorder=0)
    fig.tight_layout()
    p = OUT / "chart-summary.png"
    fig.savefig(p, dpi=144)
    plt.close(fig)
    return p


if __name__ == "__main__":
    for fn in (chart_summary, chart_top_dgx_subs, chart_wallclock):
        print(f"wrote {fn().relative_to(OUT.parent.parent.parent)}")
