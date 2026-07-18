/-
  ProjectModel — machine-checked correctness of newt's "IDE for LLMs" project
  model + drift-cache (epic #1277).

  newt scans the folder it launches in, detects the project (build system +
  languages), derives a *project model* (a per-unit catalog), caches it, and on
  the next launch re-derives only the units that *drifted* — exactly how an IDE
  builds project structure. The drift-only rescan is a performance CHEAT; what
  makes it SOUND is proved here, once, self-contained (no Mathlib, like
  `CaveatLattice/Basic.lean`):

  - **PO-A** the derivation is a pure function of (pack, tree);
  - **PO-B** content-keyed drift is COMPLETE — it never misses a model change
    (a false-clean would serve stale navigation, violating the honesty law
    SC-L5);
  - **PO-C** the incrementally-updated model EQUALS a from-scratch rebuild — the
    theorem that makes "freshness by verification" true (the epic's founding
    claim);
  - **PO-E** a pack that does not detect its build system derives the empty
    model (no phantom project).

  The one modelling assumption made explicit: a unit's `Content` stands in for
  its bytes via a content hash, and `DecidableEq Content` is hash comparison —
  so "hash-equality ⇒ content-equality" is the collision-resistance the Rust
  side discharges with a multihash (the workspace's multihash-over-algos law).
-/

namespace ProjectModel

variable {U C M : Type}

/-- The scanned tree: the content of each unit (crate / package / module).
    `Content` is a content hash; `DecidableEq` is hash comparison. -/
abbrev Tree (U C : Type) := U → C

/-- The derived project model: one derived unit-model per unit. -/
abbrev Model (U M : Type) := U → M

/-- A project pack (Rust `Cargo.toml`, Python `pyproject.toml`, Dart
    `pubspec.yaml`, TS `package.json`, …). Pure data:
    - `deriveUnit` projects one unit's content to its model — a pure function
      (PO-A) and a verbatim projection of file facts, never LLM-authored (PO-D,
      enforced structurally by the no-LLM-client lint, not modelled here);
    - `detect` recognises the build system over the whole tree (PO-E);
    - `empty` is the model of a non-matching tree. -/
structure Pack (U C M : Type) where
  deriveUnit : C → M
  detect     : Tree U C → Bool
  empty      : M

/-- Derive the whole model, per unit — compositional by construction. -/
def derive (p : Pack U C M) (t : Tree U C) : Model U M :=
  fun u => p.deriveUnit (t u)

/-- Detection-gated derivation: a tree the pack does not recognise yields the
    empty model (no false project detection). -/
def deriveGated (p : Pack U C M) (t : Tree U C) : Model U M :=
  fun u => if p.detect t then p.deriveUnit (t u) else p.empty

/-- Content-keyed drift: the units whose content changed between two scans.
    This — and only this — is what the persistent cache re-derives. -/
def drift [DecidableEq C] (t1 t2 : Tree U C) : U → Bool :=
  fun u => decide (t1 u ≠ t2 u)

/-- The incremental update the cache performs: re-derive the drifted units from
    the new tree, keep the cached model for the rest. -/
def applyDrift (p : Pack U C M) (cached : Model U M) (d : U → Bool)
    (t2 : Tree U C) : Model U M :=
  fun u => if d u then p.deriveUnit (t2 u) else cached u

/-! ### PO-A — derivation is a pure function of (pack, tree)

Equal trees derive equal models: the model carries no hidden state, so a rescan
of an unchanged tree reproduces the cache exactly (the basis of caching at all). -/
theorem po_a_derive_congr (p : Pack U C M) {t1 t2 : Tree U C} (h : t1 = t2) :
    derive p t1 = derive p t2 := by
  rw [h]

/-! ### PO-B — drift completeness (no false-clean)

The load-bearing theorem. If a unit's *derived model* changed, content-keyed
drift flags it. Equivalently: content-hash equality is enough to skip a unit
safely — a skipped (clean) unit's model provably did not change. A false-clean
is impossible, so the cache never serves stale navigation (SC-L5). -/
theorem po_b_drift_complete [DecidableEq C] (p : Pack U C M)
    (t1 t2 : Tree U C) (u : U)
    (hmodel : p.deriveUnit (t1 u) ≠ p.deriveUnit (t2 u)) :
    drift t1 t2 u = true := by
  have hc : t1 u ≠ t2 u := fun he => hmodel (by rw [he])
  simp only [drift, ne_eq, decide_not, Bool.not_eq_true', decide_eq_false_iff_not]
  exact hc

/-! ### PO-C — incremental ≡ full rebuild (the centerpiece)

Applying content-keyed drift to the cached model yields *exactly* the model a
from-scratch rebuild would produce. This is the refinement that certifies the
drift-cache: IDE speed with a machine-checked guarantee that the cache never
diverges from an honest fresh scan. -/
theorem po_c_incremental_eq_full [DecidableEq C] (p : Pack U C M)
    (t1 t2 : Tree U C) :
    applyDrift p (derive p t1) (drift t1 t2) t2 = derive p t2 := by
  funext u
  simp only [applyDrift, derive, drift, ne_eq]
  by_cases hu : t1 u = t2 u
  · simp [hu]
  · simp [hu]

/-! ### PO-E — pack soundness: a non-matching tree derives the empty model -/
theorem po_e_non_match_empty (p : Pack U C M) (t : Tree U C) (u : U)
    (h : p.detect t = false) :
    deriveGated p t u = p.empty := by
  simp only [deriveGated, h, Bool.false_eq_true, if_false]

end ProjectModel
