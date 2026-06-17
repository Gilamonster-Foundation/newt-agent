#!/usr/bin/env python3
"""Crew loadout MVP — the boring two-pass coding machine (crawl step).

Drives THREE real models across the gnuc+DGX pool to expose the real problems
(JSON adherence, edit validity, model-load/swap latency, convergence) before any
Rust newt-scheduler crate exists. The harness owns the loop; the models are
workers, each given one role and bounded context. Never writes to the real repo —
all edits land in an isolated git worktree under /tmp.

Placement (forced by gnuc = 16GB RTX 4060 Ti, which cannot hold a 30B model):
  navigator  devstral-small-2:24b   @ DGX
  planner    qwen3-coder:30b        @ DGX   (co-resident with navigator on the big box)
  triage     qwen2.5-coder:3b       @ gnuc  (tiny, runs in parallel with the DGX)

Ollama returns load_duration per call — that IS the model-swap cost, measured.
"""
from __future__ import annotations
import json, ssl, sys, time, urllib.request, subprocess, tempfile, shutil
from pathlib import Path

# ── pool + roles ────────────────────────────────────────────────────────────
POOL = {
    "gnuc": {"url": "http://localhost:11434", "verify": True},
    "dgx":  {"url": "https://dgx-ollama.home.lab", "verify": False},
}
ROLES = {
    "navigator": {"backend": "dgx",  "model": "devstral-small-2:24b", "temp": 0.1},
    "planner":   {"backend": "dgx",  "model": "qwen3-coder:30b",      "temp": 0.2},
    "triage":    {"backend": "gnuc", "model": "qwen2.5-coder:3b",     "temp": 0.0},
}
TIMEOUT = 240
SWAPLOG: list[dict] = []  # the empirical record: load + total durations per call

_NOVERIFY = ssl.create_default_context()
_NOVERIFY.check_hostname = False
_NOVERIFY.verify_mode = ssl.CERT_NONE


def ask(role: str, system: str, user: str) -> dict:
    """One structured chat turn to a role's model; returns parsed JSON + records timing."""
    r = ROLES[role]
    be = POOL[r["backend"]]
    body = json.dumps({
        "model": r["model"],
        "messages": [{"role": "system", "content": system},
                     {"role": "user", "content": user}],
        "stream": False,
        "format": "json",
        "options": {"temperature": r["temp"]},
    }).encode()
    print(f"  → {role:9s} {r['model']:22s} @ {r['backend']:4s} …", end="", flush=True)
    t0 = time.time()
    req = urllib.request.Request(be["url"] + "/api/chat", data=body,
                                 headers={"Content-Type": "application/json"})
    ctx = None if be["verify"] else _NOVERIFY
    with urllib.request.urlopen(req, timeout=TIMEOUT, context=ctx) as resp:
        out = json.loads(resp.read())
    wall = time.time() - t0
    load_ms = out.get("load_duration", 0) / 1e6
    total_ms = out.get("total_duration", 0) / 1e6
    SWAPLOG.append({"role": role, "model": r["model"], "backend": r["backend"],
                    "wall_s": round(wall, 1), "load_ms": round(load_ms), "total_ms": round(total_ms)})
    print(f" {wall:5.1f}s  (model-load {load_ms/1000:4.1f}s, gen {(total_ms-load_ms)/1000:4.1f}s)", flush=True)
    content = out.get("message", {}).get("content", "")
    try:
        return json.loads(content)
    except json.JSONDecodeError:
        i, j = content.find("{"), content.rfind("}")
        if i != -1 and j != -1:
            return json.loads(content[i:j + 1])
        raise


# ── the target(s): tiny repos with distractors and one failing test ─────────
TASKS = {
    "fib": {
        "task": ("Implement the `fib` function in mathy.py so every assertion in "
                 "test_mathy.py passes. fib(0)=0, fib(1)=1, fib(n)=fib(n-1)+fib(n-2). "
                 "Do not change any other file or the existing `add` function."),
        "test": ["python3", "test_mathy.py"],
        "files": {
            "mathy.py": "def add(a, b):\n    return a + b\n\n\ndef fib(n):\n    raise NotImplementedError(\"fib is not implemented yet\")\n",
            "test_mathy.py": (
                "from mathy import add, fib\n\n"
                "def main():\n"
                "    assert add(2, 3) == 5\n"
                "    expected = [0, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55]\n"
                "    got = [fib(i) for i in range(11)]\n"
                "    assert got == expected, f'fib wrong: {got}'\n"
                "    print('ALL TESTS PASSED')\n\n"
                "if __name__ == '__main__':\n    main()\n"
            ),
            "utils.py": "def slugify(s):\n    return s.lower().replace(' ', '-')\n",
            "config.py": "DEBUG = False\nMAX_RETRIES = 4\n",
            "README.md": "# sample project\n\nA tiny package used by the crew-MVP harness.\n",
        },
    },
    # A task more likely to need a revise (subtractive Roman notation is easy to
    # under-implement first pass) — exercises the triage(gnuc) + revise leg.
    "roman": {
        "task": ("Implement `int_to_roman(n)` in roman.py to convert an integer 1..3999 to a "
                 "Roman numeral string, INCLUDING subtractive notation (4=IV, 9=IX, 40=XL, "
                 "90=XC, 400=CD, 900=CM). Make every assertion in test_roman.py pass. "
                 "Change only roman.py."),
        "test": ["python3", "test_roman.py"],
        "files": {
            "roman.py": "def int_to_roman(n):\n    raise NotImplementedError\n",
            "test_roman.py": (
                "from roman import int_to_roman\n\n"
                "CASES = {1:'I',3:'III',4:'IV',9:'IX',14:'XIV',40:'XL',90:'XC',"
                "400:'CD',900:'CM',944:'CMXLIV',1994:'MCMXCIV',3888:'MMMDCCCLXXXVIII'}\n\n"
                "def main():\n"
                "    for n, want in CASES.items():\n"
                "        got = int_to_roman(n)\n"
                "        assert got == want, f'int_to_roman({n}) = {got!r}, want {want!r}'\n"
                "    print('ALL TESTS PASSED')\n\n"
                "if __name__ == '__main__':\n    main()\n"
            ),
            "helpers.py": "GREETING = 'hello'\n",
            "README.md": "# roman sample\n",
        },
    },
}
_SEL = sys.argv[1] if len(sys.argv) > 1 else "fib"
_T = TASKS[_SEL]
SAMPLE, TASK, TEST_CMD = _T["files"], _T["task"], _T["test"]


def setup_worktree() -> Path:
    wt = Path(tempfile.mkdtemp(prefix="crew-mvp-"))
    for name, content in SAMPLE.items():
        (wt / name).write_text(content)
    subprocess.run(["git", "init", "-q"], cwd=wt)
    subprocess.run(["git", "add", "-A"], cwd=wt)
    subprocess.run(["git", "-c", "user.email=crew@local", "-c", "user.name=crew",
                    "commit", "-q", "-m", "baseline"], cwd=wt)
    return wt


def run_test(wt: Path):
    p = subprocess.run(TEST_CMD, cwd=wt, capture_output=True, text=True, timeout=30)
    return p.returncode == 0, (p.stdout + p.stderr).strip()


def apply_edits(wt: Path, edits: list[dict]) -> list[str]:
    touched = []
    for e in edits:
        path = (wt / e["path"]).resolve()
        if wt not in path.parents and path != wt:   # worktree fence
            print(f"     ! refused out-of-worktree edit: {e['path']}")
            continue
        path.write_text(e["new_content"])
        touched.append(e["path"])
    return touched


# ── the control loop ────────────────────────────────────────────────────────
def main() -> int:
    print(f"\nTASK: {TASK}\n")
    wt = setup_worktree()
    tree = sorted(p.name for p in wt.iterdir() if p.is_file())
    print(f"worktree: {wt}\nfiles: {tree}\n")

    # 1. NAVIGATOR — which files matter?
    nav = ask("navigator",
              "You are a repo navigator. Given a task and a file list, return ONLY the files "
              'relevant to the task. Reply as JSON: {"relevant_files": ["path", ...], "reasoning": "..."}.',
              f"TASK:\n{TASK}\n\nFILES:\n{tree}")
    rel = [f for f in nav.get("relevant_files", []) if (wt / f).exists()]
    print(f"     navigator picked: {rel}  ({nav.get('reasoning','')[:80]})\n")
    if not rel:
        rel = ["mathy.py", "test_mathy.py"]

    ctx_files = {f: (wt / f).read_text() for f in rel}
    failures = []
    MAX_ATTEMPTS = 2

    for attempt in range(1, MAX_ATTEMPTS + 1):
        print(f"── attempt {attempt}/{MAX_ATTEMPTS} " + "─" * 40)
        ctx = "\n\n".join(f"=== {f} ===\n{c}" for f, c in ctx_files.items())
        prior = ""
        if failures:
            prior = ("\n\nThe previous attempt FAILED. Triage report:\n"
                     + json.dumps(failures[-1], indent=2)
                     + "\nFix it. Return the full corrected file content.")
        plan = ask("planner",
                   "You are a senior engineer. Given a task and the relevant files, return a patch as "
                   'JSON: {"plan": ["step", ...], "edits": [{"path": "file", "new_content": "FULL new file content"}]}. '
                   "Emit the COMPLETE new content for each file you change. Change only what the task asks.",
                   f"TASK:\n{TASK}\n\nRELEVANT FILES:\n{ctx}{prior}")
        edits = plan.get("edits", [])
        print(f"     plan: {plan.get('plan', [])}")
        touched = apply_edits(wt, edits)
        print(f"     applied edits to: {touched}")
        for f in touched:                       # refresh context with what we wrote
            if (wt / f).exists():
                ctx_files[f] = (wt / f).read_text()

        ok, output = run_test(wt)
        print(f"     test `{' '.join(TEST_CMD)}` → {'PASS' if ok else 'FAIL'}")
        if ok:
            print("\n✅ CREW SUCCEEDED\n")
            diff = subprocess.run(["git", "diff", "HEAD"], cwd=wt, capture_output=True, text=True).stdout
            print(diff)
            report(wt, status="passed", attempts=attempt)
            return 0

        # 3. TRIAGE — compress the failure for the planner
        tri = ask("triage",
                  "You are a build cop. Given a failing test command output and the diff, return JSON: "
                  '{"status":"failed","summary":"...","likely_cause":"...","next_action":"..."}. Be terse.',
                  f"TASK:\n{TASK}\n\nTEST OUTPUT:\n{output[:1500]}\n\nFILES NOW:\n"
                  + "\n".join(f"=== {f} ===\n{c}" for f, c in ctx_files.items()))
        print(f"     triage: {tri.get('summary','')[:100]}  → {tri.get('next_action','')[:80]}")
        failures.append(tri)

    print("\n⚠️  needs human review — budget exhausted\n")
    report(wt, status="needs_human_review", attempts=MAX_ATTEMPTS)
    return 1


def report(wt: Path, status: str, attempts: int):
    print("── empirical findings " + "─" * 43)
    print(f"  status: {status}   attempts: {attempts}   model calls: {len(SWAPLOG)}")
    print(f"  {'role':9s} {'backend':5s} {'wall':>6s} {'load':>7s} {'gen':>7s}")
    for s in SWAPLOG:
        gen = (s["total_ms"] - s["load_ms"]) / 1000
        print(f"  {s['role']:9s} {s['backend']:5s} {s['wall_s']:5.1f}s {s['load_ms']/1000:6.1f}s {gen:6.1f}s")
    swaps = [s for s in SWAPLOG if s["load_ms"] > 1500]   # >1.5s load ⇒ a (re)load happened
    print(f"  model (re)loads >1.5s (the swap cost): {len(swaps)} — "
          + ", ".join(f"{s['role']}:{s['load_ms']/1000:.0f}s" for s in swaps) if swaps
          else "  model (re)loads >1.5s: 0 (all resident — zero-swap happy path)")
    print(f"  worktree kept for inspection: {wt}")


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:
        print(f"\n💥 harness error: {type(e).__name__}: {e}", file=sys.stderr)
        if SWAPLOG:
            report(Path("/tmp"), status="harness_error", attempts=0)
        sys.exit(2)
