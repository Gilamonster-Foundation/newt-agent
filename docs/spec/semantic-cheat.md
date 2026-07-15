# The Semantic Cheat

**Status:** DRAFT 0.2.0 (2026-07-15) — synthesis of the #1216 research brief,
three law-draft lenses (honesty, budget, seams), and four code-level seam
maps; revised per review round 1 (three blockers + majors applied; ledger in
§7). Normative once accepted.
**Scope:** the contract between newt-core's navigation-knowledge *mechanism*
(index, render, lookup, lens, dispatch payload) and the *data* that steers it
(language packs, workspace maps, budget pins, nudger knobs). It governs how
repo knowledge crosses the model/tool line so that small models navigate like
big ones — the observed failure being Ornith-1.0-35B guessing
`newt-core/src/commands/dgx.rs` from training priors on the 2026-07-15 #548
bench while a 3,000-char map starved beside a 210k budget.
**Audience:** implementers of the index core (newt-core), consumers that place
it (newt-tui, crew dispatch, headless drivers; future gila/hermes), and data
authors (workspace-map cards, model cards, family profiles).
**Companion docs:** the #1216 design brief (prior art, ladder rationale — this
document deliberately restates none of it); agent-bridle
`docs/spec/ceremony-contract.md` (house shape, governance discipline, and the
upstream authority laws this spec cites rather than restates).

---

## 1. Terms

| Term | Definition | Grounded in |
|---|---|---|
| **Index** | the one extraction per session: `(path → [(symbol, kind)], entry_points)`, computed from the working tree by pure-data rules | `ApiSurfaceProvider::initialize` (newt-core/src/api_surface.rs:350-375); today discarded after render (api_surface.rs:244) — retention is this spec's first mechanism |
| **Snapshot** | the name of the candidate file walk: **manifest CID + session generation counter**. The manifest names the **full candidate walk** — every file matching pack extensions is hashed in, *including* files subsequently cut by walk/byte caps (extraction may be skipped; naming never is) — so the CutSet is a decidable list of `(path, class)` pairs. Wall-clock is never a coordination primitive; RFC 3339 timestamps are provenance data only (house rule; ceremony §1) | `content-addressable` `ContentId`; generation counters per house rule |
| **Fact / Witness** | a rendered claim `(path, symbol, kind)` together with the datum that produced it (matching rule, snapshot). `facts_of(fs, packs)` is the fact set over a file set; **`facts_of(gathered)`** — the extracted subset — is all the index can witness. On a cut-free snapshot (`CutSet = ∅`) the gathered facts and the snapshot facts coincide | `public_symbols` (api_surface.rs:275-289) |
| **Discovered budget (`w`)** | THE input to `resolve_budget`: the **resolved session send budget in tokens** as computed at provider construction — the mem_budget site (the double-80% path: a 262,144-token window resolves to `w = 167,772`). NOT the raw window; NOT safe_context. When discovery fails, `resolve_memory_budget` falls back to `DEFAULT_CONTEXT_TOKENS = 8,192` (memory.rs:520) — where the floor clamp dominates by construction (SC-L2) | mod.rs:327-341 + lib.rs:5019-5020; resolve_memory_budget (memory.rs:520) |
| **Tier-1 / crate map** | one pointer-card line per crate (name, purpose, key paths) — pure DATA (§3.1), hand-written first (D2), never LLM-derived | `.newt/workspace-map.toml`; pointer-card format borrowed from the knowledge repo (format only — no data, no dependency) |
| **Tier-2 / skeleton** | the symbol lines of the rendered surface — verbatim signatures, never summaries (RepoGraph ablation) | `render()` (api_surface.rs:301-341) |
| **Budget (`b`)** | tier-2 character allowance: `b = clamp(floor_chars, ⌊pct/100 × w⌋ × chars_per_token, ceiling_chars)` — `w` in tokens, `b` in chars, conversion pinned to the **static** `[context.estimation] chars_per_token` value (§8), never the live calibrated ratio | replaces `max_block_chars = 3_000` (config.rs:1211-1213); chars_per_token (config.rs:1088-1092) |
| **Pin** | a profile value (§8) carrying a stated property, never law — the ceiling is a pin whose v1 value is set empirically by the #548 map-size arms (D1) | §8; #852 model cards, #1218 family chain |
| **Cut / CutSet** | a declared omission carrying a class from the profile registry (§8). The CutSet is the **retained list of `(path, class)` pairs** the manifest names but extraction skipped — known by path and class, never by the symbols inside | the three silent truncation stages (api_surface.rs:308, 325-329; semantic.rs:498-499) made loud |
| **Verdict (lookup)** | `Found \| NotGathered \| NoSuchSymbol` — total, exclusive, exhaustive under the **conservative rule** (SC-L5): `NoSuchSymbol` is assertable only on a cut-free snapshot | `SymbolIndex::classify` semantics (newt-core/src/symbols.rs:153-254), reformulated conservative |
| **Lens / focus** | a re-render of the same index at the same budget with rich detail on one crate, one-liners elsewhere | `render(focus: Option<CrateId>)` |
| **Frozen head** | the compaction-immune leading system messages; the surface block's residence. Mid-session block swaps require an explicit `MemoryProvider` contract amendment (§5.4) — the shipped contract freezes the block at session start (memory.rs:114-121) | `head_len` (compress.rs:998-1010); guarded by `knowledge_base_stable_base_survives_compression` (compress.rs:3018-3044) |
| **Kernel crate** | the dedicated leaf crate (working name `newt-nav-kernel`) housing the pure kernels — `resolve_budget`, the merge fold, `select`/`render`, `classify`, `attenuate` — with an allowlisted dependency set (WF-2). Charon translates at crate granularity; this carve-out is the extraction precondition, not hygiene | §6.1 structural precondition |
| **Layer** | a #1219 drop-in data file; artifacts resolve by merge-by-name fold | `merge_packs` (api_surface.rs:176-193), `load_packs_from_dir` (api_surface.rs:148-169) |
| **Chain** | knob resolution: operator > model card > family > global default | #1218; #852 cards |
| **Payload** | the L2.5 crew-dispatch knowledge slice (file list + lens slice), gated on L2 bench results (D4). The leaf's snapshot is **distinct** from the parent's (§3.5): leaves execute in worktrees forked from `base_ref`, blind to the parent's dirty tree | closes crew gap (h): today only `task.goal`+`scope` cross the seam (plan_exec.rs:111-130); crew.rs:129-156 |

**Encodings:** TOML at rest (maps, cards, knobs — #1219 drop-ins); JSON on the
tool wire (`where_is`, `focus`, payload). Field names are normative; unknown
fields MUST be ignored; all wire objects carry `"v": 1`. Snapshot manifests
hash under the profile's hash pin (§8) — the property required is collision
resistance, the algorithm is a pin.

## 2. The seam

The index core is **mechanism**; everything that names a crate, a path, a
purpose, a threshold, or a budget number is **data** (three-Cs: knowledge =
data). Between them sit two seams:

```
 data layers (#1219)            newt-nav-kernel (leaf crate)   consumers (#1206)
┌──────────────────────┐       ┌───────────────────────┐      ┌────────────────────┐
│ language packs        │ fold  │ pure kernels:         │      │ newt-tui: frozen-  │
│ workspace-map.toml    │──────►│  extract · merge ·    │views │  head placement,   │
│ ~/.newt/workspace-    │       │  resolve_budget ·     │─────►│  tool advertisement│
│   maps/<name>.toml    │       │  render(focus) ·      │      │ crew dispatch:     │
│ model cards (#852)    │ chain │  classify · attenuate │◄─────│  payload assembly  │
│ family defs (#1218)   │──────►│ ONE retained Index    │ asks │ headless drivers   │
│ nudger knobs          │       │  per Snapshot         │      │ future gila/hermes │
└──────────────────────┘       └───────────────────────┘      └────────────────────┘
```

**The #1206 split + kernel carve-out (WF-2).** The reusable core — index
type, layered merge, `resolve_budget`, `render`, `classify`, `attenuate` — is
library code exposing structs and pure APIs, housed in the dedicated kernel
crate (§1) whose dependency set is an explicit allowlist. It MUST NOT contain
TUI rendering, interactive prompting, harness wiring, or any LLM-client
dependency. Consumers own placement: the frozen-head injection
(`MemoryProvider` seam, memory.rs:257-263 → tui:6764-6779), the 4-touch tool
seam (§5.3), crew prompt assembly. This mirrors ceremony §2 ("libraries
define the space; consumers define the layout") and the existing `CrewRunner`
trait-injection precedent (crew_tool.rs:46-49).

**What this spec is not.** No new store, no embeddings index, no server (the
Sourcegraph/Cody freshness-economics lesson): the index is recomputed at
initialize from the working tree — freshness by construction. Claim-check
ground truth is the **retained** index, never re-read bytes (§4 SC-L4
corollary). The knowledge repo (`~/workspaces/knowledge`) contributes the
*pointer-card format* and zero data; loreserver is a VCS and irrelevant;
modulex remains the MCP seam for operational state, not for this index —
native in-process computation beats any tool call here (knowledge-assets seam
map, bottom line).

## 3. Wire objects

### 3.1 workspace-map.toml (tier-1 pointer cards)

Repo layer `.newt/workspace-map.toml`, user layer
`~/.newt/workspace-maps/<workspace-name>.toml`; lowest layer is the
convention-extracted fallback (lib.rs first doc-comment line — follows the
hand-written seed, D2). Resolution per SC-L1: merge-by-name, later layer wins,
first-seen order, tolerant load.

```toml
v = 1

[[crate]]
name = "newt-tui"                          # merge key
purpose = "terminal UI; slash commands"    # ONE line, rendered verbatim (SC-L4·i)
paths = ["newt-tui/src/commands/"]         # key paths, workspace-relative
entry = "newt-tui/src/lib.rs"              # optional; else entry-point convention
                                           # (api_surface.rs:291-297)

[[crate]]
name = "newt-core"
purpose = "agent loop, tools, memory, compression"
paths = ["newt-core/src/agentic/"]
```

No hosts, IPs, GPUs, or machine-specific facts in shipped data (#852 rule).
Injected map lines are workspace-derived facts, so #1214 claim-check can
mechanically refute lies about them (SC-L4 corollary).

### 3.2 where_is (request / response)

Request — **name-shaped** (WF-1): a bare symbol string plus at most closed
enums. No query language, no path expressions, no traversal operators.

```json
{ "symbol": "run_crew", "kind": "fn" }        // kind optional; domain defined below
```

`kind`'s domain is **the set of kind tokens occurring in the session's
resolved packs** — `SymbolRule.kind` is free-form pack data
(config.rs:1153-1159), so no static enum exists; the tool schema is generated
per session from the resolved set (newt already builds tool schemas at
runtime). WF-1: an unknown kind in a request is a typed error, never a guess.

Response — a typed verdict, never free prose, never a guess:

```json
{ "v": 1, "verdict": "found",
  "witnesses": [ { "path": "newt-scheduler/src/crew.rs", "kind": "fn" } ] }

{ "v": 1, "verdict": "not_gathered",
  "cuts_open": ["walk_file_cap", "file_byte_cap"],
  "fallback": { "engine": "rg", "scope": "cut_files",
                "evidence": [ "newt-tui/src/lib.rs:4711: …" ] } }

{ "v": 1, "verdict": "no_such_symbol",
  "fallback": { "engine": "rg",
                "evidence": [ "newt-core/src/agentic/mod.rs:412: …" ] } }
```

The verdicts follow the **conservative rule** (SC-L5): `no_such_symbol` is
assertable only on a cut-free snapshot (`CutSet = ∅`). On any snapshot with
open cuts, a miss returns `not_gathered` with `cuts_open` listing the open
cut classes as **possibilities** — never a per-symbol attribution, which the
system cannot make (cut files are known by path, never by the symbols inside
them). An implementation MAY sharpen a `not_gathered` response with a bounded
rg scan over the retained cut-file list; its hits render as labeled fallback
**evidence** — never a verdict upgrade. `fallback` is present only on a miss,
explicitly headed as fallback evidence, never index fact (SC-L5). The tool
description is self-teaching, gilabot-style, so a 35B prefers one classified
round to N grep rounds.

**Snapshot plumbing (not model-visible).** Every response is stamped with the
snapshot (manifest CID + generation) **harness-side**: the consumer records
verdict + snapshot per tool round in session state for #1214 claim-check, and
strips the stamp from the model-visible rendering by default (a debug knob
re-enables the echo). The model never needs the CID; ~60 chars per round
matters at 8k.

### 3.3 focus (request)

```json
{ "crate": "newt-tui" }        // name-shaped (WF-1); null / omitted clears the lens
```

Response: acknowledgment plus open cuts
(`{ "v":1, "focused": "newt-tui", "cuts": ["block_budget"] }`). Block stats
(char counts, entry counts) are debug/telemetry outside this spec — no law
consumes them, so no field carries them (every field earns its place). The
re-render obeys SC-L6: same budget, in-place head swap, byte-identical
round-trip on clear within one generation.

### 3.4 Surface-budget fields (model card / family — pins, not laws)

```toml
# ~/.newt/models/<model>.toml (#852) or family default; resolved per SC-L1's chain:
# operator > card > family > global. All values are pins (D1; §8).
[context.api_surface]
floor_chars   = 2000       # lower clamp — dominates on the w = 8_192 fallback path
pct_of_budget = 5          # percent of the DISCOVERED BUDGET (§1 Terms): the resolved
                           # session send budget in tokens at provider construction
ceiling_chars = 16384      # CONFIGURABLE pin; shipped default set by the #548
                           # 16k/32k/64k map-size arms (D1) — no literature-inherited cap
max_symbols_per_file = 12  # pin (was the silent api_surface.rs:308 constant);
                           # held constant across the §6.3 map-size arms (controlled var)
auto_focus    = true       # D3 knob — homed HERE (card/family) until the nudger ships;
                           # gentle-ON for small-model families, OFF for frontier,
                           # one operator gesture disables (#1218)
auto_focus_min_turns_between = 4   # swap hysteresis — data, never a code constant (§5.4)
```

Token→char conversion is pinned to the static `[context.estimation]
chars_per_token` value (config.rs:1088-1092) — NEVER the live calibrated
`estimate_ratio` (mod.rs:347-366), so `resolve_budget` stays a session-fixed
pure function of `(w, chars_per_token, cfg)`. `floor ≤ ceiling` is WF-3,
checked at config load; shipped defaults MUST NOT pin `floor = ceiling`
(SC-L2). **Legacy:** a present `max_block_chars` (config.rs:1211-1213) is
read as an operator-layer `floor_chars = ceiling_chars = max_block_chars` pin
with a deprecation warning — the constant becomes a pin routed *through* the
formula, never a bypass, so SC-L2 holds without an exception window and the
migration is a pure data rewrite. The raw knob is removed after one minor
version.

### 3.5 Crew dispatch payload (L2.5 — gated, D4)

Binding shape for any implementation; funding is gated on L2 bench results.

```json
{ "v": 1,
  "task": "…",
  "files": [ "newt-tui/src/commands/mod.rs" ],
  "lens":  "<focused tier-2 slice — verbatim signatures, extracted over the LEAF's tree>",
  "snapshot": { "manifest": "cid:…(base_ref tree)", "generation": 1 },
  "caveats": { "…": "the leaf's effective caveats — the dispatch_caveats result
                     (session ⊓ clamp), serialized typed (hermes-audit §A4)" }
}
```

**Snapshot coherence (normative).** The leaf executes in a worktree forked
from committed state (`base_ref`, crew.rs:129-156) — the parent's dirty
working tree is invisible to it. The payload lens MUST therefore be
**re-extracted over the leaf's `base_ref` tree at dispatch** (cheap:
extraction is per-line regex over the payload file list), and the payload's
snapshot CID names the *leaf's* tree — one index per snapshot; the leaf's
snapshot is distinct, and SC-L3 binds per snapshot. If re-extraction is
unavailable, the lens MUST drop to file-list-only: a parent-tree lens stamped
into the leaf's context is fabrication by provenance mismatch delivered
through the honesty machinery itself.

`caveats` carries the actual serialized effective-caveats object — typed and
verifiable, satisfying hermes-audit §A4's "state the child's met-down caveats
in its prompt" — never a free-prose label. The payload rides `run_crew`
prompt assembly AFTER `dispatch_caveats` (crew_runner.rs:260-262) and BEFORE
planner-context build, entering through the same read-authority gate as
curated file contents (crew.rs:450-454) — one choke-point, one predicate.

## 4. The Laws (normative)

Seven laws, merged from three drafted lenses (ten candidates; cut ledger in
§7). Each carries a proof obligation (PO) over a pure kernel; §6.1 maps POs to
the formal track (`newt-agent/formal/` — the existing Lean lake project and CI
`formal.yml` that already carry the caveat-lattice proofs). Per the governance
rule, **nothing joins this section without a proof obligation demanding it**.
Values (order pins, hash algorithms, class names, defaults) live in the
profile (§8), never in law text.

### SC-L1 — Resolution is a layered fold

Every navigation-knowledge artifact — language packs, workspace maps, budget
configs, focus heuristics, payload templates — MUST be pure data in #1219
drop-in layers; code holds only mechanism and MUST NOT embed workspace facts
(crate names, paths, budget constants beyond declared data-defaults). Artifact
resolution MUST be a deterministic fold: merge-by-name, later layer wins,
first-seen order preserved; load is tolerant (missing dir → empty; malformed
file → `none` with warning — the IO half) and a skip MUST NOT perturb any
other key (locality — a property of the pure post-parse fold over
`List (Option Pack)`). Behavioral knobs MUST resolve **total and
first-defined** down the #1218 chain (operator > card > family > global — a
global default always exists; no knob reaches "undefined"), stable under
extension by lower-precedence layers. The two clauses are one theorem read
from opposite ends of the layer list: first-defined-wins over precedence ≡
last-wins over reversed layers.

**SC-PO-1:** (a) `lookup (merge ls) k = last_defined ls k`; (b) skip locality
over the pure post-parse fold —
`merge (ls with element i ↦ none)` agrees with `merge ls` on every key not
defined solely by element `i` (the parse-tolerance half — malformed TOML →
`none` with warning — is a well-formedness predicate discharged by the §6.2
malformed-layer vector, not Lean); (c) chain totality + extension stability.
`merge_packs` (api_surface.rs:176-193) is pure but keys through
`std::collections::HashMap` (api_surface.rs:179-180), which has no shipped
Aeneas model — **Aeneas after the data-structure swap** (sorted Vec-of-pairs),
**plain-Lean until then**; the chain resolver is modeled **plain-Lean** first
(caveat-lattice style) until the #1218 resolver is carved pure.

### SC-L2 — The budget is proportional

The tier-2 budget MUST be produced by `resolve_budget` at every render:
`b = resolve_budget(w, r, {floor_chars, pct_of_budget, ceiling_chars})` =
`clamp(floor_chars, ⌊pct/100 × w⌋ × r, ceiling_chars)`, where `w` is the
**discovered budget** (§1 Terms — the resolved session send budget in tokens
at provider construction) and `r` the static `chars_per_token` pin (§8; never
the live calibrated ratio, so the function is session-fixed and pure).
`resolve_budget` is monotone non-decreasing in `w`, clamped
`floor ≤ b ≤ ceiling` for every `w` including `w = 0`, total under saturating
arithmetic. On the discovery-failure fallback (`w = 8,192`, memory.rs:520)
the floor clamp dominates by construction — the fallback path can never
re-create the starved-map failure (vector, §6.2). Shipped defaults MUST NOT
pin `floor_chars = ceiling_chars` (the fixed 3,000-char constant beside a
210k budget is the observed failure); an *operator* pinning floor = ceiling
is lawful operator supremacy under SC-L1's chain. A change in `w` mid-session
(model switch, probe ratchet) is a freshness event (§5.7): the law holds at
every render, not only at t0. The ceiling is a **pin, not a law value** (D1):
it lives in the model card, resolves through the family chain, and its
shipped default is set by the #548 map-size arms — no literature-inherited
hard cap appears in code. The legacy `max_block_chars` maps into the formula
as an operator pin (§3.4) — never around it.

**SC-PO-2:** (i) monotonicity in `w` for fixed `(r, cfg)`; (ii) clamp under
WF-3; (iii) totality — all over the session-fixed signature
`resolve_budget(w_tokens, chars_per_token, cfg) → chars`. Single pure
arithmetic function — the easiest **Aeneas extraction** in the spec; **pilot
it first** to prove the Charon→Aeneas pipeline here. Success criterion is
explicit: if the pilot fails, every Aeneas row in §6.1 downgrades to
plain-Lean **without weakening any law** — no law depends on the extraction
toolchain landing.

### SC-L3 — One index, many lenses

Each session MUST compute the index once at initialize from the working tree
plus resolved data, and retain it — one index per snapshot (a crew leaf's
snapshot is distinct, §3.5). The L0 surface, the tier-1 map render, `where_is`
verdicts, focus-lens renders, and L2.5 payload slices MUST all be pure
functions of the single retained index value at the current generation. No
view listed in this law may recompute, shadow-index, or present any other
source **as index fact**. The sanctioned non-index channels — the rg fallback
and the `code_search` embedding tool (`SessionSemanticIndex`, tui:3600) — sit
outside the index's trust domain by construction and are lawful only under
SC-L5's fallback-evidence labeling. Consequence: `where_is` never **asserts**
a file the index doesn't witness — fallback evidence is labeled, never a
verdict; the lens can never show a symbol `where_is` misses; #1214
claim-check refutes against exactly one ground truth per generation. (The
atomicity of a freshness replacement is mechanism, §5.7.)

**SC-PO-3 (view factorization):** `entries(render idx f b) ⊆ entries(idx)`;
`p ∈ witnesses(where_is idx s k?) ↔ (p, s, k) ∈ entries(idx) ∧ k matches k?`;
`verdict(where_is idx s) = Found ↔ ∃ p k. (p, s, k) ∈ entries(idx)`;
`payload_slice idx files ⊆ entries(idx)`. **Aeneas-extractable** — `render`
is pure and unit-tested today (api_surface.rs:301-341, 486-523); lookup and
slice are new pure functions over the retained table.

### SC-L4 — Rendering is witnessed, map-total, budget-monotone, and declares its cuts

Four clauses, one kernel (the de-stringed `select`/`render`), one law:

**(i) Witnessed.** Every fact in a model-visible map artifact MUST be produced
by a deterministic pure function of (a) a stated snapshot — named by manifest
CID + generation, recorded in session state — and (b) declared data. No
stochastic link may appear on the path from tree to prompt: map artifacts MUST
NOT be LLM-generated, LLM-summarized, or paraphrased by compaction (discharged
structurally by WF-2's no-LLM-client dependency lint, not by a theorem).
Verbatim signatures at the editing layer; prose one-liners only in tier-1
data. The gather walk itself MUST be deterministic — sorted before caps, per
WF-4 — so the snapshot is a function of the tree. The frozen-head invariant is
this clause observed at a second point on the same path.

**(ii) Map-total / tier-1 indelible.** In every render of an **enabled**
session (WF-3's profile gate scopes enablement), the tier-1 crate map appears
**whole and verbatim, at every budget, after every squeeze**: the budget
governs tier-2 only — `|render(m,e,b,f)| ≤ |m| + b + K`, where `K` is the
profile's fixed marker reserve (§8): cut markers and rollup lines are charged
to `K`, tier-2 facts to `b`, so the `b = 0` render (map whole, tier-2 empty,
marker present) satisfies the bound exactly. No tier-2 byte is emitted before
tier-1 is complete, and no crate silently vanishes. This kills the observed
wrong-crate failure at its root: a starved map that still *looks* complete.

**(iii) Budget-monotone eviction.** Under a stated total eviction order — per
the profile (§8) — that is a function of **the entries and the focus** alone:
`∀ f. b₁ ≤ b₂ ⇒ tier2(render(m,e,b₁,f))` is a prefix of
`tier2(render(m,e,b₂,f))`. The order's v1 value is a profile pin, not law
text; swapping it (e.g. PageRank, OQ3) is a profile rotation plus PO re-proof,
never a law amendment.

**(iv) Declared partiality.** Facts are omitted **iff** an in-band marker is
emitted naming a declared class from the profile registry (§8). The silent
pre-cuts — `MAX_FILES = 400` walk-order and `MAX_BYTES = 200_000` per file
(semantic.rs:498-499) — MUST be lifted into declared parameters whose
exceedance renders as declared rollup lines (WF-4, PR-0). A partial map that
presents as total is a lie the model will repeat.

**Corollary (what pays for the law):** because every fact carries a witness
against a named snapshot and the render is a function, #1214 claim-check
refutes mechanically in two tiers: **primary** — recompute `select` over the
**retained entry table** of the claim's stamped generation (pure comparison,
no file IO; the harness retains the last N generations' entry tables —
entries, not blobs); **full** — re-run `select ∘ extract` only when the
working tree still hashes to the claim's manifest (verify-then-recompute). A
claim citing an evicted generation reports **"unverifiable: snapshot
superseded"** — a fourth, honest outcome per #1214 — never a spurious
refutation from a moved tree.

**SC-PO-4:** (a) soundness `entries(select(extract(fs,p),b,f)) ⊆ facts_of(fs,p)`;
(b) indelibility + map totality over enabled sessions —
`∀ b f. tier1(render) = m` and `∀ c ∈ crates(idx). map_line(c) ∈ render` —
**pilot clause** (the one whose absence was empirically observed on #548);
(c) prefix monotonicity per fixed focus + order totality, proved
**parametrically over any total order that is a function of (entries,
focus)**; (d) `CutSet ≠ ∅ ⇔ marker emitted`, with the conservation bound
proved with `K` as a parameter. The former clause (e), "determinism", is
**retired as a Lean theorem** — every Lean function is deterministic by
construction, so the theorem was vacuous; its real content is re-homed:
deterministic gather → WF-4's sorted-walk conformance vector; no stochastic
tree→prompt step → WF-2's dependency lint; kernel determinism is certified by
the Aeneas extraction itself when it lands. `select`/`render` are
**Aeneas-extractable** once render is split into de-stringed entry selection
(`Vec<Entry>` in/out + cut flags); the `facts_of`/witness side is
**plain-Lean** with rule-match as an opaque decidable predicate (regex engines
are not Aeneas-viable) — the existing caveat-lattice style.

### SC-L5 — A miss is a verdict, never a guess

`where_is` — and every lookup over the retained index — MUST be total and MUST
return exactly one of three verdicts under the **conservative rule**:

- **Found** — witnessed in the retained index (all witnesses returned);
- **NotGathered** — not witnessed AND `CutSet ≠ ∅`: the response lists the
  **open cut classes** (`cuts_open`) as possibilities, never a per-symbol
  attribution — the index knows cut *paths* and *classes*, never the symbols
  inside a region it did not extract;
- **NoSuchSymbol** — not witnessed AND `CutSet = ∅` (complete gather): only a
  cut-free snapshot licenses an assertion of global absence.

It MUST NOT return an unwitnessed location, MUST NOT rank guesses, and MUST
label rg/`code_search` fallback output as fallback **evidence**, never index
fact or a verdict upgrade. An implementation MAY sharpen `NotGathered` with a
bounded rg scan over the retained cut-file list — its hits are labeled
evidence, per this law. The empty answer is a first-class informative answer —
the anti-confabulation floor (the tool must never be a second training prior).
The conservative rule is the SC-L4 interlock delivered honestly: on any
snapshot with open cuts, a miss can never assert global absence, so
misclassifying a partiality miss as absence — fabrication by omission — is
impossible **by construction**, without requiring knowledge of symbols in
regions never read.

**SC-PO-5:** totality; mutual exclusivity + joint exhaustiveness — immediate
from the case split (witnessed / ¬witnessed ∧ CutSet ≠ ∅ / ¬witnessed ∧
CutSet = ∅), all cases decidable over the retained index plus the retained
cut list; miss soundness —
`NoSuchSymbol(s) ⇒ CutSet = ∅ ∧ s ∉ facts_of(gathered)` (and on a cut-free
snapshot `facts_of(gathered) = facts_of(snapshot)`);
`NotGathered(s) ⇒ CutSet ≠ ∅ ∧ s ∉ facts_of(gathered)`;
`Found(s, W) ⇒ W ≠ ∅ ∧ ∀ w ∈ W. w ∈ facts_of(gathered)`. Every statement is
in one sort and computable over the pure kernel plus the retained cut-file
list. `classify` is match/lookup logic with no IO, but its `SymbolIndex` keys
through `BTreeSet` (symbols.rs:153-155) — **Aeneas after the data-structure
swap** (sorted Vec), **plain-Lean until then**; the snapshot-relative clauses
live in the same **plain-Lean** model as SC-PO-4 (shared `facts_of`).

### SC-L6 — The lens conserves

A focus re-render redistributes **detail, never coverage, and never bytes**:

(i) the total entitlement is preserved — `|render(m,e,b,f)| ≤ |m| + b + K`
with the same `b` and the same marker reserve `K` (§8); focusing buys no
larger surface, unfocusing shrinks nothing;
(ii) tier-1 is invariant — SC-L4·(ii) already quantifies over `f`; this
clause *cites* it, adding no second copy;
(iii) every crate remains named: non-focused crates MAY degrade to their
tier-1 one-liner but never below;
(iv) detail is focus-monotone: the focused crate's rendered symbols are a
superset of that crate's symbols in the unfocused render at equal budget
(consistent with SC-L4·(iii): the eviction order is a function of (entries,
focus), and the profile's focus clause ranks focused-crate entries first);
(v) the re-render **replaces the head block in place** — post-state has
exactly one surface block, resident in the frozen head, never appended to the
trimmable tail; and, **within one generation**, rendering `focus = None`
after any focus history reproduces the unfocused block **byte-for-byte**
(render is a function; no hidden ratchet). Across a generation bump (§5.7 —
freshness, window change) the block legitimately differs.

**SC-PO-6:** clauses (i)–(iv) over the pure `render` kernel —
**Aeneas-extractable**, same extraction unit as SC-PO-4. Clause (v) is a
property of the stateful head slot: model it **plain-Lean** as a
generation-indexed transcript `List Msg` with `head_len`
(compress.rs:998-1010): within generation `g`,
`swap_g(swap_g(t, block_f), block_∅) = swap_g(t, block_∅)`;
`count_surface_blocks(swap_g(t, x)) = 1`; and `compress` is the identity on
`[0, head_len)` — which **restates in the model what the shipped regression
test** `knowledge_base_stable_base_survives_compression`
(compress.rs:3018-3044) **checks executably**. The test remains normative and
load-bearing (§6.2); the Lean model documents the invariant, it does not
replace its enforcement.

### SC-L7 — Knowledge crosses the crew seam attenuated

Binding on any L2.5 implementation (D4 gates the funding, not the law). The
dispatched knowledge payload MUST be filtered under the leaf's **effective
caveats** — the same `session ⊓ clamp` meet already computed at
`dispatch_caveats` (crew_runner.rs:260-262, #749 step 2) — by the **same
predicate the curate step enforces** (`permits_fs_read`, crew.rs:450-454). The
payload channel MUST NOT hand a leaf facts about paths its caveats forbid it
to read: otherwise the map becomes an authority side-channel that outruns the
lattice. Attenuating leaf caveats MUST only shrink the payload. The payload
MUST cohere with the leaf's snapshot (§3.5). This law does NOT restate
`leaf ⊑ orchestrator` — that is upstream law (`meet_never_amplifies`;
ceremony L4/PO-4; the shipped formal/ caveat-lattice proofs). SC-L7's only
new content: **knowledge is authority-shaped** and rides the same lattice.

**SC-PO-7:** no invention — `facts(attenuate(P,c)) ⊆ facts(P)`; caveat
soundness — `∀ f ∈ attenuate(P,c). permits_fs_read(c, path(f))`; monotone in
authority — `c' ⊑ c ⇒ attenuate(P,c') ⊆ attenuate(P,c)`. **Plain-Lean**, a
direct extension of the existing caveat-lattice model (path-tagged fact sets
over the same meet-semilattice; three short lemmas atop meet monotonicity).

### Well-formedness predicates (binding conformance; deliberately not laws)

- **WF-1 — Name-shaped queries.** The input schemas of `where_is` and `focus`
  are bare strings plus closed enums only: no query language, no path
  expressions, no traversal operators, no composable syntax. The `kind` enum
  is the session-computed resolved-pack kind set (§3.2); an unknown kind is a
  typed error, never a guess. Traversal, ranking, and flattening are
  harness-side, pre-computed into prompt or one-shot tool semantics. Enforced
  by schema lint; the empirical evidence is CodexGraph (Qwen2-72B collapses
  to 5% EM vs GPT-4o's 27.9% the moment the small model drives Cypher) plus
  the #548 rounds metric itself. The injection-surface residue is already
  covered: the SC-L4/SC-L5 kernels take `Symbol` as an opaque token, never a
  program.
- **WF-2 — Kernel carve-out + library/consumer split** (#1206; §2). The pure
  kernels live in the dedicated leaf kernel crate (§1). CI lints that can
  fail: (a) kernel-crate dependencies ⊆ the declared allowlist — no tokio,
  serde, regex, LLM clients, or std collections beyond what Aeneas models
  ("no dep on newt-tui" alone is vacuous: the build graph already forces it);
  (b) the kernel exposes only typed values (`Index`, `Entry`, `CutSet`,
  `Verdict`) — the one String-producing render lives behind the
  consumer-facing view layer; (c) no LLM-client dependency anywhere on the
  tree→prompt path (discharges SC-L4·(i)'s no-stochastic-link clause
  structurally). Plus the consumer checklist.
- **WF-3 — Budget config + profile gate.** `floor ≤ ceiling` checked at load.
  **Profile gate (normative):** when the resolved tier-1 map alone exceeds
  `floor_chars`, the knowledge_base technique disables for the session at the
  existing enable point (tui:3514-3530) and logs it — never a
  lawful-but-crowding render, never a weakened tier-1. SC-L4·(ii)'s
  quantifier is scoped to enabled sessions; the small-window bench arm (§6.3)
  tunes where the disable threshold sits. Plus the fallback vector
  (`w = 8,192` → `b = floor_chars`).
- **WF-4 — Gather determinism + completeness declared.** The gather walk is
  **sorted (lexicographic, workspace-relative) before any cap applies**, so
  the gathered set, the manifest, and the CutSet are deterministic functions
  of the tree (today's `ignore::WalkBuilder` order is unsorted,
  semantic.rs:498-503 — platform-varying manifests would make CutSets,
  verdicts, and #548 arms non-reproducible). Walk/byte caps are parameters,
  not constants; the manifest names the **full candidate walk** (§1); any
  exceedance is marked partial; tier-1 rollup counts (`N files, M syms`) are
  computed over the complete gather or carry a partial marker. Discharged by
  the double-gather identical-manifest vector plus #1214; feeds SC-L4·(iv)
  and SC-L5's cut list.

## 5. Mechanism (below the law line)

Mechanisms implement or discharge the laws; they add no new ones. The ladder
ships one concern per PR with a #548 bench gate between rungs.

### 5.0 L0-pre — gather honesty floor (PR-0)

Prerequisite for every bench arm: a degradation curve measured over a
silently corrupted gather pins nothing. Sort the walk (lexicographic,
workspace-relative) before caps (WF-4); lift `MAX_FILES`/`MAX_BYTES`
(semantic.rs:497-523) into declared parameters; hash the **full candidate
walk** into the manifest (naming is never skipped — only extraction); retain
the cut list `(path, class)`; replace the bare cut line
(api_surface.rs:327-329) with per-crate rollup lines naming the cut class.
Today files past #400 in walk order and any file >200KB (newt-tui/src/lib.rs
among them) never reach the extractor, silently — after PR-0 they are named,
counted, and marked.

### 5.1 L0a — proportional budget (PR-1: budget formula ONLY)

`ApiSurfaceConfig` grows `{floor_chars, pct_of_budget, ceiling_chars,
max_symbols_per_file}` beside the deprecated `max_block_chars`
(config.rs:1186-1217; legacy mapping per §3.4). Resolution happens at the one
point the TUI already holds the discovered budget — the mem_budget site
(newt-tui/src/lib.rs:3478-3494), immediately before provider registration
(tui:3524-3529); the provider consumes it at `new()` (api_surface.rs:251-258)
→ render gate (api_surface.rs:326). Budget math this must respect
(context-economics seam map): a 262k-window model resolves to `w = 167,772`
tokens (the double-80% = 64% path, lib.rs:5019-5020 + mod.rs:327-341) with
≥145k effectively free — 5% ≈ 8.4k tokens ≈ 33k chars, clamped to the
ceiling; an 8k local model affords only ~300–650 tokens, where the floor
dominates — which is why the formula is proportional and why the fixed
3,000-char constant was wrong in both directions. Token→char conversion uses
the static `chars_per_token` pin, never the live ratio (§3.4). A mid-session
change in `w` (model switch, tui:4907; probe ratchet, probe.rs:187-249) is a
**freshness event** routed through §5.7 — SC-L2 holds at every render.
**Gate:** smoke only (budget resolves proportionally; no regression vs
baseline) — the D1 arms run at the PR-2 gate.

**PR-1b (separate; may fold into the workspace-map PR — both are #1219 layer
plumbing):** finish the flagged drop-in-dir wiring (`load_packs_from_dir`
layers into the live ctor; "wired next" comment tui:3521-3523; live ctor
merges only builtins + inline today, api_surface.rs:263-266) — discharging
SC-L1's fold on the pack carrier.

### 5.2 L0b — untruncatable crate map (PR-2)

Tier-1 (§3.1) is prepended above the surface **inside the same frozen-head
provider block**; the budget gate moves to after map emission (SC-L4·ii).
Merge-by-name across repo/user/fallback layers (SC-L1); hand-written
newt-agent seed ships now, lib.rs-doc-line extractor follows as the lowest
layer (D2). The de-stringed kernel accounts **check-after-append** (exact
accounting), replacing today's check-before-append with its tolerated +100
overshoot (api_surface.rs:325-329, and the test that tolerates it) — a
behavior change this PR carries; the fixed marker reserve `K` (§8) makes the
conservation bound `|render| ≤ |m| + b + K` provable as stated. **Gate:** the
crate-map arm AND the D1 16k/32k/64k map-size arms run here — after PR-0's
honest gather and over the shipped tiered artifact, so the measured
degradation curve is the artifact's, not a flat file list's. Bench
expectation: **the wrong-crate first guess (`dgx.rs` prior) dies here**.

### 5.3 L1 — where_is (PR-3)

Stop discarding: retain the entry table and the cut list on the provider (or
a session store patterned on `SessionSemanticIndex`, tui:3600) — the same
retention feeds every later rung (SC-L3) and is claim-check's ground truth.
Stamp the snapshot (manifest CID + generation) into session state at
initialize; keep the last N generations' entry tables (entries, not blobs)
for the two-tier checker. Tool wiring is the well-worn 4-touch seam: one
`ToolSpec` in `EXTENDED_TOOL_REGISTRY` (tools.rs:689; registry auto-updates
advertisement), a dispatch arm copying the Option-resourced `code_search`
pattern (tools.rs:3195), a `ChatCtx` field (mod.rs:628 precedent), TUI
population (tui:5446). Verdicts render as fixed strings from the typed enum
under the conservative rule (SC-L5); the rg fallback section is explicitly
headed; the snapshot stamp is harness-side, stripped from the model-visible
rendering (§3.2). Self-teaching tool description, gilabot-style. #1214
claim-check gains the two-tier map-fact checker (SC-L4 corollary): retained
entries primary; extract-recompute only on manifest match; superseded
generation → "unverifiable".

### 5.4 L2 — focus lens + auto-focus knob (PR-4)

`render(focus: Option<CrateId>)`; the provider overwrites its block in place
(swap, never append — SC-L6·v). **Contract amendment (required, not
assumed):** the shipped `MemoryProvider` contract freezes the initial block
("Called once at session start and frozen", memory.rs:114-121) precisely to
keep the KV/prefix cache valid; PR-4 MUST amend the trait explicitly —
frozen-by-default with an opt-in mutable-block capability (or a dedicated
re-render hook) — and update the doc comment in the same PR. The rebuild
trigger is named, not assumed: focus tool dispatch → provider block swap →
explicit `rebuild_system_prompt` invocation (memory.rs:257-263 →
tui:6764-6779). **Cost honesty:** every swap invalidates the KV/prefix cache
and forces a full head+history re-prefill — most expensive on exactly the
small local models this spec serves; auto-focus swap frequency is bounded by
a data threshold (`auto_focus_min_turns_between`, §3.4), and prefill latency
is a measured metric in the §6.3 lens arm so the D3 default is priced
honestly. Triggers, per D3: **both** the model-callable `focus` tool **and**
harness auto-focus (heuristic on repeated `where_is` hits in one crate).
**One knob home:** `auto_focus` and its thresholds live in the
model-card/family layer (#852/#1218 — files that exist today), resolved
operator > card > global, with family inserted when the #1218 resolver lands;
the nudger profile, when it ships, joins as a higher-precedence layer in the
same SC-L1 chain — never a second home, and PR-4 does not block on unbuilt
nudger machinery.

### 5.5 L2.5 — crew dispatch payload (PR-5; GATED on L2 bench, D4)

The payload (§3.5) is a lens slice **re-extracted over the leaf's `base_ref`
tree at dispatch** (snapshot coherence — the parent's session index covers
the parent's dirty tree, which the worktree leaf never sees), riding
`run_crew` prompt assembly after `dispatch_caveats` and before
planner-context build, through the same read gate as curated contents
(SC-L7). This closes crew gap (h) — today only `task.goal` + `scope` cross
the seam (plan_exec.rs:111-130) and a leaf navigates cold, re-burning exactly
the rounds L0–L2 save. The crew.rs grounding read is done (crew-machinery
seam map), so PR-5 starts warm if funded. Property-tested with proptest
alongside `meet_never_amplifies`. The bench arm MUST NOT run without the
SC-L7 filter in place.

### 5.6 L3 — held

Per-region resident sub-agent contexts stay **held** (the #1163 lesson:
hand-off amnesia is compaction amnesia by another door; cross-crate changes
are the common case). Revisit only if L1+L2+L2.5 leave a measured gap.

### 5.7 Freshness events (PR-6 for mtime; window-change wiring lands with PR-1)

A freshness event is any of: mtime-keyed re-extraction (PR-6, optional), a
model switch that changes the resolved window (tui:4907), a probe ratchet
(probe.rs:187-249). On any freshness event the index is re-extracted (or the
budget re-resolved), the session generation bumps, and **the new instance
replaces the old atomically — no view reads a mix of generations; all views
derive from the new instance thereafter** (the stateful complement of SC-L3,
homed here per the §7 ledger). The head block re-renders through the same
swap path as focus (SC-L6·v); SC-L6·(v)'s byte-for-byte round-trip is scoped
within one generation.

## 6. Conformance

### 6.1 Formal obligations

POs extend the existing `newt-agent/formal/` track — the Lean lake project
and CI `formal.yml` that already carry the caveat-lattice proofs.
**Structural precondition (WF-2):** Charon translates at crate granularity,
and newt-core (tokio, serde, regex, FFI, 100+ dependencies) will never be
swallowed — the kernels are carved into the leaf kernel crate with an
allowlisted dependency set; that carve-out, plus two data-structure swaps
(sorted Vec-of-pairs replacing `HashMap` in `merge_packs`,
api_surface.rs:179-180, and `BTreeSet` in `classify`'s `SymbolIndex`,
symbols.rs:153-155 — neither has a shipped Aeneas model), plus de-stringing
`render` into entry selection (`Vec<Entry>` + cut flags in/out), are the
extraction preconditions. The extraction pipeline has never run in this repo
(formal/README.md roadmap item 3) — hence the pilot, whose failure criterion
is stated at SC-PO-2: every Aeneas row downgrades to plain-Lean without
weakening any law.

| PO | Law | Statement proved | Track |
|---|---|---|---|
| SC-PO-1 | SC-L1 | fold determinism + pure-fold skip locality; chain totality + extension stability | `merge_packs` **Aeneas after data-structure swap** (plain-Lean until then); parse tolerance → §6.2 vector; chain **plain-Lean** until the #1218 resolver is carved |
| SC-PO-2 | SC-L2 | `resolve_budget(w, chars_per_token, cfg)` monotone, clamped, total | **Aeneas — PILOT** (single pure fn; proves the pipeline; failure ⇒ blanket plain-Lean downgrade, laws unweakened) |
| SC-PO-3 | SC-L3 | every view factors through the one retained index (set-valued witnesses) | **Aeneas** (pure `render` + lookup + slice, post-carve) |
| SC-PO-4 a–d | SC-L4 | soundness; **map totality (PILOT clause)** + indelibility (enabled sessions); prefix-monotone eviction per fixed focus, parametric in the order; cut ⇔ marker with reserve `K` | `select` **Aeneas** (post de-string); `facts_of` witness model **plain-Lean** (rule-match opaque decidable). Former (e) retired → WF-2 lint + WF-4 vector |
| SC-PO-5 | SC-L5 | conservative classify: total, exclusive, exhaustive; miss soundness in one sort over (index, cut list) | `classify` **Aeneas after data-structure swap** (plain-Lean until then); snapshot-relative clauses **plain-Lean** (shared `facts_of`) |
| SC-PO-6 | SC-L6 | conservation (i–iv); generation-indexed head-slot swap/single-block/round-trip (v) | (i–iv) **Aeneas** (same unit as SC-PO-4); (v) **plain-Lean** transcript model — mirrors, never replaces, the shipped compress test |
| SC-PO-7 | SC-L7 | attenuation: no invention, caveat-sound, authority-monotone | **plain-Lean** atop the shipped caveat-lattice model |
| WF-1 | §3.2/3.3 | name-shaped inputs; session-computed kind enum | schema lint (per-session) |
| WF-2 | §2 | kernel-crate deps ⊆ allowlist; typed-values-only surface; no LLM client on the tree→prompt path | dep-graph + module CI lint (checks that can fail) |
| WF-3 | §3.4 | floor ≤ ceiling; tier-1-exceeds-floor → profile-gate disable; fallback floor dominance | config-load check + vectors |
| WF-4 | §5.0 | sorted walk before caps; caps declared; manifest = full candidate walk; rollup counts total-or-marked | conformance vectors + #1214 |

**Pilots:** SC-PO-2 (easiest extraction; explicit success criterion) and
SC-PO-4b (map totality — the clause whose absence was empirically observed on
#548).

### 6.2 Shared vectors

`tests/vectors/semantic-cheat/*.json`; all implementations MUST produce
identical results (the kyln round-trip-law pattern). Property suites (proptest)
mirror each PO executably.

- `(w_tokens, chars_per_token, cfg) → chars` — incl. `w = 0`;
  **`w = 8_192` (the discovery-failure fallback → `b = floor_chars`)**;
  `w = 167_772` (the double-80% resolution of a 262,144 window → ceiling
  clamp); `pct = 0`; `floor = ceiling` (lawful operator pin);
  `max_block_chars` legacy mapping
- `(layers, key) → value` — incl. malformed-layer fixtures (discharging
  SC-PO-1(b)'s parse-tolerance half) and absent-layer chain combos
  (operator/card/family/global)
- **gather determinism:** same fixture tree, two gathers → identical
  manifests and identical CutSets (WF-4)
- `(fixture tree manifest, packs, budget, focus) → (entries, cuts, marker?)`
  — incl. `b < |map|` and `b = 0` (map whole, tier-2 empty, marker present,
  total bytes ≤ |m| + K)
- **focus pair at equal budget:** focused render's focused-crate symbols ⊇
  unfocused render's (SC-L6·iv); `focus = None` round-trip byte-identical
  within one generation (SC-L6·v)
- classify traps: miss on a snapshot with open cuts → `not_gathered` +
  `cuts_open`; miss on a cut-free snapshot → `no_such_symbol`; **cut-flip:
  same symbol, same tree, gather complete vs gather cut → the verdict flips
  `no_such_symbol` → `not_gathered`**; fuzzy near-miss → verdict per cut
  state + labeled fallback evidence
- `(payload, caveats) → filtered` — incl. deny-one-path proving the lens
  slice drops that crate's rich symbols; payload snapshot names the leaf's
  `base_ref` tree
- claim-check: map-fact check against a mutated line **over the retained
  entry table** (no file IO); claim citing an evicted generation →
  "unverifiable: snapshot superseded"
- a CI map-lint that greps every rendered symbol back to its witness file
  over newt-agent itself
- `knowledge_base_stable_base_survives_compression` (compress.rs:3018-3044)
  remains **normative and load-bearing** as an SC-L4/SC-L6 conformance check,
  extended with a focused block and a map-line survival assertion

### 6.3 The #548 bench matrix (the empirical half of conformance)

Task: #548 rollups; Ornith-1.0-35B vs Claude; metrics:
**rounds-to-first-correct-file** (primary), total rounds to PR,
first-tool-call-correct-crate rate, confabulated-path rate,
rounds-to-honest-miss; the lens arm adds **prefill latency** (KV-cache
invalidation cost per swap). **Repetition:** N ≥ 5 repeats per arm, paired
per-seed comparison — single agentic runs at 35B variance distinguish
nothing. **Ceiling decision rule (D1):** the smallest map-size arm within one
standard deviation of the best becomes the shipped family-default ceiling
pin. `max_symbols_per_file` is **held constant across the map-size arms as a
controlled variable** (§8) — the arms vary breadth, deliberately not depth; a
depth-scaling arm is a recorded follow-up candidate. **Family scope:** "per
family" means per *benched* family — initially Ornith only; other families
inherit the Ornith-derived defaults until benched, which is exactly what the
#1218 chain is for. **Run budget (costed before PR-0 lands):**
≈ (3 map-size + 5 fixed arms) × 2 models × N=5 ≈ **80 full agentic runs**
(the local DGX node + Claude API) — the gate cadence is priced, not assumed.

| Arm | Rung gated | What it measures / decides |
|---|---|---|
| baseline | — | pre-change reference (the 2026-07-15 run) |
| budget smoke | PR-1 | budget resolves proportionally; no regression vs baseline |
| crate map | PR-2 | wrong-crate first-guess death; first-tool-call-correct-crate rate |
| **map-size 16k / 32k / 64k chars** (D1) | PR-2 (after PR-0's honest gather — the arms measure the shipped tiered artifact, not a flat list over a silently cut walk) | degradation curve vs the RULER/lost-in-middle prediction; the winning arm (decision rule above) becomes the shipped family-default ceiling pin |
| small-window | PR-2 | tunes WF-3's profile-gate disable threshold on 8k-class models |
| where_is + miss-injection | PR-3 | halve exploratory rounds (target); queries for nonexistent and cut-region symbols → confabulated-path rate, rounds-to-honest-miss, cut-flip honesty |
| lens, auto-focus ON/OFF A/B | PR-4 | prices the D3 default (rounds saved vs prefill latency), per benched family (Ornith first) |
| L2.5 payload (**iff funded** — D4) | PR-5 | leaf rounds saved; precondition: SC-L7 filter + snapshot coherence in place |
| adversarial | continuous | mutate one injected map line → #1214 claim-check MUST flag it via the retained-entries path |

### 6.4 Consumer checklist

A conforming consumer:

- [ ] resolves the budget via `resolve_budget` against the **discovered
      budget** (§1 Terms — the resolved session send budget in tokens, never
      the raw window) with the static `chars_per_token` pin; never ships a
      fixed char constant as the budget outside a lawful operator pin (SC-L2)
- [ ] houses the index core in the kernel crate; deps ⊆ allowlist (WF-2)
- [ ] emits the surface block only via the MemoryProvider frozen-head seam —
      never as a per-turn ephemeral prepend (SC-L4·i, SC-L6·v) — and adopts
      the amended mutable-block contract before any mid-session swap (§5.4)
- [ ] keeps exactly one surface block; focus swaps it in place and invokes
      the rebuild trigger explicitly (SC-L6·v, §5.4)
- [ ] renders tier-1 whole and verbatim before any tier-2 byte, or disables
      at the profile gate when tier-1 exceeds the floor (SC-L4·ii, WF-3)
- [ ] emits the cut-class marker whenever anything is omitted, within the
      reserve `K` (SC-L4·iv)
- [ ] routes every view — map, `where_is`, lens, payload — through the one
      retained index at the current generation; treats window changes as
      freshness events (SC-L3, §5.7)
- [ ] renders lookup misses from the typed verdict under the conservative
      rule; heads fallback output as fallback evidence (SC-L5)
- [ ] keeps `where_is`/`focus` schemas name-shaped with the session-computed
      kind enum (WF-1)
- [ ] stamps snapshot manifest CID + generation into session state
      harness-side (stripped from the model-visible rendering) and retains
      the last N generations' entry tables for claim-check (SC-L4·i,
      corollary)
- [ ] (crew) applies the SC-L7 filter after `dispatch_caveats`, before prompt
      assembly; re-extracts the lens over the leaf's `base_ref` tree or drops
      to file-list-only; refuses payload dispatch without both (§3.5)

## 7. Governance — law minimalism

A good system has only the laws it absolutely needs. **Nothing enters §4
without a proof obligation demanding it; everything else is mechanism (§5),
well-formedness, or profile (§8).** The count is audited ruthlessly; every
audit is recorded here. Amendments are PRs against this document; a new law
must arrive with its PO and its bench arm — no law without a vector suite, no
bench arm without a law.

- **Executed (merge round, 2026-07-15):** three drafted lenses, ten candidate
  laws → seven. Honesty-K1 + budget-M2 + seams-K4 unified as **SC-L4** — four
  clauses of one law about one pure kernel (witnessed, map-total, monotone,
  declared), the ceremony-L2 precedent of multiple directions in one law.
- **Executed:** "head residency" refused as a standalone law — compaction
  identity on the head is shipped, regression-tested mechanism
  (compress.rs:3018-3044); the genuinely new obligations (in-place swap,
  exactly-one-block, tail-never, byte round-trip) were absorbed as
  **SC-L6·(v)**, where the plain-Lean transcript model naturally lives.
- **Executed:** name-shaped queries demoted to **WF-1**; library/consumer
  split demoted to **WF-2** (dep predicates); counted-claim honesty demoted
  to **WF-4** (render-input predicate; #1214 does the refutation).
- **Executed (review round 1, 2026-07-15):** SC-L3's atomic-replacement
  sentence moved below the law line (§5.7) — a stateful property no PO
  demanded; SC-L3 now states only view purity, which SC-PO-3 covers.
- **Executed (review round 1):** SC-PO-4's former clause (e), "determinism",
  retired as a Lean theorem — vacuous by construction (every Lean function is
  deterministic); its real content re-homed to WF-4 (sorted-walk gather
  determinism, vector-discharged) and WF-2 (no stochastic tree→prompt step,
  lint-discharged).
- **Executed (review round 1):** SC-L5's verdict rule reformulated
  **conservative** — the drafted per-symbol cut attribution demanded
  knowledge of symbols in regions never read (unimplementable; `names(CutSet)`
  was not a computable object); `NoSuchSymbol` now requires a cut-free
  snapshot, and every PO clause is decidable over the retained index plus the
  retained cut list.
- **Executed (review round 1):** pins evacuated from law text into **§8
  Profile** (eviction-order value, snapshot hash, cut-class registry, budget
  defaults, marker reserve, `max_symbols_per_file`) — SC-L4·(iii)/(iv) demand
  an order/class *per the profile*; pin swaps become profile rotations plus
  PO re-proof, never law amendments.
- **Refused:** the budget formula's *values* as law — floor, pct, ceiling are
  pins (D1; three-Cs: data, not law); honesty under ANY budget is already
  SC-L4·(iv). Refused restating `leaf ⊑ orchestrator` — upstream, proved,
  cited (SC-L7 note).
- **Next candidates:** SC-L1+SC-L2 ("configuration resolves deterministically"
  — different algebra today; watch the Lean). SC-L4+SC-L5 — both share
  `facts_of` and one snapshot model; if the formulation lands cleanly they may
  be one law — *"every answer, positive or negative, is a witnessed verdict
  about a named snapshot"* — on two carriers, and seven becomes six.

## 8. Profile v1 (pins, not laws)

Values are **implementation details**; each pin states the *property* any
replacement must carry (ceremony §8 precedent). Card/family-resolvable pins
travel the SC-L1 chain; the rest are named code defaults selectable per the
chain when alternatives ship. Rotating a pin is a profile rotation plus PO
re-proof — never a law amendment.

| Pin | v1 value | Required property (the law's interest) |
|---|---|---|
| Eviction order | `entry_first_rev_alpha_focus`: focused-crate entries precede all non-focused tier-2 entries; within each class, entry points never evict before non-entry-points of the same crate (api_surface.rs:291-297), non-entry-points evict last-path-alphabetical-first. Named code default, chain-selectable when a second order (e.g. PageRank, OQ3) ships | total order; a pure function of (entries, focus) — SC-PO-4c is proved parametrically over any order with this property |
| Snapshot hash | BLAKE3-256 multihash, canonical form (the content-addressable v1 profile) | collision resistance (manifest naming) |
| Budget defaults | `floor_chars = 2000`, `pct_of_budget = 5`, `ceiling_chars` set by the #548 map-size arms (D1) | WF-3 (`floor ≤ ceiling`); shipped defaults never pin floor = ceiling (SC-L2) |
| `chars_per_token` | the static `[context.estimation]` value (config.rs:1088-1092) | session-fixed — SC-PO-2's purity is over this signature; never the live calibrated ratio |
| `max_symbols_per_file` | 12 (was the silent api_surface.rs:308 constant); card/family-resolvable | WF-3-checked; held constant across the §6.3 map-size arms as a controlled variable |
| Cut-class registry | `block_budget`, `file_symbol_cap`, `walk_file_cap`, `file_byte_cap` | exhaustive over the pipeline's omission stages (SC-L4·iv, WF-4); extending the pipeline extends the registry |
| Marker reserve `K` | declared kernel constant, set in PR-2 | fixed per kernel version; the conservation bound `\|render\| ≤ \|m\| + b + K` is proved with `K` as a parameter |

## 9. Recorded decisions (Shawn, 2026-07-15 — constraints, not open questions)

- **D1 — Map budget:** proportional formula (floor + pct of discovered budget)
  with a CONFIGURABLE ceiling whose value is EMPIRICALLY MEASURED — the #548
  bench matrix gains 16k/32k/64k-char map-size arms. No literature-inherited
  hard cap. (Bound into SC-L2; measured in §6.3 at the PR-2 gate over the
  honest gather.)
- **D2 — Crate map:** hand-written `.newt/workspace-map.toml` ships FIRST as
  seed data (newt-agent authored now); the lib.rs-doc-line convention
  extractor follows as the fallback (lowest) layer. (§3.1, §5.2.)
- **D3 — Focus lens:** BOTH the model-callable focus tool AND harness
  auto-focus; auto-focus is a nudger knob — gentle default ON for small-model
  families, OFF for frontier, one-gesture disable (#1218 chain). Interim
  authority home is the card/family layer until the nudger ships (§5.4).
- **D4 — Crew dispatch (L2.5):** GATED on L2 bench results; the crew.rs
  grounding read happened now so PR-5 starts warm if funded. L3 stays held.
  (§5.5, §5.6; SC-L7 binds regardless of funding.)

## 10. Open questions

1. **tree-sitter timing.** Language packs declare tree-sitter as the target
   engine (api_surface.rs:20-28). Does regex-floor `where_is` accuracy suffice
   for the bench, or does a noisy miss-injection arm pull the tree-sitter rung
   forward? The PR-3 arm decides.
2. **L2.5's L3-adjacency.** The spec's position: payload-carrying leaves are
   workers, not resident contexts, so SC-L7-governed dispatch is not an L3
   breach — but the hold is Shawn's to interpret.
3. **Eviction-order evolution.** The v1 pin (§8) is deliberately crude;
   aider-style def/ref PageRank is a candidate **profile rotation** — it needs
   its own bench arm before adoption, and SC-PO-4c re-proves over the new
   order (the PO is already parametric, so this is a value proof, not a law
   amendment).
4. **Multi-witness rendering.** `where_is` returns all witnesses in index
   order (the POs are set-valued); is kind-disambiguation (the session kind
   enum, §3.2) enough for symbols defined in many files (e.g. `new`), or does
   the response need a per-crate grouping hint? Watch the PR-3 arm's round
   counts.

*Resolved into the spec (review round 1):* the snapshot-hash question is now
a §8 profile row; the tiny-window question is resolved normatively by WF-3's
profile-gate disable, with the small-window arm (§6.3) tuning the threshold.

## 11. Relations

- newt-agent#1216 — this spec's issue; #548 — the bench; #1214 — mechanical
  honesty / claim-check; #669 — frozen-head stable base; #1199 — window
  discovery; #852 / #1218 — model cards and family chain; #1219 — lean-config
  drop-ins; #1206 — library/consumer split; #749 — dispatch caveats; #1126-H —
  crew backend inheritance; #948 — subagent tool (adjacent, not this spec).
- agent-bridle `docs/spec/ceremony-contract.md` — house shape (§8 profile
  discipline included); upstream authority laws (L4 attenuation) cited by
  SC-L7.
- `newt-agent/formal/` — the Lean lake project and CI these POs extend;
  Charon→Aeneas extraction is that track's roadmap item 3 (unproven here —
  hence the SC-PO-2 pilot and its downgrade criterion).
- `docs/design/crew-swarm-overseer.md`, `docs/design/hermes-audit-2026-07.md`
  §A4 — crew/dispatch design of record for the L2.5 rung.
