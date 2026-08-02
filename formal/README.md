# `formal/` — machine-checked OCAP specifications (#902)

Our OCAP formalisms have been **smeared across prose** (ADRs, the caveat-lattice
paper) and only *sampled* by property tests. This tree makes the load-bearing
security invariants **theorems** — total, machine-checked in **Lean 4** — so an
implementation can't be locally-correct-but-globally-wrong.

See `docs/vision.md`: *sub-agent = tool call = bounded delegation.* The one thing
that must always hold is that a delegation never **amplifies** authority. Here
that is proven, once, for the algebra everything else composes from.

## What's checked today

`CaveatLattice/Basic.lean` — a self-contained (no Mathlib) model of
`agent-mesh-protocol`'s `Scope` / `Caveats` / `meet`:

- **Attenuation-only (the keystone)** — `meet a b ⊑ a` (and `⊑ b`): a meet can
  never grant more than either operand.
- **`meet` is a genuine lattice meet** — greatest lower bound (`le_meet`),
  commutative / associative / idempotent (up to authority-equivalence), with
  `Scope.all` as top.
- **The confused-deputy bound** — `meet caller grant ⊑ caller`: a dispatched
  delegation (crew / sub-agent / re-minted tool call) can act with *no more*
  authority than its caller. Since sub-agent = tool call, this is the theorem
  that certifies delegation-as-tool-calling never leaks authority.
- **Composition** — `delegation_chain_bounded`: any chain of delegations stays
  `⊑` the original caller (the *global* property sampling can't certify).

`ProjectModel/Basic.lean` — the "IDE for LLMs" project model + drift-cache
(#1277, `docs/spec/ide-project-model.md`). newt derives a project model on
launch, caches it, and re-derives only the units that *drifted*. The drift-only
rescan is a performance CHEAT; these theorems make it SOUND:

- **PO-A** `po_a_derive_congr` — derivation is a pure function of (pack, tree)
  (the basis of caching).
- **PO-B** `po_b_drift_complete` — a derived-model change implies content-keyed
  drift flags the unit: **no false-clean**, so the cache never serves stale
  navigation (SC-L5).
- **PO-C** `po_c_incremental_eq_full` (the keystone) — the drift-updated model
  **equals** a from-scratch rebuild: `applyDrift(derive t₁, drift t₁ t₂, t₂) =
  derive t₂`. This is what makes *freshness by verification* true.
- **PO-E** `po_e_non_match_empty` — a pack that doesn't detect its build system
  derives the empty model (no phantom project).

`ProjectModel/Basic.lean` also proves displayed-actions-only authorization
(`PromptForm` namespace) so hidden actions cannot authorize.

The permission path is the first concrete instantiation of this contract:
`newt-core::Question` is emitted once, then consumed by both the TTY and HTMX
surfaces through the same typed action list (`action.parse` in both consumers,
shared by store row and form serialization).
`PromptControls.tla` + `.cfg` model-check Submit, Esc back/cancel, immediate
Ctrl-C/Ctrl-D exit, and terminal exit behavior.

Self-contained (no Mathlib), `sorry`-free. PO-D (no LLM authoring the model) is a
dep-graph/map lint, not a theorem.

## How to check it

Needs a Lean toolchain (via [`elan`](https://github.com/leanprover/elan); the
version is pinned in `lean-toolchain`). No Mathlib.

```sh
cd formal
lake build        # checks every theorem; exit 0 iff all proofs go through
java -cp /path/to/tla2tools.jar tlc2.TLC PromptControls.tla \
  -config PromptControls.cfg -coverage 1
```

CI runs this on every change under `formal/` (`.github/workflows/formal.yml`), so
a Rust change that would break a proven invariant fails the build — the "formal
specifications engine" of #902 in its first, smallest form.

## Roadmap (#902)

1. **This** — the caveat-lattice invariants (attenuation, meet-semilattice,
   confused-deputy bound). ✅
2. Extend the model with the remaining `Caveats` components (`max_calls`
   count-bound, `valid_for_generation`) and the **enforcement floor**
   (`widen(base,g).meet(clamp) ⊑ clamp`) + **re-mint soundness** (`policy ⊑
   root`).
3. **Tie the spec to the real Rust** via Charon → Aeneas extraction, so the
   theorems are proven about the *actual* `agent-mesh-protocol` code, not a
   hand-model — the full "kept-in-sync" engine (see #902 for the toolchain).
4. The toolchain / agentic-workflow surfaces (per-axis enforcement honesty; the
   dispatch topology as a non-amplifying router).
