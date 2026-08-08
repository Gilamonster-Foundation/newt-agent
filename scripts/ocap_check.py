#!/usr/bin/env python3
"""ocap-check — the OCAP deviation honesty gate (first cut).

The authority plane of the Centaur swarm operates by a *deviation ratchet*
(docs/design/centaur-swarm-architecture.md, docs/security/ocap-deviations.md):

    effective authority = meet( the human's grant ,
                                what the currently-verified invariants enforce )

A *deviation* is an OCAP invariant that is not yet enforced. While it is open, the
dangerous capabilities that depend on it must stay FAIL-CLOSED. This script is the
gate that keeps that honest — the analog of `cov-ci`, but it ratchets deviations
CLOSED instead of coverage UP. It does three things:

  1. REGISTER INTEGRITY — every full deviation entry in the register carries its
     required fields (no half-specified, silently-incomplete caveats).

  2. CODE GUARD (the teeth) — any dangerous operation in the tree self-declares with
     an `OCAP-DANGER: <id>` marker. If that deviation is OPEN, the same site must also
     carry an `OCAP-GATE: <id>` marker (the runtime fail-closed check, e.g. a
     `verify_b1()` that refuses unless the invariant holds). So you CANNOT add
     credential-seeding later without either closing `b1-os-isolation` or wiring its
     gate — the build refuses. Closing a deviation is the ratchet click that frees
     its capabilities.

  3. HONEST LEDGER — print the open deviations + severities + what each disables, so
     the degraded state is never silent.

First cut: with no dangerous code yet, the guard passes trivially — but the
convention is armed and the register is enforced. As the captured shell / credential
code lands, the guards gain real teeth. Exit nonzero on any violation.

Usage:  python3 scripts/ocap_check.py        (or `just ocap-check`)
"""

from __future__ import annotations
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
REGISTER = REPO / "docs" / "security" / "ocap-deviations.md"

# Fields every full `### <id>` deviation entry must carry. A missing field is a
# silently-incomplete caveat — the thing this gate exists to forbid.
REQUIRED_FIELDS = [
    "Invariant",
    "Residual",
    "Disabled while open",
    "Closure criterion",
    "Ratchet guard",
    "Status",
]

# How far below an OCAP-DANGER marker we look for its matching OCAP-GATE.
GATE_PROXIMITY_LINES = 20

# Source trees to scan for self-declared dangerous capabilities.
SRC_GLOBS = ["*.rs", "*.py", "*.sh"]
SKIP_DIRS = {".git", "target", "node_modules", ".venv", "__pycache__"}

DANGER_RE = re.compile(r"OCAP-DANGER:\s*([a-z0-9-]+)")
GATE_RE = re.compile(r"OCAP-GATE:\s*([a-z0-9-]+)")

# `noninteractive-launch-policy` ratchet: authority is a value resolved ONCE at
# startup, not an ambient signal a later actor can flip. The three authority env
# twins may be READ (`env::var`) only by the single resolver
# (`LaunchAuthority::from_env` in `newt-core/src/launch_authority.rs`); every
# deep library decides authority via `launch_authority::current()`. A stray deep
# `env::var("NEWT_DISABLE_OCAP" | …)` re-opens the widen-mid-process hole.
AUTHORITY_ENV_READ_RE = re.compile(
    r'env::var(?:_os)?\s*\(\s*"(NEWT_DISABLE_OCAP|NEWT_FULL_ACCESS|NEWT_UNSAFE_HOST_EXEC)"'
)
# The deep-library crate the ban applies to (entrypoint crates may still read the
# twins at startup as the compatibility input, then freeze the resolved value).
AUTHORITY_LIB_ROOT = REPO / "newt-core" / "src"
AUTHORITY_RESOLVER = AUTHORITY_LIB_ROOT / "launch_authority.rs"


class Result:
    def __init__(self) -> None:
        self.errors: list[str] = []
        self.warnings: list[str] = []

    def err(self, m: str) -> None:
        self.errors.append(m)

    def warn(self, m: str) -> None:
        self.warnings.append(m)


def parse_register(text: str):
    """Return (table_ids, entries) where entries maps id -> {fields, status}."""
    # Index table rows look like:  | `id` | invariant | residual | disabled |
    table_ids = set(re.findall(r"^\|\s*`([a-z0-9-]+)`\s*\|", text, re.MULTILINE))

    entries: dict[str, dict] = {}
    # Full entries are `### <id>` sections; capture until the next ## / ### / >.
    for m in re.finditer(r"^###\s+([a-z0-9-]+)\s*$(.*?)(?=^\#{2,3}\s|\Z|^>\s)", text,
                         re.MULTILINE | re.DOTALL):
        dev_id, body = m.group(1), m.group(2)
        present = [f for f in REQUIRED_FIELDS if re.search(rf"\b{re.escape(f)}\b", body)]
        status = "UNKNOWN"
        sm = re.search(r"Status:\*\*?\s*([A-Za-z]+)", body)
        if sm:
            status = sm.group(1).upper()
        entries[dev_id] = {"present": present, "status": status, "body": body}
    return table_ids, entries


def check_register(res: Result):
    if not REGISTER.exists():
        res.err(f"deviation register not found at {REGISTER.relative_to(REPO)} — "
                "the honesty gate has no source of truth (merge the docs PR or restore it)")
        return set(), {}
    text = REGISTER.read_text(encoding="utf-8")
    table_ids, entries = parse_register(text)

    if not table_ids and not entries:
        res.err("deviation register parsed to ZERO deviations — refusing to claim a clean "
                "OCAP state with no registered invariants (parser or register is broken)")

    for dev_id, e in entries.items():
        missing = [f for f in REQUIRED_FIELDS if f not in e["present"]]
        if missing:
            res.err(f"deviation '{dev_id}' is incomplete — missing fields: {', '.join(missing)} "
                    "(a half-specified caveat is a silent one)")
        if e["status"] not in ("OPEN", "CLOSED", "GATED", "BOUNDED"):
            res.err(f"deviation '{dev_id}' has no clear OPEN/CLOSED/GATED/BOUNDED status")

    # Index rows without a full entry are stubs — surfaced, never silent.
    for dev_id in sorted(table_ids - set(entries)):
        res.warn(f"deviation '{dev_id}' is listed in the index table but has no full entry yet — "
                 "complete it BEFORE building any capability it gates")
    return table_ids | set(entries), entries


def iter_source_files():
    for path in REPO.rglob("*"):
        if not path.is_file():
            continue
        if any(part in SKIP_DIRS for part in path.relative_to(REPO).parts):
            continue
        if path.suffix in {".rs", ".py", ".sh"} and path.resolve() != Path(__file__).resolve():
            yield path


def check_code_guards(res: Result, known_ids: set, entries: dict):
    """The teeth: every OCAP-DANGER marker must be gated unless its deviation is CLOSED."""
    danger_sites = 0
    for path in iter_source_files():
        try:
            lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError:
            continue
        for i, line in enumerate(lines):
            dm = DANGER_RE.search(line)
            if not dm:
                continue
            danger_sites += 1
            dev_id = dm.group(1)
            rel = path.relative_to(REPO)
            if dev_id not in known_ids:
                res.err(f"{rel}:{i+1} declares OCAP-DANGER:{dev_id}, which is not a registered "
                        "deviation — register it before shipping the capability")
                continue
            status = entries.get(dev_id, {}).get("status", "OPEN")
            if status == "CLOSED":
                continue  # invariant enforced; the capability is freed
            window = "\n".join(lines[i:i + GATE_PROXIMITY_LINES + 1])
            if dev_id not in {g for g in GATE_RE.findall(window)}:
                res.err(f"{rel}:{i+1} exercises a capability gated by OPEN deviation "
                        f"'{dev_id}' but has no OCAP-GATE:{dev_id} within "
                        f"{GATE_PROXIMITY_LINES} lines — wire the fail-closed gate, or close "
                        "the deviation, before this can ship")
    return danger_sites


def check_launch_authority_reads(res: Result):
    """`noninteractive-launch-policy`: no deep-library ambient authority read.

    In `newt-core/src`, `env::var("NEWT_DISABLE_OCAP" | "NEWT_FULL_ACCESS" |
    "NEWT_UNSAFE_HOST_EXEC")` may appear ONLY in `launch_authority.rs` (the sole
    resolver). Any other deep read lets a later-appearing env var widen authority
    mid-process — the hole the frozen `LaunchAuthority` closes.
    """
    if not AUTHORITY_LIB_ROOT.is_dir():
        return
    resolver = AUTHORITY_RESOLVER.resolve()
    for path in AUTHORITY_LIB_ROOT.rglob("*.rs"):
        if path.resolve() == resolver:
            continue
        try:
            lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError:
            continue
        for i, line in enumerate(lines):
            m = AUTHORITY_ENV_READ_RE.search(line)
            if m:
                rel = path.relative_to(REPO)
                res.err(
                    f"{rel}:{i+1} reads authority env var {m.group(1)!r} directly — deep "
                    "libraries must decide authority via launch_authority::current() (the "
                    "frozen value); only launch_authority.rs may read the env twin "
                    "(noninteractive-launch-policy)"
                )


# Machine-checkable evidence fields that turn a Status label into a proven state.
GUARD_SYMBOLS_RE = re.compile(r"Unreachable-guard-symbols:\**\s*`?([A-Za-z0-9_,\s`]+)")
BOUNDED_BY_RE = re.compile(r"Bounded-by:\**\s*`?([a-z0-9,\-\s`]+)")


def _list_field(field_re, body):
    m = field_re.search(body)
    if not m:
        return []
    raw = m.group(1).replace("`", "").strip()
    # Stop at the first line break — the field is a single line.
    raw = raw.splitlines()[0] if raw else raw
    return [s.strip() for s in re.split(r"[,\s]+", raw) if s.strip()]


def _rust_texts():
    out = {}
    for path in REPO.rglob("*.rs"):
        rel = path.relative_to(REPO)
        if any(part in SKIP_DIRS for part in rel.parts):
            continue
        try:
            out[rel] = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            out[rel] = ""
    return out


def _is_test_file(rel: Path) -> bool:
    return ("tests" in rel.parts) or rel.name.endswith(("_test.rs", "_tests.rs"))


_LINE_COMMENT_RE = re.compile(r"//.*")
_BLOCK_COMMENT_RE = re.compile(r"/\*.*?\*/", re.DOTALL)
_TEST_ATTR_RE = re.compile(r"#\[\s*(?:cfg\(\s*test\s*\)|test)\s*\]")


def _strip_comments(text: str) -> str:
    return _LINE_COMMENT_RE.sub("", _BLOCK_COMMENT_RE.sub("", text))


def _strip_test_blocks(text: str) -> str:
    """Remove every `#[cfg(test)]` / `#[test]`-guarded item (brace- or
    semicolon-terminated) so a capability's OWN unit tests are not counted as
    callers. Approximate brace-balance (does not model braces inside strings),
    which is safe here: it can only remove MORE, never expose a hidden caller in
    the production portion."""
    out, last, n = [], 0, len(text)
    for m in _TEST_ATTR_RE.finditer(text):
        if m.start() < last:
            continue
        out.append(text[last:m.start()])
        j, depth, started = m.end(), 0, False
        while j < n:
            c = text[j]
            if c == "{":
                depth += 1
                started = True
            elif c == "}":
                depth -= 1
                if started and depth == 0:
                    j += 1
                    break
            elif c == ";" and not started:
                j += 1
                break
            j += 1
        last = j
    out.append(text[last:])
    return "".join(out)


def _capability_referenced_in(sym: str, texts: dict):
    """Files where `sym` is REFERENCED in production code by ANY means — a direct
    call, an alias import (`use … as`), a function item/pointer, a closure
    capture, a callback registration, a UFCS/method path, a macro argument — i.e.
    ANY word-occurrence that is not the `fn <sym>` definition line, a comment, or
    test code. This closes the holes where a `<sym>(` -only regex, or excluding
    the defining file, would let a contributor wire a GATED capability while
    ocap-check stayed green.
    """
    ref_re = re.compile(rf"\b{re.escape(sym)}\b")
    def_line_re = re.compile(rf"\bfn\s+{re.escape(sym)}\s*[<(]")
    hits = []
    for rel, t in texts.items():
        if _is_test_file(rel):
            continue
        prod = _strip_test_blocks(_strip_comments(t))
        for line in prod.splitlines():
            if ref_re.search(line) and not def_line_re.search(line):
                hits.append(str(rel))
                break
    return sorted(hits)


def check_state_proofs(res: Result, entries: dict, texts=None):
    """The teeth for the non-ACTIVE states: a `Status:` label is NOT evidence.

    GATED must name the Rust capability symbols it gates (`Unreachable-guard-
    symbols:`) and ocap-check proves they have NO caller outside their defining
    file + tests — so a future contributor who wires the capability makes CI fail.
    BOUNDED must name the CLOSED deviations that bound the reachable residual's
    authority (`Bounded-by:`) — a bound that is not itself enforced bounds nothing.
    A cosmetic OPEN->GATED/BOUNDED relabel therefore cannot pass.

    `texts` (rel-path -> source) is injectable so the self-test can drive the
    unreachability check over a fixture tree instead of the real repo.
    """
    if texts is None:
        texts = _rust_texts()
    for dev_id, e in entries.items():
        status, body = e["status"], e["body"]
        if status == "GATED":
            syms = _list_field(GUARD_SYMBOLS_RE, body)
            if not syms:
                res.err(
                    f"GATED '{dev_id}' declares no `Unreachable-guard-symbols:` — GATED needs a "
                    "MACHINE-CHECKABLE unreachability proof, not a label. Name the Rust capability "
                    "functions it gates (they must have no caller)."
                )
                continue
            for sym in syms:
                def_files = {
                    rel for rel, t in texts.items()
                    if re.search(rf"\bfn\s+{re.escape(sym)}\s*[<(]", t)
                }
                if not def_files:
                    res.err(
                        f"GATED '{dev_id}': guard symbol `{sym}` has no `fn {sym}` in the tree — "
                        "cannot prove unreachability of a symbol that does not exist."
                    )
                    continue
                refs = _capability_referenced_in(sym, texts)
                if refs:
                    res.err(
                        f"GATED '{dev_id}': guard symbol `{sym}` is REFERENCED (reachable) in "
                        f"{', '.join(refs)} — a GATED capability must be UNREACHABLE (no call, "
                        "alias, function-item, pointer, callback, or macro wiring outside its "
                        "definition + tests). Make the deviation ACTIVE and gate the caller."
                    )
        elif status == "BOUNDED":
            bys = _list_field(BOUNDED_BY_RE, body)
            if not bys:
                res.err(
                    f"BOUNDED '{dev_id}' declares no `Bounded-by:` — a reachable residual counts as "
                    "BOUNDED only when its authority cannot exceed other INDEPENDENTLY-ENFORCED "
                    "(CLOSED) invariants. Name them."
                )
                continue
            for b in bys:
                bstat = entries.get(b, {}).get("status")
                if bstat != "CLOSED":
                    res.err(
                        f"BOUNDED '{dev_id}': bound `{b}` is not CLOSED (status={bstat or 'MISSING'}) "
                        "— a bound that is not itself enforced bounds nothing."
                    )


def print_ledger(known_ids: set, entries: dict, danger_sites: int):
    def status_of(i):
        return entries.get(i, {}).get("status", "OPEN")

    # ACTIVE = OPEN: a *reachable* capability whose invariant is not yet enforced,
    # held fail-closed. GATED: the capability is UNREACHABLE (no caller wires it)
    # AND fail-closed AND its forward obligation is machine-enforced (the
    # OCAP-DANGER/GATE ratchet still requires the gate) — a deliberately-deferred
    # build, not an active mediation gap. "zero ACTIVE" = zero OPEN.
    active = sorted(i for i in known_ids if status_of(i) == "OPEN")
    gated = sorted(i for i in known_ids if status_of(i) == "GATED")
    bounded = sorted(i for i in known_ids if status_of(i) == "BOUNDED")
    closed = sorted(i for i in known_ids if status_of(i) == "CLOSED")
    print("── OCAP deviation ledger " + "─" * 40)
    print(f"  registered deviations : {len(known_ids)}")
    print(f"  ACTIVE / OPEN (reachable capability, invariant NOT enforced NOR bounded): {len(active)}  ->  {', '.join(active) or '(none)'}")
    print(f"  GATED (capability mechanically UNREACHABLE + fail-closed + ratcheted): {len(gated)}  ->  {', '.join(gated) or '(none)'}")
    print(f"  BOUNDED (reachable, authority cannot exceed CLOSED invariants): {len(bounded)}  ->  {', '.join(bounded) or '(none)'}")
    print(f"  CLOSED (invariant enforced, capability freed): {len(closed)}  ->  {', '.join(closed) or '(none)'}")
    print(f"  OCAP-DANGER sites in tree     : {danger_sites}")
    if not active:
        print("  ✓ ZERO ACTIVE deviations — every reachable capability is enforced (CLOSED), "
              "bounded by CLOSED invariants (BOUNDED), or mechanically unreachable (GATED)")
    print("─" * 64)


def _fixture_entries(md: str) -> dict:
    return parse_register(md)[1]


def run_self_tests() -> int:
    """Adversarial mutation tests proving the state machinery is GROUNDED, not a
    label (review concern 1). Wired into CI beside the real check.

    - a cosmetic OPEN->GATED relabel (no guard-symbols) is rejected;
    - a GATED capability that has ANY caller (made reachable) is rejected;
    - a GATED capability with no caller is accepted;
    - a BOUNDED bound that is not itself CLOSED is rejected.
    """
    failures = []

    def expect(name, cond):
        if not cond:
            failures.append(name)

    # 1. Cosmetic relabel OPEN->GATED with no proof MUST error.
    md = "### x\n- **Status:** GATED — relabelled with no proof\n"
    r = Result()
    check_state_proofs(r, _fixture_entries(md), texts={})
    expect(
        "cosmetic-gated-relabel-rejected",
        any("Unreachable-guard-symbols" in e for e in r.errors),
    )

    # 2. GATED unreachability must be robust to EVERY wiring form (review
    #    concern 11) — a `<sym>(`-only regex, or excluding the defining file,
    #    would let these through. Each MUST be rejected.
    md = "### x\n- **Unreachable-guard-symbols:** foo\n- **Status:** GATED\n"
    entries_x = _fixture_entries(md)
    def_only = "pub fn foo() { let _ = 1; }"
    holes = {
        "direct-call": {Path("b/c.rs"): "fn go() { foo(); }"},
        # a caller in the symbol's OWN defining file (was excluded before).
        "same-file-caller": {Path("a/def.rs"): def_only + "\nfn go() { foo(); }"},
        "alias-import": {Path("b/c.rs"): "use crate::a::foo as bar;\nfn go(){ bar(); }"},
        "function-item": {Path("b/c.rs"): "fn go() { let f = foo; f(); }"},
        "function-pointer": {Path("b/c.rs"): "fn go(x:&[i32]){ x.iter().for_each(foo); }"},
        "closure-capture": {Path("b/c.rs"): "fn go() { let c = || foo(); c(); }"},
        "callback-register": {Path("b/c.rs"): "fn go(r:&mut R){ r.on(foo); }"},
        "macro-arg": {Path("b/c.rs"): "fn go() { register!(foo); }"},
    }
    for name, extra in holes.items():
        texts = {Path("a/def.rs"): def_only}
        texts.update(extra)
        r = Result()
        check_state_proofs(r, entries_x, texts=texts)
        expect(f"gated-{name}-rejected", any("is REFERENCED (reachable)" in e for e in r.errors))

    # 3. GATED with references only in a comment + an inline `#[cfg(test)]` mod +
    #    a test file MUST pass (those are not production wiring).
    texts = {
        Path("a/def.rs"): (
            "// foo is fail-closed; see foo() in tests\n"
            "pub fn foo() {}\n"
            "#[cfg(test)]\nmod tests { fn t() { foo(); } }\n"
        ),
        Path("a/tests/t.rs"): "fn t() { foo(); }",
    }
    r = Result()
    check_state_proofs(r, entries_x, texts=texts)
    expect("gated-unreachable-accepted", not r.errors)

    # 4. BOUNDED whose bound is not CLOSED MUST error.
    md = "### x\n- **Bounded-by:** y\n- **Status:** BOUNDED\n\n### y\n- **Status:** OPEN\n"
    r = Result()
    check_state_proofs(r, _fixture_entries(md), texts={})
    expect("bounded-by-open-rejected", any("is not CLOSED" in e for e in r.errors))

    # 5. BOUNDED whose bound IS CLOSED MUST pass.
    md = "### x\n- **Bounded-by:** y\n- **Status:** BOUNDED\n\n### y\n- **Status:** CLOSED\n"
    r = Result()
    check_state_proofs(r, _fixture_entries(md), texts={})
    expect("bounded-by-closed-accepted", not r.errors)

    if failures:
        for f in failures:
            print(f"  SELF-TEST FAIL: {f}", file=sys.stderr)
        print(
            f"\nocap-check --self-test FAILED ({len(failures)}) — the GATED/BOUNDED machinery is "
            "not grounded; a cosmetic relabel could pass. Fix check_state_proofs.",
            file=sys.stderr,
        )
        return 1
    print("ocap-check --self-test OK — cosmetic relabels + reachable-capability mutations rejected.")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return run_self_tests()
    res = Result()
    known_ids, entries = check_register(res)
    danger_sites = check_code_guards(res, known_ids, entries)
    check_state_proofs(res, entries)
    check_launch_authority_reads(res)
    print_ledger(known_ids, entries, danger_sites)

    for w in res.warnings:
        print(f"  warn: {w}")
    for e in res.errors:
        print(f"  FAIL: {e}", file=sys.stderr)

    if res.errors:
        print(f"\nocap-check FAILED ({len(res.errors)} violation(s)) — the deviation ratchet is "
              "the honesty gate; do not bypass it.", file=sys.stderr)
        return 1
    print("\nocap-check OK — register honest, every dangerous capability is gated or its "
          "deviation is closed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
