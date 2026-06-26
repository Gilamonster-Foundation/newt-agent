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

# --- Run data (mirrors results/{A,B,C}-*.md) -------------------------------
RUNS = ["A", "B", "C"]
SUBTITLE = {
    "A": "68c9b2c\nbaseline",
    "B": "+#661\ncompaction",
    "C": "+#669\nworkspace-API",
}
TOP_DGX_SUBS = {"A": 8, "B": 8, "C": 8}  # grader: lower=better, 0=PASS
BASELINE = 8  # unimplemented #548
TARGET = 0  # rolled up => PASS
WALLCLOCK_S = {"A": None, "B": 9103, "C": 7266}  # A not captured
LEAVES = {"A": 9, "B": 11, "C": 9}
OUTCOME = {  # the distinct non-implementation mode per run
    "A": "orphan module",
    "B": "gutted README",
    "C": "no changes",
}
RED, GREEN, GREY, BLUE = "#c0392b", "#27ae60", "#95a5a6", "#2c6fbb"


def _xlabels():
    return [f"{r}\n{SUBTITLE[r]}" for r in RUNS]


def chart_top_dgx_subs():
    """Headline: the grader metric is flat at the FAIL baseline across A/B/C."""
    fig, ax = plt.subplots(figsize=(7.5, 4.6))
    vals = [TOP_DGX_SUBS[r] for r in RUNS]
    bars = ax.bar(RUNS, vals, color=RED, width=0.55, zorder=3)
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
            b.get_x() + b.get_width() / 2, b.get_height() - 0.55,
            f"FAIL\n{OUTCOME[r]}", ha="center", va="top",
            color="white", fontsize=8.5, fontweight="bold",
        )
    ax.set_xticks(range(len(RUNS)))
    ax.set_xticklabels(_xlabels(), fontsize=9)
    ax.set_ylabel("top-level /help  ·  /dgx subcommand lines")
    ax.set_ylim(0, 9)
    ax.set_title(
        "#548 grader: /dgx rollup not implemented in any run  (0 / 3)",
        fontweight="bold",
    )
    ax.legend(loc="center right", framealpha=0.95, fontsize=8.5)
    ax.grid(axis="y", ls=":", alpha=0.4, zorder=0)
    fig.tight_layout()
    p = OUT / "chart-top-dgx-subs.png"
    fig.savefig(p, dpi=144)
    plt.close(fig)
    return p


def chart_wallclock():
    """Cost: wall-clock per run (A not captured)."""
    fig, ax = plt.subplots(figsize=(7.5, 4.6))
    mins = [(WALLCLOCK_S[r] / 60.0 if WALLCLOCK_S[r] else 0) for r in RUNS]
    colors = [GREY if WALLCLOCK_S[r] is None else BLUE for r in RUNS]
    bars = ax.bar(RUNS, mins, color=colors, width=0.55, zorder=3)
    for b, r in zip(bars, RUNS):
        if WALLCLOCK_S[r] is None:
            ax.text(
                b.get_x() + b.get_width() / 2, 1.5, "not\ncaptured",
                ha="center", va="bottom", color="#555", fontsize=8.5,
            )
        else:
            ax.text(
                b.get_x() + b.get_width() / 2, b.get_height() + 1.5,
                f"{b.get_height():.0f} min\n({LEAVES[r]} leaves)",
                ha="center", va="bottom", fontsize=8.5, fontweight="bold",
            )
    ax.set_xticks(range(len(RUNS)))
    ax.set_xticklabels(_xlabels(), fontsize=9)
    ax.set_ylabel("wall-clock (minutes)")
    ax.set_ylim(0, max(m for m in mins) * 1.25 + 5)
    ax.set_title(
        "Autonomous run cost  —  ~2-2.5 h, dominated by per-leaf just-check builds",
        fontweight="bold",
    )
    ax.grid(axis="y", ls=":", alpha=0.4, zorder=0)
    fig.tight_layout()
    p = OUT / "chart-wallclock.png"
    fig.savefig(p, dpi=144)
    plt.close(fig)
    return p


def chart_summary():
    """One-glance summary: loop completes 3/3, feature implemented 0/3."""
    fig, ax = plt.subplots(figsize=(7.5, 4.0))
    cats = ["loop\ncompleted", "wired real\nhelp_lines", "#548\nimplemented"]
    completed = [3, 0, 0]
    bars = ax.bar(cats, completed, color=[GREEN, RED, RED], width=0.5, zorder=3)
    for b, n in zip(bars, completed):
        ax.text(
            b.get_x() + b.get_width() / 2, b.get_height() + 0.06,
            f"{n} / 3", ha="center", va="bottom",
            fontsize=12, fontweight="bold",
        )
    ax.set_ylim(0, 3.5)
    ax.set_yticks([0, 1, 2, 3])
    ax.set_ylabel("runs (of 3)")
    ax.set_title(
        "Mechanically robust, functionally empty: 3/3 complete, 0/3 implement",
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
