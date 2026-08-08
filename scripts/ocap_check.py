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
        if e["status"] not in ("OPEN", "CLOSED"):
            res.err(f"deviation '{dev_id}' has no clear OPEN/CLOSED status")

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


def print_ledger(known_ids: set, entries: dict, danger_sites: int):
    open_ids = sorted(i for i in known_ids if entries.get(i, {}).get("status", "OPEN") != "CLOSED")
    closed = sorted(i for i in known_ids if entries.get(i, {}).get("status") == "CLOSED")
    print("── OCAP deviation ledger " + "─" * 40)
    print(f"  registered deviations : {len(known_ids)}")
    print(f"  OPEN (authority caveated down): {len(open_ids)}  ->  {', '.join(open_ids) or '(none)'}")
    print(f"  CLOSED (capabilities freed)   : {len(closed)}  ->  {', '.join(closed) or '(none)'}")
    print(f"  OCAP-DANGER sites in tree     : {danger_sites}")
    print("─" * 64)


def main() -> int:
    res = Result()
    known_ids, entries = check_register(res)
    danger_sites = check_code_guards(res, known_ids, entries)
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
