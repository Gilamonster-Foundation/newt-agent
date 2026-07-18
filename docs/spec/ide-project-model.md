# The IDE for LLMs — project model & drift-cache

**Status:** DRAFT 0.1.0. Normative for the *substrate* the Semantic Cheat
surface (`semantic-cheat.md`) sits on. Companion to that spec: the laws SC-L1..7
govern how repo knowledge *crosses the model/tool line*; this spec governs how
the harness *derives and caches* that knowledge — like an IDE.

## 0. Frame

newt scans the folder it launches in, **detects the project** (build system +
languages), derives a **project model** (a per-unit catalog: units, source
roots, symbol surface, dependency graph), **caches** it, and on the next launch
re-derives only what **drifted** from the last scan; the index is **rebuildable**
on demand. This is how an advanced IDE builds project structure — the difference
is the reader: the **LLM**. Two payoffs, one per tier: a small model becomes
*able* to navigate (a correct, always-fresh model that never confabulates is the
capability floor); a large model is *faster* and more token-efficient.

The Semantic Cheat's two resolvers ride this substrate: **`where_is`** (exact,
typed-verdict, SC-L5) over the model's symbol table, and **`code_search`**
(by-meaning, on-host embeddings) as labeled fallback evidence (SC-L3/L5). The
map floor (SC-L4) is the *rendering* of this model.

## 1. Terms

- **Unit** — a project unit: a Cargo crate, a Python/Dart/TS package, a module.
- **Project pack** — pure data (the `LanguagePack` shape, `api_surface.rs`) that
  recognises a build system and says how to derive units, source roots, the
  dependency graph, and which language pack extracts symbols. Droppable,
  merge-by-name (SC-L1).
- **Project model** — `{ units, source_roots, symbol_surface, dep_graph }`,
  derived per unit from the scanned tree.
- **Scan / drift / cache** — the scan reads unit content; the cache persists the
  model keyed `(repo, pack-set, chunker-version)`; drift is the set of units
  whose content changed since the cached scan (content-hash, not mtime alone).

## 2. Project packs (the plugin surface)

A pack detects marker file(s) and derives units. **Out-of-the-box targets — and
these four freeze the plugin shape:**

| Target | Marker(s) | Units |
|---|---|---|
| Rust | `Cargo.toml` (+ `[workspace]`) | crates / members |
| Python | `pyproject.toml` / `setup.cfg` | packages / top-level modules |
| Dart | `pubspec.yaml` | packages |
| TypeScript | `package.json` / `tsconfig.json` (+ workspaces) | packages / entrypoints |

**Java** (`pom.xml`/`build.gradle`), **C#** (`*.csproj`/`*.sln`), and others are
**plugins** — droppable `~/.newt/project-packs/*.toml`, no core change. The pack
contract **freezes only after the first four agree** (freeze-minimally). Layer
precedence: repo `.newt/project-packs/` > user `~/.newt/project-packs/` >
built-ins (SC-L1, later-wins). The hand-written `.newt/workspace-map.toml` is the
**Rust instance** of a derived model; PR-2's loader renders it.

## 3. The Laws (normative — LAW MINIMALISM; each carries a proof obligation)

### PM-L1 — Derivation is a pure per-unit fold
`derive(pack, tree) = fun u => deriveUnit(pack, tree[u])`. The model is a
function of `(pack, tree)` alone — no hidden state, compositional over units.
*(PO-A. Basis of caching and of PO-C.)*

### PM-L2 — Drift is content-keyed and complete
Drift flags a unit **iff** its content changed; and content change is implied by
any *model* change. So a unit skipped as clean provably did not change its
model — **no false-clean**. A false-clean would serve stale navigation and
violate SC-L5 ("a miss is a verdict, never a guess" — and neither is a stale
hit). Discharged by a collision-resistant content hash over full unit content
(multihash law: hash-equality ⇒ content-equality). *(PO-B.)*

### PM-L3 — The cache refines the scan (incremental ≡ full)
`applyDrift(cached, drift(t₁,t₂), t₂) = derive(pack, t₂)`: the drift-updated
model **equals** a from-scratch rebuild. This is what makes *freshness by
verification* true — the IDE "cheat" (drift-only rescan) is provably equivalent
to an honest full scan. *(PO-C — the centerpiece.)*

### PM-L4 — The model is a witnessed projection, never authored
Every model entry is a verbatim projection of a real file fact — never
LLM-generated, LLM-summarised, or paraphrased by compaction (inherits SC-L4·i).
A query miss over the model is a typed verdict (SC-L5). *(PO-D — enforced
structurally by the no-LLM-client lint on the tree→model path, WF-2-style, not a
Lean theorem.)*

### PM-L5 — Packs are sound and confluent
A pack applied to a tree it does not detect derives the **empty** model (no
phantom project); applied to one it detects, its units are a subset of the real
filesystem (no phantom units); multiple packs merge deterministically
(merge-by-name, later-wins — SC-L1), so plugin composition is confluent.
*(PO-E; confluence inherits SC-PO-1.)*

## 4. Formal obligations

These extend `formal/` (the Lean lake project + `formal.yml` gate). Unlike the
Semantic Cheat POs (several "plain-Lean until a carve"), the project-model core
is **mechanized now** — self-contained plain Lean 4, no Mathlib,
`sorry`-free, checked by `lake build`:

| PO | Law | Statement proved | Track |
|---|---|---|---|
| **PO-A** | PM-L1 | `po_a_derive_congr` — equal trees derive equal models (purity) | Lean ✓ `ProjectModel/Basic.lean` |
| **PO-B** | PM-L2 | `po_b_drift_complete` — a derived-model change ⇒ content-keyed drift flags the unit (no false-clean) | Lean ✓ |
| **PO-C** | PM-L3 | `po_c_incremental_eq_full` — `applyDrift(derive t₁, drift t₁ t₂, t₂) = derive t₂` | Lean ✓ **centerpiece** |
| **PO-E** | PM-L5 | `po_e_non_match_empty` — a non-detecting pack derives the empty model | Lean ✓ |
| **PO-D** | PM-L4 | no LLM client on the tree→model path; entries are witnessed projections | WF-lint (dep-graph + map-lint), per SC-L4/WF-2 |

**Modelling assumption made explicit (PO-B):** a unit's `Content` stands in for
its bytes via a content hash; `DecidableEq Content` is hash comparison — so
"hash-equality ⇒ content-equality" is the collision-resistance the Rust side
discharges with a multihash. Weakening the hash weakens PO-B and nothing else
(LAW MINIMALISM: the assumption is named where it bites).

**Shared vectors** (`tests/vectors/ide-project-model/*.json`, kyln
round-trip-law pattern): `derive` determinism (same tree twice → identical
model); drift completeness (edit one unit → exactly that unit flagged, others
clean); incremental = full (edit / add / delete a unit → drift-update byte-equals
a fresh derive); non-match → empty; pack merge precedence.

## 5. Mechanism (below the law line)

The scan is the honest gather (`semantic-cheat.md` §5.0 / PR-0); the cache is the
persistent, content-hash, drift-tracked store (epic #1277 story #1282, folding
the mtime freshness of PR-6); the packs + `derive` are story #1288; the render is
PR-2 (crate/package map) + PR-1 (proportional surface); the resolvers are PR-3
(`where_is`) + `code_search`.

## 6. Relations

- `semantic-cheat.md` — the navigation **surface** (SC-L1..L7, the bench matrix).
  Unchanged; this spec is the **substrate** it derives over. SC-L1 (layered
  fold) governs pack merging; SC-L4·i / SC-L5 bind PM-L4.
- `formal/ProjectModel/Basic.lean` — the checked proofs (PO-A/B/C/E).
- Epic **#1277** (the roadmap); stories **#1288** (packs), **#1281** (scan),
  **#1282** (drift-cache).
