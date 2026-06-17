#!/usr/bin/env python3
"""Crew MVP — the WALK step: real navigation + curated context on the actual newt repo.

Unlike crew.py (toy files), this targets a large real file (newt-core/src/config.rs,
~2k lines), so it forces the three real problems:
  - NAVIGATION: the navigator can't be handed the file — it picks files + search terms,
    the harness greps, and we extract only the relevant *spans* (struct + impl block).
  - CURATED CONTEXT: the planner sees ~80 curated lines, not the 2000-line file.
  - ITEM-LEVEL EDITS: the planner returns a new `impl` block the harness INSERTS after
    the located item — no full-file rewrite, no fragile exact-old-snippet matching.
Verified by a HARNESS-AUTHORED ground-truth test (the planner cannot Goodhart its own
check), built with `cargo test` against a shared warm target (~7s).

Usage:  python3 crew_repo.py <worktree>   (worktree from `git worktree add`, CARGO_TARGET_DIR shared)
"""
from __future__ import annotations
import json, ssl, sys, time, urllib.request, subprocess, os, re
from pathlib import Path

POOL = {"gnuc": {"url": "http://localhost:11434", "verify": True},
        "dgx":  {"url": "https://dgx-ollama.home.lab", "verify": False}}
ROLES = {"navigator": {"backend": "dgx", "model": "devstral-small-2:24b", "temp": 0.1},
         "planner":   {"backend": "dgx", "model": "qwen3-coder:30b",      "temp": 0.2},
         "triage":    {"backend": "gnuc", "model": "qwen2.5-coder:3b",    "temp": 0.0}}
TIMEOUT = 300
SWAPLOG: list[dict] = []
_NV = ssl.create_default_context(); _NV.check_hostname = False; _NV.verify_mode = ssl.CERT_NONE

WT = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("/tmp/crew-repo")
TARGET = "newt-core/src/config.rs"
CRATE = "newt-core"
SHARED_TARGET = os.environ.get("CARGO_TARGET_DIR", "")

# Harness-authored ground truth: appended to the target, fails to COMPILE until the
# crew implements Loadout::is_empty. The planner never sees/edits this test.
GROUND_TRUTH_TEST = """

#[cfg(test)]
mod crew_task_tests {
    use super::*;
    #[test]
    fn loadout_is_empty() {
        assert!(Loadout::default().is_empty(), "a default Loadout is empty");
        let l = Loadout { provider: Some("dgx".into()), ..Default::default() };
        assert!(!l.is_empty(), "a Loadout with a provider is not empty");
    }
}
"""
TASK = ("Add a method `pub fn is_empty(&self) -> bool` to the `Loadout` struct in "
        f"{TARGET}. It must return true exactly when ALL of the Loadout's axes "
        "(provider, model, kit, profile, role, settings) are None. A unit test "
        "`loadout_is_empty` already exists and must pass. Add the method in a NEW "
        "`impl Loadout { ... }` block; do not modify the existing impl or struct.")


def ask(role, system, user):
    r = ROLES[role]; be = POOL[r["backend"]]
    body = json.dumps({"model": r["model"],
                       "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
                       "stream": False, "format": "json", "options": {"temperature": r["temp"]}}).encode()
    print(f"  → {role:9s} {r['model']:22s} @ {r['backend']:4s} …", end="", flush=True)
    t0 = time.time()
    req = urllib.request.Request(be["url"] + "/api/chat", data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=TIMEOUT, context=None if be["verify"] else _NV) as resp:
        out = json.loads(resp.read())
    wall = time.time() - t0; load = out.get("load_duration", 0) / 1e9; tot = out.get("total_duration", 0) / 1e9
    SWAPLOG.append({"role": role, "backend": r["backend"], "wall": round(wall, 1), "load": round(load, 1)})
    print(f" {wall:5.1f}s  (load {load:4.1f}s, gen {tot-load:4.1f}s)", flush=True)
    c = out.get("message", {}).get("content", "")
    try:
        return json.loads(c)
    except json.JSONDecodeError:
        i, j = c.find("{"), c.rfind("}")
        return json.loads(c[i:j + 1])


def extract_item(lines, pattern):
    """Return (span_text, start_idx, end_idx) of the brace-balanced item whose head matches `pattern`."""
    rx = re.compile(pattern)
    for i, ln in enumerate(lines):
        if rx.search(ln):
            depth, started, out = 0, False, []
            for j in range(i, min(i + 250, len(lines))):
                out.append(lines[j]); depth += lines[j].count("{") - lines[j].count("}")
                if "{" in lines[j]:
                    started = True
                if started and depth <= 0:
                    return "\n".join(out), i, j
            return "\n".join(out), i, len(lines) - 1
    return "", -1, -1


def cargo_test():
    env = dict(os.environ);
    if SHARED_TARGET:
        env["CARGO_TARGET_DIR"] = SHARED_TARGET
    p = subprocess.run(["cargo", "test", "-p", CRATE, "loadout_is_empty"],
                       cwd=WT, capture_output=True, text=True, env=env, timeout=600)
    ok = p.returncode == 0 and "test result: ok" in p.stdout
    return ok, (p.stdout + p.stderr)


def main() -> int:
    cfg = WT / TARGET
    print(f"\nTASK: {TASK}\n\nworktree: {WT}\ntarget:   {TARGET} ({len(cfg.read_text().splitlines())} lines)\n")
    # Harness sets up the ground-truth test (compile-fails until is_empty exists).
    cfg.write_text(cfg.read_text() + GROUND_TRUTH_TEST)
    print("harness: appended ground-truth test `loadout_is_empty` (planner never sees it)\n")

    # 1. NAVIGATE — pick file + search terms from the crate's file list (real search, no file dump).
    files = sorted(str(p.relative_to(WT)) for p in (WT / "newt-core" / "src").glob("*.rs"))
    nav = ask("navigator",
              "You are a repo navigator. Given a task and a crate's source files, return JSON: "
              '{"target_file":"path","search_terms":["rg pattern", ...],"reasoning":"...","risks":["..."]}. '
              "search_terms are ripgrep patterns that locate the exact items to edit.",
              f"TASK:\n{TASK}\n\nFILES in newt-core/src:\n{files}")
    tf = nav.get("target_file", TARGET)
    print(f"     navigator → {tf}  terms={nav.get('search_terms')}  risks={nav.get('risks')}\n")

    # 2. CURATE — extract only the struct + impl spans, not the whole file.
    lines = (WT / tf).read_text().splitlines() if (WT / tf).exists() else cfg.read_text().splitlines()
    struct_span, _, _ = extract_item(lines, r"pub struct Loadout\b")
    impl_span, _, impl_end = extract_item(lines, r"^impl Loadout\b")
    curated = f"=== struct Loadout (fields) ===\n{struct_span}\n\n=== impl Loadout (existing) ===\n{impl_span}"
    print(f"     curated context: {len(curated.splitlines())} lines (vs {len(lines)} in the file)\n")

    failures = []
    for attempt in range(1, 3):
        print(f"── attempt {attempt}/2 " + "─" * 44)
        prior = ("\n\nPrevious attempt FAILED. Triage:\n" + json.dumps(failures[-1], indent=2)
                 + "\nReturn a corrected new_impl_block.") if failures else ""
        plan = ask("planner",
                   "You are a senior Rust engineer. Given a task and curated context (a struct + its "
                   "existing impl), return JSON: {\"plan\":[\"...\"],\"new_impl_block\":\"a COMPLETE new "
                   "`impl Loadout { ... }` block containing ONLY the new method\"}. Use the exact field "
                   "names from the struct. Do not restate the existing impl.",
                   f"TASK:\n{TASK}\n\nCURATED CONTEXT:\n{curated}{prior}")
        block = plan.get("new_impl_block", "").strip()
        print(f"     plan: {plan.get('plan')}")
        # 3. APPLY — insert the new impl block right after the existing impl Loadout item.
        cur = (WT / tf).read_text().splitlines()
        _, _, end = extract_item(cur, r"^impl Loadout\b")
        cur[end + 1:end + 1] = ["", *block.splitlines()]
        (WT / tf).write_text("\n".join(cur) + "\n")
        print(f"     inserted new impl block ({len(block.splitlines())} lines) after the existing impl Loadout")

        ok, output = cargo_test()
        print(f"     cargo test -p {CRATE} loadout_is_empty → {'PASS' if ok else 'FAIL'}")
        if ok:
            print("\n✅ CREW SUCCEEDED on the real repo\n")
            diff = subprocess.run(["git", "-C", str(WT), "diff", "--", tf], capture_output=True, text=True).stdout
            print(diff[-1400:]); report("passed", attempt); return 0
        # 4. TRIAGE the compile/test error on gnuc, then revise.
        errs = "\n".join(l for l in output.splitlines() if "error" in l.lower())[:1200] or output[-1200:]
        tri = ask("triage",
                  "You are a build cop for Rust. Given compiler/test errors, return JSON: "
                  '{"summary":"...","likely_cause":"...","next_action":"..."}. Terse.',
                  f"TASK:\n{TASK}\n\ncargo errors:\n{errs}")
        print(f"     triage: {tri.get('summary','')[:110]} → {tri.get('next_action','')[:80]}")
        failures.append(tri)
        # reset the file for the next attempt (drop the bad insert; keep the ground-truth test)
        subprocess.run(["git", "-C", str(WT), "checkout", "--", tf], capture_output=True)
        cfg.write_text(cfg.read_text() + GROUND_TRUTH_TEST)

    print("\n⚠️  needs human review\n"); report("needs_human_review", 2); return 1


def report(status, attempts):
    print("── findings " + "─" * 50)
    print(f"  status: {status}   attempts: {attempts}   model calls: {len(SWAPLOG)}")
    for s in SWAPLOG:
        print(f"  {s['role']:9s} {s['backend']:4s} wall={s['wall']:.1f}s load={s['load']:.1f}s")
    loads = [s for s in SWAPLOG if s["load"] > 1.5]
    detail = ", ".join("{}:{:.0f}s".format(s["role"], s["load"]) for s in loads) or "0 (warm, zero-swap)"
    print(f"  model (re)loads >1.5s: {len(loads)} — {detail}")
    print(f"  worktree: {WT}")


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:
        print(f"\n💥 {type(e).__name__}: {e}", file=sys.stderr); sys.exit(2)
