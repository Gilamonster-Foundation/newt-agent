#!/usr/bin/env python3
"""Self-tests for spec/lint-behavior-map.py — proves the registry linter is exact
and fail-closed via fixture trees + negative cases. Run:

    python3 spec/test-lint-behavior-map.py
"""
from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
LINTER = HERE / "lint-behavior-map.py"

# A minimal fixture that fully RESOLVES; each negative case mutates one piece.
BASE_LEAN = "namespace A\ntheorem t : True := trivial\nend A\n"
BASE_RUST = "mod m {\n    #[test]\n    fn foo() {}\n}\npub fn prod() {}\n"
BASE_MAP = """\
schema = 3
[BHV-X-001]
description = "a resolving contract"
[BHV-X-001.status]
lean = "proven"
rust = "tested"
tla = "none"
trace = "none"
conformance = "partial"
[[BHV-X-001.refs.lean]]
module = "A"
symbol = "t"
[[BHV-X-001.refs.rust_tests]]
path = "r.rs"
symbol = "m::foo"
[[BHV-X-001.refs.production]]
path = "r.rs"
symbol = "prod"
"""

_pass = 0
_fail = 0


def run_case(name, *, mapping=BASE_MAP, lean=BASE_LEAN, rust=BASE_RUST,
             tla=None, strict=False, want_exit, want_msg=None):
    """tla: optional {spec_name: (tla_text, cfg_text_or_None)} written under
    spec/tla/ so the TLA reference validator can be exercised."""
    global _pass, _fail
    with tempfile.TemporaryDirectory() as d:
        repo = Path(d)
        (repo / "formal" / "NewtPolicy").mkdir(parents=True)
        (repo / "formal" / "NewtPolicy" / "Fix.lean").write_text(lean)
        (repo / "r.rs").write_text(rust)
        (repo / "map.toml").write_text(mapping)
        if tla:
            (repo / "spec" / "tla").mkdir(parents=True)
            for spec, (tla_text, cfg_text) in tla.items():
                (repo / "spec" / "tla" / f"{spec}.tla").write_text(tla_text)
                if cfg_text is not None:
                    (repo / "spec" / "tla" / f"{spec}.cfg").write_text(cfg_text)
        cmd = [sys.executable, str(LINTER), "--map", str(repo / "map.toml"),
               "--repo", str(repo), "--lean-dir", str(repo / "formal" / "NewtPolicy")]
        if strict:
            cmd.append("--strict")
        r = subprocess.run(cmd, capture_output=True, text=True)
        out = r.stdout + r.stderr
        ok = (r.returncode == want_exit) and (want_msg is None or want_msg in out)
        if ok:
            print(f"ok   - {name}")
            _pass += 1
        else:
            print(f"FAIL - {name}: exit={r.returncode} (want {want_exit})"
                  + (f", missing {want_msg!r}" if want_msg and want_msg not in out else ""))
            print("       " + out.replace("\n", "\n       ").strip())
            _fail += 1


# 0. Positive control: everything resolves → exit 0.
run_case("all references resolve", want_exit=0)

# 1. Wrong Lean namespace with the SAME theorem basename elsewhere.
run_case("wrong lean namespace (same basename under B) fails",
         lean="namespace B\ntheorem t : True := trivial\nend B\n",
         want_exit=1, want_msg="lean A::t does not resolve")

# 2. Wrong Rust module with the SAME test basename elsewhere (foo IS a test, but
#    under `b`, not the referenced `m`) → module mismatch, not attribute mismatch.
run_case("wrong rust module (foo only under b) fails",
         rust="mod b {\n    #[test]\n    fn foo() {}\n}\npub fn prod() {}\n",
         want_exit=1, want_msg="rust_tests r.rs::m::foo does not resolve")

# 3. Missing production symbol in an existing file (foo stays a valid test).
run_case("missing production symbol fails",
         rust="mod m {\n    #[test]\n    fn foo() {}\n}\n",  # no `prod`
         want_exit=1, want_msg="production r.rs::prod does not resolve")

# 4. Invalid status value.
run_case("invalid status value fails",
         mapping=BASE_MAP.replace('lean = "proven"', 'lean = "bogus"'),
         want_exit=1, want_msg="status.lean = 'bogus' is invalid")

# 5. A missing reference (no pending marker) is a hard error.
run_case("missing non-pending reference fails",
         mapping=BASE_MAP.replace('symbol = "prod"', 'symbol = "does_not_exist"'),
         want_exit=1, want_msg="production r.rs::does_not_exist does not resolve")

# 6. Ambiguous (multi-match) resolution fails.
run_case("ambiguous production symbol fails",
         rust="mod m {\n    fn foo() {}\n}\npub fn prod() {}\nfn prod() {}\n",
         want_exit=1, want_msg="AMBIGUOUS")

# 7. Invalid conformance = "full" (prerequisite layers not represented).
run_case("conformance=full without prerequisites fails",
         mapping=BASE_MAP.replace('conformance = "partial"', 'conformance = "full"'),
         want_exit=1, want_msg="conformance = 'full' requires")

# 8. rust = "tested" with no rust refs fails.
NO_RUST_REFS = """\
schema = 3
[BHV-Y-001]
description = "tested but no rust refs"
[BHV-Y-001.status]
lean = "none"
rust = "tested"
tla = "none"
trace = "none"
conformance = "partial"
"""
run_case("rust=tested with no rust refs fails", mapping=NO_RUST_REFS,
         want_exit=1, want_msg="status.rust = 'tested' but no `refs.rust_tests`")

# 9. lean = "proven" with no lean refs fails.
NO_LEAN_REFS = NO_RUST_REFS.replace('rust = "tested"', 'rust = "none"').replace(
    'lean = "none"', 'lean = "proven"')
run_case("lean=proven with no lean refs fails", mapping=NO_LEAN_REFS,
         want_exit=1, want_msg="no `refs.lean`")

# 10. tla = "checked" with no tla ref fails.
TLA_CHECKED = NO_RUST_REFS.replace('rust = "tested"', 'rust = "none"').replace(
    'tla = "none"', 'tla = "checked"')
run_case("tla=checked with no tla ref fails", mapping=TLA_CHECKED,
         want_exit=1, want_msg="status.tla = 'checked' but no `refs.tla`")

# 11. Missing description fails.
NO_DESC = BASE_MAP.replace('description = "a resolving contract"\n', "")
run_case("missing description fails", mapping=NO_DESC,
         want_exit=1, want_msg="missing non-empty `description`")

# 12. Duplicate reference within a contract fails.
DUP_REF = BASE_MAP + '[[BHV-X-001.refs.production]]\npath = "r.rs"\nsymbol = "prod"\n'
run_case("duplicate reference fails", mapping=DUP_REF,
         want_exit=1, want_msg="duplicate production reference")

# 13. A pending_pr reference that is missing WARNS (exit 0) but FAILS under --strict.
PENDING = BASE_MAP.replace(
    '[[BHV-X-001.refs.production]]\npath = "r.rs"\nsymbol = "prod"\n',
    '[[BHV-X-001.refs.production]]\npath = "missing.rs"\nsymbol = "later"\npending_pr = 999\n')
run_case("pending missing ref warns (exit 0)", mapping=PENDING,
         want_exit=0, want_msg="pending PR #999")
run_case("pending missing ref fails under --strict", mapping=PENDING, strict=True,
         want_exit=1, want_msg="pending PR #999")

# 14. The linter must not satisfy a ref by matching a string in prose/docs: the
#     symbol appears ONLY in a comment, not as a definition → fails.
run_case("comment-only symbol does not satisfy a production ref",
         rust="mod m {\n    #[test]\n    fn foo() {}\n}\n// pub fn prod is described here but not defined\n",
         want_exit=1, want_msg="production r.rs::prod does not resolve")

# 15. MG2: a fn WITHOUT a recognized test attribute cannot satisfy a rust_tests ref
#     (deleting `#[test]` orphans the ref, even though the fn still exists).
run_case("rust fn without a test attribute does not satisfy a rust_tests ref",
         rust="mod m {\n    fn foo() {}\n}\npub fn prod() {}\n",
         want_exit=1, want_msg="rust_tests r.rs::m::foo does not resolve")

# 16. MG2: `#[tokio::test]` (async fn) IS recognized — the positive control for the
#     attribute requirement, exercising the tail-segment rule + modifier skip.
run_case("tokio::test on an async fn satisfies a rust_tests ref",
         rust="mod m {\n    #[tokio::test]\n    async fn foo() {}\n}\npub fn prod() {}\n",
         want_exit=0)

# 16b. Regression: Rust LIFETIMES ('a, &'a) must not be lexed as char literals — a
#      char-literal misread eats to the next `'`, blanking `fn`/`mod`/`{` so the
#      symbol after a lifetime silently "does not resolve". Char/byte literals in
#      the same file must still parse. (Found by running the linter on real code.)
LIFETIME_RUST = (
    "pub fn helper<'a>(x: &'a str) -> &'a str { x }\n"
    "fn eats_quotes() { let _ = ('a', b'z', '\\''); }\n"
    "mod m {\n    #[test]\n    fn foo() {}\n}\n"
    "pub fn prod() {}\n"
)
run_case("lifetimes are not char literals (symbol after a lifetime still resolves)",
         rust=LIFETIME_RUST, want_exit=0)

# ── TLA reference validator (item 4: exact, pre-AgentTurn) ───────────────────
# A contract that declares tla = "checked" and points at a real spec/invariant.
TLA_MAP = """\
schema = 3
[BHV-T-001]
description = "a checked tla contract"
[BHV-T-001.status]
lean = "none"
rust = "none"
tla = "checked"
trace = "none"
conformance = "partial"
[[BHV-T-001.refs.tla]]
spec = "Agent"
invariant = "Bounded"
"""
GOOD_TLA = "---- MODULE Agent ----\nBounded == TRUE\n===="
GOOD_CFG = "SPECIFICATION Spec\nINVARIANT Bounded\n"

# 17. A fully-resolving tla ref (operator defined + declared in the .cfg) → exit 0.
run_case("tla ref resolves when operator is defined and declared as INVARIANT",
         mapping=TLA_MAP, tla={"Agent": (GOOD_TLA, GOOD_CFG)}, want_exit=0)

# 18. Missing `invariant` field fails (a spec alone proves nothing is checked).
run_case("tla ref without an invariant fails",
         mapping=TLA_MAP.replace('invariant = "Bounded"\n', ""),
         tla={"Agent": (GOOD_TLA, GOOD_CFG)},
         want_exit=1, want_msg="needs `invariant`")

# 19. Invariant not defined as an operator in the .tla fails.
run_case("tla ref whose invariant is not an operator in the module fails",
         mapping=TLA_MAP, tla={"Agent": ("---- MODULE Agent ----\nOther == TRUE\n====", GOOD_CFG)},
         want_exit=1, want_msg="not defined as an operator")

# 20. Missing .cfg fails.
run_case("tla ref with no matching .cfg fails",
         mapping=TLA_MAP, tla={"Agent": (GOOD_TLA, None)},
         want_exit=1, want_msg="has no spec/tla/Agent.cfg")

# 21. Operator defined but NOT declared as an INVARIANT in the .cfg fails
#     (TLC would parse it but never check it).
run_case("tla ref whose invariant is not declared in the .cfg fails",
         mapping=TLA_MAP, tla={"Agent": (GOOD_TLA, "SPECIFICATION Spec\n")},
         want_exit=1, want_msg="not declared as an INVARIANT")

print(f"\n{_pass} passed, {_fail} failed")
sys.exit(1 if _fail else 0)
