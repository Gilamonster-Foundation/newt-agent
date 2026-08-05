#!/usr/bin/env python3
"""Exact, fail-closed reference linter for spec/behavior-map.toml (epic #1529).

A behavioral change must not silently orphan a constitution entry. This validates
that every reference resolves to its INTENDED artifact — not merely that a like-
named string exists somewhere:

  * lean       {module, symbol}  -> a decl of that name inside that namespace,
                                     anywhere in formal/**.lean — every lake lib of
                                     the Lean layer (`.lake` build copies excluded so
                                     they cannot fabricate ambiguity) — exactly once.
  * rust_tests {path, symbol}    -> a `fn <leaf>` at the in-file module path
                                     `<mod...>::<leaf>` inside <path>, exactly once,
                                     AND carrying a recognized test attribute
                                     (`#[test]`, `#[tokio::test]`, …). A plain `fn`
                                     Cargo would not execute cannot satisfy it, so
                                     deleting `#[test]` flips the ref to unresolved.
  * production {path, symbol}    -> exactly one definition of <symbol> in <path>.
  * tla        {spec, invariant} -> spec/tla/<spec>.tla DEFINES <invariant> as an
                                     operator AND spec/tla/<spec>.cfg declares it in
                                     an INVARIANT(S) line (not merely a file that
                                     exists) — required before any tla="checked".

Fail-closed. Anything that does not resolve is an ERROR, EXCEPT a reference that
carries explicit `pending_pr = <n>` (its artifact lives on an unmerged PR): those
WARN, naming the contract and the PR. `--strict` makes even pending refs fail (run
it on `main` after the dependency merges; then delete the markers).

Searches are scoped to the NAMED file (rust/production) or the formal/ Lean layer
(lean) — never the registry, the docs, or this linter — so a reference can never
satisfy itself by matching a string in prose.

Also enforced: valid status vocabulary; status<->refs consistency (rust=tested
needs rust refs; lean in {spec,proven} needs resolving lean refs; tla=checked
needs a checked spec ref; conformance=full needs every prerequisite layer);
required fields; duplicate section headers; duplicate refs; zero and ambiguous
(multi-match) resolutions.

Usage:  python3 spec/lint-behavior-map.py [--strict] [--map P] [--repo D] [--lean-dir D]
Exit 0 = clean (per mode); 1 = at least one hard error.
"""
from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path

STATUS_VOCAB = {
    "lean": {"none", "spec", "proven"},
    "rust": {"none", "tested"},
    "tla": {"none", "planned", "checked"},
    "trace": {"none", "planned", "validated"},
    "conformance": {"partial", "full"},
}
RUST_DEF_KEYWORDS = ("fn", "const", "static", "struct", "enum", "type", "trait", "union")


# ─────────────────────────── source parsing ────────────────────────────────

def _strip_rust(text: str) -> list[tuple[str, int]]:
    """[(char, brace_depth_after)] with comments/strings blanked out. A small
    lexer — good enough for well-formed Rust (fixtures + the real files)."""
    out: list[tuple[str, int]] = []
    i, n, depth = 0, len(text), 0
    state = "code"
    raw_hashes = 0
    block_depth = 0
    while i < n:
        c = text[i]
        nxt = text[i + 1] if i + 1 < n else ""
        if state == "code":
            if c == "/" and nxt == "/":
                state = "line"; i += 2; continue
            if c == "/" and nxt == "*":
                state = "block"; block_depth = 1; i += 2; continue
            if c == '"':
                state = "str"; i += 1; continue
            if c == "r" and nxt in ('"', "#"):
                j = i + 1; raw_hashes = 0
                while j < n and text[j] == "#":
                    raw_hashes += 1; j += 1
                if j < n and text[j] == '"':
                    state = "raw"; i = j + 1; continue
            if c == "'":
                # A char literal ('x', '\n') vs a lifetime/label ('a, 'static, '_).
                # Only the former opens a quoted span; a lifetime is ordinary code —
                # mis-treating it as a char literal eats to the next `'`, blanking
                # out `fn`/`mod`/`{` and desyncing brace depth.
                if nxt == "\\" or (i + 2 < n and text[i + 2] == "'"):
                    state = "chr"; i += 1; continue
                out.append((c, depth)); i += 1; continue
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
            out.append((c, depth)); i += 1; continue
        if state == "line":
            if c == "\n":
                state = "code"; out.append(("\n", depth))
            i += 1; continue
        if state == "block":
            # Rust block comments NEST: /* /* */ */ — track depth, exit only at 0,
            # else code after the first inner `*/` is lexed as live (phantom defs).
            if c == "/" and nxt == "*":
                block_depth += 1; i += 2; continue
            if c == "*" and nxt == "/":
                block_depth -= 1; i += 2
                if block_depth == 0:
                    state = "code"
                continue
            i += 1; continue
        if state == "str":
            if c == "\\":
                i += 2; continue
            if c == '"':
                state = "code"
            i += 1; continue
        if state == "chr":
            if c == "\\":
                i += 2; continue
            if c == "'":
                state = "code"
            i += 1; continue
        if state == "raw":
            if c == '"':
                j = i + 1; k = 0
                while k < raw_hashes and j < n and text[j] == "#":
                    k += 1; j += 1
                if k == raw_hashes:
                    state = "code"; i = j; continue
            i += 1; continue
    return out


# Attribute clusters that mark a function as a Cargo-executed test. Matched on the
# LAST `::` segment so `test`, `tokio::test`, `async_std::test`, `actix_rt::test`
# all count; plus a small allowlist of well-known test-macro names.
_TEST_ATTR_NAMES = {"rstest", "test_case", "googletest", "gtest"}
_MOD_TAIL = re.compile(
    r"(?:\bpub\b(?:\s*\([^()]*\))?|\basync\b|\bunsafe\b|\bconst\b|\bextern\b"
    r"|\bdefault\b|\bmove\b)\s*$"
)


def _is_test_attr(body: str) -> bool:
    """True if an attribute body (between `#[` and `]`) is a recognized test marker."""
    head = re.split(r"[(\s]", body.strip(), maxsplit=1)[0]
    head = head.replace(" ", "").replace("\t", "")
    if not head:
        return False
    last = head.split("::")[-1]
    return last == "test" or head in _TEST_ATTR_NAMES or last in _TEST_ATTR_NAMES


def _attrs_before(s: str, p: int) -> list[str]:
    """Attribute bodies attached to the item whose keyword starts at index `p`,
    walking back over intervening whitespace and modifier keywords (pub/async/…)."""
    attrs: list[str] = []
    i = p
    while True:
        j = i
        while j > 0 and s[j - 1].isspace():
            j -= 1
        mt = _MOD_TAIL.search(s[:j])  # a modifier keyword sits between attrs and fn
        if mt:
            i = mt.start()
            continue
        if j > 0 and s[j - 1] == "]":  # an attribute `#[ … ]` ends right here
            depth, k = 0, j - 1
            while k >= 0:
                if s[k] == "]":
                    depth += 1
                elif s[k] == "[":
                    depth -= 1
                    if depth == 0:
                        break
                k -= 1
            if k < 0:
                break
            b = k - 1
            if b >= 0 and s[b] == "!":  # inner attribute `#![ … ]`
                b -= 1
            if b >= 0 and s[b] == "#":
                attrs.append(s[k + 1:j - 1])
                i = b
                continue
        break
    return attrs


def _fn_is_test(s: str, kw_start: int) -> bool:
    return any(_is_test_attr(a) for a in _attrs_before(s, kw_start))


def rust_test_defs(text: str) -> list[tuple[tuple[str, ...], str]]:
    """[(in-file module path, fn name)] for TEST-ATTRIBUTED fns only, tracking
    `mod X { … }` nesting by depth. A `fn` without a recognized test attribute is
    NOT returned — so a rust_tests ref cannot resolve to something Cargo won't run."""
    lex = _strip_rust(text)
    s = "".join(c for c, _ in lex)
    depths = [d for _, d in lex]
    defs: list[tuple[tuple[str, ...], str]] = []
    modstack: list[tuple[str, int]] = []  # (name, body_depth)
    for m in re.finditer(
        r"\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{|\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\b|\}", s
    ):
        depth_here = depths[m.start()]
        while modstack and depth_here < modstack[-1][1]:
            modstack.pop()
        if m.group(0) == "}":
            continue
        if m.group(1):  # `mod NAME {`
            body_depth = depths[m.end() - 1]  # depth just after the `{`
            modstack.append((m.group(1), body_depth))
        elif m.group(2) and _fn_is_test(s, m.start()):  # `fn NAME` with a test attr
            # A #[test] fn is Cargo-runnable only at MODULE scope (rustc's
            # `unnameable_test_items`): a #[test] nested inside another fn's body
            # never runs, so it must not satisfy a rust_tests ref.
            scope_depth = modstack[-1][1] if modstack else 0
            if depth_here == scope_depth:
                defs.append((tuple(nm for nm, _ in modstack), m.group(2)))
    return defs


def rust_definition_count(text: str, symbol: str) -> int:
    """How many definitions of `symbol` (any def keyword) exist in the file."""
    s = "".join(c for c, _ in _strip_rust(text))
    kw = "|".join(RUST_DEF_KEYWORDS)
    return len(re.findall(rf"\b(?:{kw})\s+{re.escape(symbol)}\b", s))


def lean_decls(text: str) -> list[str]:
    """Fully-qualified declaration names, tracking `namespace`/`end` + comments."""
    fqs: list[str] = []
    stack: list[str] = []
    in_block = False
    for raw in text.splitlines():
        line = raw
        if in_block:
            if "-/" in line:
                line = line.split("-/", 1)[1]; in_block = False
            else:
                continue
        while "/-" in line:
            before, _, rest = line.partition("/-")
            if "-/" in rest:
                line = before + rest.split("-/", 1)[1]
            else:
                line = before; in_block = True; break
        line = re.split(r"--", line, maxsplit=1)[0]
        m = re.match(r"\s*namespace\s+([A-Za-z0-9_.']+)", line)
        if m:
            stack.append(m.group(1)); continue
        m = re.match(r"\s*end\s+([A-Za-z0-9_.']+)\s*$", line)
        if m:
            if stack and stack[-1] == m.group(1):
                stack.pop()
            continue
        m = re.match(
            r"\s*(?:@\[[^\]]*\]\s*)?(?:private\s+|protected\s+|noncomputable\s+)*"
            r"(?:theorem|lemma|def|abbrev|structure|inductive|class|instance)\s+([A-Za-z0-9_']+)",
            line,
        )
        if m:
            fqs.append(".".join(stack + [m.group(1)]))
    return fqs


# TLC .cfg section keywords — an INVARIANT(S) list runs until the next one.
_CFG_KEYWORDS = {
    "SPECIFICATION", "INIT", "NEXT", "CONSTANT", "CONSTANTS", "INVARIANT",
    "INVARIANTS", "PROPERTY", "PROPERTIES", "SYMMETRY", "VIEW", "CONSTRAINT",
    "ACTION_CONSTRAINT", "ALIAS", "POSTCONDITION", "CHECK_DEADLOCK",
}


def strip_tla_comments(text: str) -> str:
    """Blank TLA+/`.cfg` comments: NESTING `(* … *)` blocks + `\\*` line comments.
    TLC's parser nests block comments, so a non-nesting regex leaves a commented-out
    operator/INVARIANT visible — a false green where the linter certifies something
    TLC never checks. Newlines are preserved so `(?m)^` anchors still hold."""
    out: list[str] = []
    i, n, block = 0, len(text), 0
    while i < n:
        two = text[i:i + 2]
        if block == 0 and two == "\\*":            # line comment → end of line
            j = text.find("\n", i)
            if j == -1:
                break
            out.append("\n"); i = j + 1; continue
        if two == "(*":
            block += 1; i += 2; continue
        if two == "*)" and block > 0:
            block -= 1; i += 2; continue
        if block == 0:
            out.append(text[i])
        elif text[i] == "\n":
            out.append("\n")
        i += 1
    return "".join(out)


def tla_operator_defined(tla_text: str, name: str) -> bool:
    """True if <name> is defined as an operator (`name == …` or `name(args) == …`)."""
    text = strip_tla_comments(tla_text)
    return re.search(rf"(?m)^\s*{re.escape(name)}\s*(?:\([^()]*\))?\s*==", text) is not None


def cfg_declared_invariants(cfg_text: str) -> set[str]:
    """Operator names TLC is told to check as invariants (INVARIANT/INVARIANTS)."""
    text = strip_tla_comments(cfg_text)
    out: set[str] = set()
    collecting = False
    for tok in re.findall(r"[A-Za-z_][A-Za-z0-9_]*", text):
        if tok in ("INVARIANT", "INVARIANTS"):
            collecting = True
        elif tok in _CFG_KEYWORDS:
            collecting = False
        elif collecting:
            out.add(tok)
    return out


# ─────────────────────────── the linter ────────────────────────────────────

class Linter:
    def __init__(self, repo: Path, lean_dir: Path, strict: bool):
        self.repo = repo
        self.lean_dir = lean_dir
        self.strict = strict
        self.errors: list[str] = []
        self.warnings: list[str] = []
        self._lean_cache: list[str] | None = None

    def err(self, bhv: str, msg: str) -> None:
        self.errors.append(f"{bhv}: {msg}")

    def warn(self, bhv: str, msg: str) -> None:
        self.warnings.append(f"{bhv}: {msg}")

    def lean_declset(self) -> list[str]:
        if self._lean_cache is None:
            decls: list[str] = []
            if self.lean_dir.exists():
                for f in sorted(self.lean_dir.rglob("*.lean")):
                    # `.lake` holds build copies of the same source .lean files;
                    # scanning them would double-count decls and turn every ref
                    # AMBIGUOUS. Skip them — decls are namespace-qualified, so
                    # scanning the rest of formal/ adds only distinct libs.
                    if ".lake" in f.parts:
                        continue
                    decls += lean_decls(f.read_text(encoding="utf-8", errors="replace"))
            self._lean_cache = decls
        return self._lean_cache

    def resolve_lean(self, ref: dict) -> int:
        return self.lean_declset().count(f"{ref['module']}.{ref['symbol']}")

    def resolve_rust_test(self, ref: dict) -> int | None:
        p = self.repo / ref["path"]
        if not p.exists():
            return None
        parts = ref["symbol"].split("::")
        parents, leaf = tuple(parts[:-1]), parts[-1]
        defs = rust_test_defs(p.read_text(encoding="utf-8", errors="replace"))
        return sum(1 for mp, fn in defs if fn == leaf and mp == parents)

    def resolve_production(self, ref: dict) -> int | None:
        p = self.repo / ref["path"]
        if not p.exists():
            return None
        return rust_definition_count(p.read_text(encoding="utf-8", errors="replace"), ref["symbol"])

    def check_resolvable(self, bhv: str, kind: str, ref: dict, count: int | None) -> None:
        pending = ref.get("pending_pr")
        label = f"{kind} {ref.get('module', ref.get('path'))}::{ref['symbol']}"
        if count is None or count == 0:
            if pending and not self.strict:
                self.warn(bhv, f"{label} unresolved (pending PR #{pending})")
            else:
                self.err(bhv, f"{label} does not resolve"
                              + (f" (pending PR #{pending}, and --strict)" if pending else ""))
        elif count > 1:
            self.err(bhv, f"{label} is AMBIGUOUS ({count} matches)")

    def check_tla_ref(self, bhv: str, ref: dict) -> None:
        """A tla ref must name a spec + an invariant that is BOTH defined as an
        operator in <spec>.tla AND declared in an INVARIANT(S) line of <spec>.cfg —
        a file that merely exists is not proof TLC checks anything."""
        spec, inv = ref.get("spec"), ref.get("invariant")
        if not spec:
            self.err(bhv, f"tla ref needs `spec`: {ref}"); return
        if not inv:
            self.err(bhv, f"tla ref {spec!r} needs `invariant` (the operator TLC checks)"); return
        tla = self.repo / "spec" / "tla" / f"{spec}.tla"
        cfg = self.repo / "spec" / "tla" / f"{spec}.cfg"
        if not tla.exists():
            self.err(bhv, f"tla ref spec {spec!r} has no spec/tla/{spec}.tla"); return
        if not cfg.exists():
            self.err(bhv, f"tla ref spec {spec!r} has no spec/tla/{spec}.cfg"); return
        if not tla_operator_defined(tla.read_text(encoding="utf-8", errors="replace"), inv):
            self.err(bhv, f"tla ref {spec}!{inv} is not defined as an operator in {spec}.tla")
        elif inv not in cfg_declared_invariants(cfg.read_text(encoding="utf-8", errors="replace")):
            self.err(bhv, f"tla ref {spec}!{inv} is not declared as an INVARIANT in {spec}.cfg")

    def check_contract(self, bhv: str, body: dict) -> None:
        if not isinstance(body, dict):
            self.err(bhv, "entry is not a table")
            return
        desc = body.get("description")
        if not isinstance(desc, str) or not desc.strip():
            self.err(bhv, "missing non-empty `description`")
        status = body.get("status")
        if not isinstance(status, dict):
            self.err(bhv, "missing `status` table")
            return
        for key, vocab in STATUS_VOCAB.items():
            if key not in status:
                self.err(bhv, f"status.{key} is required")
            elif status[key] not in vocab:
                self.err(bhv, f"status.{key} = {status[key]!r} is invalid (allowed: {sorted(vocab)})")

        refs = body.get("refs", {})
        if not isinstance(refs, dict):
            self.err(bhv, "`refs` must be a table")
            refs = {}
        lean_refs = refs.get("lean", [])
        rust_refs = refs.get("rust_tests", [])
        prod_refs = refs.get("production", [])
        tla_refs = refs.get("tla", [])

        def dups(items, keyfn):
            seen, out = set(), set()
            for it in items:
                k = keyfn(it)
                (out if k in seen else seen).add(k)
            return out
        for kind, items, keyfn in (
            ("lean", lean_refs, lambda r: (r.get("module"), r.get("symbol"))),
            ("rust_tests", rust_refs, lambda r: (r.get("path"), r.get("symbol"))),
            ("production", prod_refs, lambda r: (r.get("path"), r.get("symbol"))),
        ):
            for d in dups(items, keyfn):
                self.err(bhv, f"duplicate {kind} reference {d}")

        if status.get("lean") in {"spec", "proven"} and not lean_refs:
            self.err(bhv, f"status.lean = {status.get('lean')!r} but no `refs.lean`")
        if status.get("rust") == "tested" and not rust_refs:
            self.err(bhv, "status.rust = 'tested' but no `refs.rust_tests`")
        if status.get("tla") == "checked" and not tla_refs:
            self.err(bhv, "status.tla = 'checked' but no `refs.tla`")
        if status.get("conformance") == "full" and not (
            status.get("lean") == "proven" and status.get("rust") == "tested"
            and status.get("tla") == "checked" and status.get("trace") == "validated"
        ):
            self.err(bhv, "conformance = 'full' requires lean=proven, rust=tested, "
                          "tla=checked, trace=validated (oracle/model/trace prerequisites)")

        for r in lean_refs:
            if not (r.get("module") and r.get("symbol")):
                self.err(bhv, f"lean ref needs `module` and `symbol`: {r}"); continue
            self.check_resolvable(bhv, "lean", r, self.resolve_lean(r))
        for r in rust_refs:
            if not (r.get("path") and r.get("symbol")):
                self.err(bhv, f"rust_tests ref needs `path` and `symbol`: {r}"); continue
            self.check_resolvable(bhv, "rust_tests", r, self.resolve_rust_test(r))
        for r in prod_refs:
            if not (r.get("path") and r.get("symbol")):
                self.err(bhv, f"production ref needs `path` and `symbol`: {r}"); continue
            self.check_resolvable(bhv, "production", r, self.resolve_production(r))
        for r in tla_refs:
            self.check_tla_ref(bhv, r)

    def run(self, map_path: Path) -> int:
        raw = map_path.read_text(encoding="utf-8")
        headers = re.findall(r"(?m)^\[(BHV-[A-Za-z0-9_-]+)\]\s*$", raw)
        for h in {x for x in headers if headers.count(x) > 1}:
            self.errors.append(f"{h}: duplicate section header")
        try:
            data = tomllib.loads(raw)
        except tomllib.TOMLDecodeError as e:
            print(f"[ERROR] behavior-map.toml does not parse: {e}", file=sys.stderr)
            return 1
        bhvs = [k for k in data if k.startswith("BHV-")]
        if not bhvs:
            print("[ERROR] no BHV-* contracts found", file=sys.stderr)
            return 1
        for bhv in bhvs:
            self.check_contract(bhv, data[bhv])

        if self.warnings:
            print(f"[warn ] {len(self.warnings)} pending-PR advisory reference(s):", file=sys.stderr)
            for w in self.warnings:
                print(f"        {w}", file=sys.stderr)
        if self.errors:
            print(f"[ERROR] {len(self.errors)} unresolved/invalid reference(s):", file=sys.stderr)
            for e in self.errors:
                print(f"        {e}", file=sys.stderr)
            return 1
        print(f"behavior-map.toml: {len(bhvs)} contracts OK — "
              f"{len(self.warnings)} pending-PR advisory, 0 errors.")
        return 0


def main() -> int:
    here = Path(__file__).resolve().parent
    ap = argparse.ArgumentParser()
    ap.add_argument("--map", default=str(here / "behavior-map.toml"))
    ap.add_argument("--repo", default=str(here.parent))
    ap.add_argument("--lean-dir", default=None)
    ap.add_argument("--strict", action="store_true")
    a = ap.parse_args()
    repo = Path(a.repo).resolve()
    lean_dir = Path(a.lean_dir).resolve() if a.lean_dir else repo / "formal"
    return Linter(repo, lean_dir, a.strict).run(Path(a.map).resolve())


if __name__ == "__main__":
    raise SystemExit(main())
