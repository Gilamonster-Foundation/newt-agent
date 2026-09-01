# Key Precedence Ladder — build plan

**Status:** accepted, implementers follow this document
**Supersedes discussion in:** `docs/decisions/esc_and_vi_contract.md` §4 (the ADR remains the *policy*; this is the *mechanism*)
**Blocking prerequisite:** the ADR is not on `main` — it lives on `origin/docs/esc-contract` (`60364def`). PR #2011 lands first or nothing below can cite it.

---

## 0. What survived review, and what did not

Three designs and four adversarial passes produced eleven fatal/serious findings. The design below is smaller than any of the three inputs, because most findings were resolved by **deletion**, not by mitigation. Section 8 answers each one with evidence.

The three biggest cuts, stated up front so no implementer reintroduces them:

- **No escalation tiers in the crate.** Both newt consumers already count presses and tier correctly (`presenter.rs:975-987`, `lib.rs:11766-11783`) and already reset the counter correctly. A copy in the crate is a second reset path.
- **No TLA+ in the crate.** A pure function has no temporal behaviour. Shipping a temporal spec beside it would be the decorative artifact `spec/tla/README.md:18-22` forbids. TLA+ goes where the sequencing is: newt's context stack.
- **No `Claim` witness type, no validated constructor, no conformance `trait Harness`.** Resolution order makes the escape-hatch property unconditional; a witness type protecting it is ceremony, and a Rust trait cannot cross PyO3 or wasm-bindgen without the callback shim every design correctly rejected for claimants.

---

## 1. The crate

**Name:** `precedence-ladder` (crates.io + PyPI), Rust lib `precedence_ladder`, Python module `precedence_ladder`.
**Home:** its own repo, `Gilamonster-Foundation/precedence-ladder`.
**Template:** `/home/hartsock/workspaces/content-addressable`, copied file-for-file.

### Reuse verdict — genuinely new, and here is the search that says so

Nothing in the workspace resolves "who owns this trigger right now."

| Candidate | Why it is not this | Anchor |
|---|---|---|
| `newt-core/src/tty/arbiter.rs` | **No priority field anywhere in `Inner`** (`:98-120`). Resolves by geometry (`Region::intersects`) + caller policy, first-come-first-served. Leases are RAII across time; precedence is recomputed per event. Process-global `OnceLock<(Mutex, Condvar)>` (`:122-127`) — unshippable as a published crate's core, and hostile to proof. Failure mode is overpaint (safety over geometry), not starvation (reachability over configurations). | Sibling, never a dependency. One idea crosses: `OnCollision`'s declared-intent principle (`:259-262`). |
| herdr `src/config/keybinds.rs` (2298 lines) | Owns **what a chord means** — `KeyCombo`, `normalize_key_combo:1309`, `key_event_matches_combo:1320`. Precedence answers **who owns it now**. | Different question. `Trigger` stays an opaque string; the first PR that teaches it modifier parsing has forked herdr. |
| `newt-tui/src/palette.rs:349` `palette_step` | The *shape* to copy — pure verdict fn, unit-tested in file, loop acts only on the verdict. | Copy the shape, not the code. |
| `newt-tui/src/lib.rs:11748-11757` | The one place a claimant is ranked above the hatch with the reasoning written down (#1704). | This crate is the extraction of that. |

Six hard-coded ladders exist (`rich_input.rs:1812`, `rich_input.rs:251`, `vi.rs:193`, `presenter.rs:961`, `lib.rs:11733`, plus 12 standalone `event::read()` loops). No shared resolver. Verified: `grep -rn "esc_claimed\|claimant\|precedence"` returns only unrelated config-precedence code.

### Why its own repo

Version identity, not taste. Every newt workspace member carries `version.workspace = true` (`newt-interaction/Cargo.toml:3` → `newt-agent/Cargo.toml:77` = `0.8.0`). An in-tree ladder ships as `0.8.0` on newt's cadence, so wyvern or a foreign TUI pins newt's entire release train to get a keystroke predicate. `publish = false` is the in-tree default anyway, and newt's `release.yml:298-380` `check-publish-order` pre-flight exists because monorepo publishing already burned v0.6.5 and v0.7.2.

### Boundary — out, each with the anchor that proves inclusion would fork it

| Excluded | Anchor |
|---|---|
| crossterm / ratatui / any terminal type | The classic watcher never sees a crossterm event — raw `libc::read` at `lib.rs:11713`. A `KeyEvent` in the signature forks the crate on day one. |
| Split-Esc grace window | `escape_grace_ms` (`lib.rs:11675`) is a raw-fd artifact; crossterm disambiguates (`presenter.rs:955`). ADR §2 drops it. Third copy already in herdr `src/raw_input.rs:451-470`. |
| Escalation tiers, press counter, reset rule | Already correct and already reset correctly in both consumers. See §0. |
| Interrupt delivery | `turn.cancel`/`turn.hard` (`chat.rs:7168-7178`), `set_interrupt_pending`, `persist_incomplete_turn` (`chat.rs:7702-7712`). The crate returns a verdict; the host performs the effect. |
| tty ownership, modal preemption | On `SurfaceRequest::Interact` the presenter blocks inside `handle_request` (`presenter.rs:813-877`) and never reaches `poll_keys`. A newt structural fact, supplied as context or not at all. |
| Trait-object claimants, builder-with-closures, a `Harness` trait | Knowledge back into logic; re-enters consumer state mid-decision; dies at both FFI boundaries. |
| `EditorOutcome::Interrupt`, staged/timer Esc, rebindable keymap | ADR lines 123, 125, §6 Q3 — already rejected there. |

### The one thing newt gets from it, stated honestly

`Verdict::Claimed(_)` does **not** dispatch. The editor still re-derives its own winner through `MountedEditor::on_event` → `Editor::input`. So at runtime the ladder's output in the cockpit is **one bit**: escape or pass. The claimant *name* earns its place through two non-runtime consumers — `describe()` (which needs to know *which* claimant owns Esc to stop advertising `^C interrupt` at an idle prompt) and the registration conformance test. Do not sell rung ordering as behaviourally observed in newt; it is not.

---

## 2. The API

### The escape-hatch invariant, precisely

> For every ladder `L`, every situation `s` with `s.work_running = true`, and every trigger `t ∈ L.reserved`: `resolve(L, t, s) = Escape`.

**Made unrepresentable by resolution order, not by validation.** The reserved check precedes the rung scan, so no value of `rungs` can affect it. There is therefore no constructor to make fallible and no validation a foreign consumer can forget to call. Non-emptiness of `reserved` — the thing that keeps the statement from being vacuous — is also by construction: `Hatch::new` takes a first element plus a rest.

Scope, stated so nobody overclaims it: this is a property of **one ladder**. It is *not* the claim "the operator can always get out of newt". That harness-level claim needs every input context to have a ladder, and newt has 12 unconverted contexts today. §5's two-sided ratchet is what discharges it, incrementally and countably.

```rust
// precedence-ladder/src/lib.rs
#![forbid(unsafe_code)]
// Runtime closure: serde + toml behind default feature "table";
// content-addressable 0.1.2 behind off-by-default "cid". Core `resolve` has none.

/// Non-empty by construction: no `Default`, no `new(Vec)`, no `Result`.
/// An empty reserved set would make the escape-hatch theorem vacuous, so it
/// is not expressible.
pub struct Hatch { reserved: BTreeSet<String>, action: String }
impl Hatch {
    pub fn new(action: impl Into<String>,
               first: impl Into<String>,
               rest: impl IntoIterator<Item = String>) -> Hatch;
    pub fn reserved(&self) -> impl Iterator<Item = &str>;
}

pub struct Rung { pub claimant: String, pub triggers: BTreeSet<String>, pub action: String }

pub struct Ladder { hatch: Hatch, rungs: Vec<Rung>, fallthrough: BTreeSet<String> }

/// Which claimant NAMES are claiming right now. A value, never a trait object:
/// that is what lets the whole API cross PyO3 and wasm as plain data.
#[derive(Default, Clone, PartialEq, Eq)]
pub struct ClaimSet(BTreeSet<String>);
impl ClaimSet {
    pub fn claiming(&mut self, name: &str) -> &mut Self;
    pub fn is_live(&self, name: &str) -> bool;
    pub fn names(&self) -> impl Iterator<Item = &str>;
}

pub struct Situation<'a> {
    pub claiming: &'a ClaimSet,
    /// CEILING: one flat work unit. A harness with a tool call nested inside a
    /// turn has no single answer here and must NOT pass `true` unconditionally —
    /// `describe` would then advertise an interrupt the outer unit will not do.
    /// If you need nesting, keep a depth counter in the consumer.
    pub work_running: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict<'l> {
    Claimed { claimant: &'l str, action: &'l str },
    Escape  { action: &'l str },
    Unbound,
}

impl Ladder {
    /// Infallible. A rung naming a reserved trigger is inert by resolution
    /// order; `collisions()` reports it as an authoring lint, not an error.
    pub fn new(hatch: Hatch, rungs: Vec<Rung>, fallthrough: BTreeSet<String>) -> Ladder;

    pub fn resolve(&self, trigger: &str, s: &Situation<'_>) -> Verdict<'_>;

    /// What `trigger` WOULD do, from the same table that decides it.
    /// Returns a borrowed action label from the ladder — never a crate constant,
    /// so the vocabulary belongs to the consumer and is localizable.
    pub fn describe(&self, trigger: &str, s: &Situation<'_>) -> Option<&str>;

    /// Authoring lint. Catches newt's live case: 0x04 is already
    /// `TurnKey::HalfPageDown` (lib.rs:10786), so "Ctrl-D interrupts" is a
    /// table collision, surfaced at load rather than to an operator.
    pub fn collisions(&self) -> Vec<Collision>;

    /// Drives the registration conformance test in the consumer.
    pub fn claimants(&self) -> impl Iterator<Item = &str>;

    pub fn from_toml(src: &str) -> Result<Ladder, LadderError>; // deny_unknown_fields, schema = 1
    #[cfg(feature = "cid")]
    pub fn content_id(&self) -> content_addressable::ContentId;
}
```

`resolve`, in full — this is the Lean definition transliterated, and the transliteration is checked by the golden vectors (§3):

```rust
pub fn resolve<'l>(&'l self, t: &str, s: &Situation<'_>) -> Verdict<'l> {
    if s.work_running && self.hatch.reserved.contains(t) {   // no rung is reachable here
        return Verdict::Escape { action: &self.hatch.action };
    }
    for r in &self.rungs {
        if r.triggers.contains(t) && s.claiming.is_live(&r.claimant) {
            return Verdict::Claimed { claimant: &r.claimant, action: &r.action };
        }
    }
    if s.work_running && self.fallthrough.contains(t) {       // rung 7: Esc, all declined
        return Verdict::Escape { action: &self.hatch.action };
    }
    Verdict::Unbound
}
```

**Reserved-while-idle is `Unbound`, deliberately.** Verified at `rich_input.rs:280-296`: idle Ctrl-C replaces the textarea, resets vi, and shows `"Ctrl-C to interrupt · Ctrl-D to exit"`. That is consumer policy, not an escape. And `describe("ctrl-c", idle) == None` is exactly what makes the lying hint at `rich_input.rs:395-398` unwritable.

`Verdict` borrows from the ladder; both bindings clone at the boundary (one small clone per keystroke — free at human timescales).

### The table — `newt-tui/assets/esc_ladder.toml`

```toml
schema = 1
[hatch]      action = "interrupt"  reserved = ["ctrl-c"]
fallthrough  = ["esc"]

[[rung]] claimant = "palette"    triggers = ["esc"]  action = "close palette"
[[rung]] claimant = "vi-confirm" triggers = ["esc"]  action = "cancel [y/N]"
[[rung]] claimant = "vi-ex"      triggers = ["esc"]  action = "cancel :"
[[rung]] claimant = "vi-insert"  triggers = ["esc"]  action = "NORMAL"
[[rung]] claimant = "vi-pending" triggers = ["esc"]  action = "cancel operator"
```

**No `modal` rung.** It can never be live — on `SurfaceRequest::Interact` the presenter blocks inside `handle_request` (`presenter.rs:813-877`), and `interaction_view::present` runs its own `event::read()` loop. A dead rung would make the "every claimant is answered" conformance test pass on a constant.

### newt adapter — presenter, replacing `presenter.rs:969-990`

```rust
Event::Key(key) if key.kind == KeyEventKind::Press => {
    let claims = self.editor.claim_set();
    let s = Situation { claiming: &claims, work_running: self.turn.is_some() };
    match ESC_LADDER.resolve(trigger_name(&key), &s) {
        Verdict::Escape { .. } => { self.escape_during_turn(); Ok(()) }
        _ => self.to_editor(Event::Key(key)),
    }
}

/// Single caller, private. Deleting the ladder arm above makes this dead code
/// and `clippy -- -D warnings` fails the PR. (Guard G1, §5.)
fn escape_during_turn(&mut self) {
    let Some(turn) = self.turn.as_mut() else { return };
    turn.presses += 1;                                  // counter stays HERE
    if turn.presses == 1 {
        turn.cancel.store(true, Ordering::SeqCst);
        newt_core::tty::set_interrupt_pending(true);
    } else {
        turn.hard.store(true, Ordering::SeqCst);
    }
}
```

`KeyEventKind::Press` guard preserved — without it, under the kitty protocol the release event counts a second press and the operator's *first* Ctrl-C force-stops.

### newt adapter — claim accessors, defined next to the claimants

`pub(crate)`, not `pub`. `presenter.rs` is in the same crate as `rich_input.rs` and `vi.rs`; no private field needs publicising.

```rust
// newt-tui/src/rich_input.rs
impl MountedEditor {
    pub(crate) fn claim_set(&self) -> ClaimSet {
        let mut c = ClaimSet::default();
        if self.palette.is_open() { c.claiming("palette"); }
        self.editor.claims(&mut c);
        c
    }
}
impl Editor {
    fn claims(&self, c: &mut ClaimSet) {
        // MANDATORY edit gate: `Editor` carries a `Vi` in ALL modes
        // (rich_input.rs:244 `vi: Vi::new()` -> vi.rs:75 `Mode::Insert`), and
        // `Editor::input` gates on this at :255 and :355. Without the gate,
        // `vi-insert` claims Esc permanently under emacs and nano.
        if self.edit == Edit::Vi { self.vi.claims(c); }
    }
}
// newt-tui/src/vi.rs — reads pending/count/insert_normal, which stay private
impl Vi {
    pub(crate) fn claims(&self, c: &mut ClaimSet) {
        if self.confirm.is_some()    { c.claiming("vi-confirm"); }
        if self.ex.is_some()         { c.claiming("vi-ex"); }
        if self.mode == Mode::Insert { c.claiming("vi-insert"); }
        if self.pending.is_some() || self.count.is_some() || self.insert_normal {
            c.claiming("vi-pending");
        }
    }
}
```

### Python

```python
from precedence_ladder import Ladder, Situation
L = Ladder.from_toml(open("esc_ladder.toml").read())
L.resolve("esc",    claiming={"vi-pending"}, work_running=True)   # ('claimed','vi-pending','cancel operator')
L.resolve("ctrl-c", claiming={"vi-pending"}, work_running=True)   # ('escape', None, 'interrupt')
L.describe("ctrl-c", claiming=set(), work_running=False)          # None
```

Plain values in, a tuple of strings out. No callbacks. That is the payoff for refusing trait-object claimants.

---

## 3. The formal layer

**House rule, applied literally** (`spec/tla/README.md:68-75`): TLA+ owns what is quantified over sequences; Lean owns what is quantified over one call. The split falls on the repo boundary because that is where the sequencing actually lives.

### Lean — `formal/Precedence/Basic.lean`, **in the crate repo**

Mathlib-free, `leanprover/lean4:v4.31.0`, `lake-manifest.json` `"packages": []`. Three edits: `formal/Precedence/Basic.lean`, a root `formal/Precedence.lean` that only imports, and `formal/lakefile.toml` — `[[lean_lib]] name = "Precedence"` **plus** adding `"Precedence"` to `defaultTargets` on line 2. A lib missing from `defaultTargets` is not built by `lake build`, so it is not checked; it would be a decorative artifact with a green badge.

Predicate-carrying structures, in the `NewtInteraction/Binding.lean:33-62` idiom, so everything stays `decide`-able.

| Theorem | Statement | Status | Why it is here |
|---|---|---|---|
| `first_match_inv` | `resolve L t s = claimed c → ∃ i, rungs[i].claimant = c ∧ claims ∧ live ∧ ∀ j < i, ¬(claims ∧ live)` | proven | The one shared inversion lemma. The content is the "nothing earlier claimed" half. |
| `resolve_hatch` | `t ∈ L.reserved → s.workRunning → resolve L t s = escape` | **spec** (by-construction) | The keystone. Discharged by `simp [resolve]` because the hatch branch precedes the rung scan. **Registered as `lean = "spec"`, never `"proven"`** — the whole design win is that this is true by construction, and dressing it as a discovered theorem would be dishonest. |
| `declining_rung_is_transparent` | inserting a rung with `claims t = false`, or with `live = false`, **at any index**, leaves the verdict unchanged | proven | **The registration-safety theorem** — the real content in this file. It licenses a new claimant (PR8's `/settings` shell) registering instead of opening a 13th `event::read()` loop. Analogue of `adding_a_differently_named_backend_cannot_fabricate_a_selection` (`NewtPolicy/Basic.lean:59-90`). |
| `describe_agrees` | `describe L t s = actionOf (resolve L t s)` | spec (`rfl`) | One line; makes affordance/behaviour agreement definitional. |

**Non-vacuity block** (mandatory, `decide`, over `newtDemo` = the shipped table):

- `esc` + `vi-insert` + working → `claimed "vi-insert"` — **not** escape
- `esc` + `vi-pending` + working → `claimed "vi-pending"` — the codex bug newt beats (ADR §1 note (a))
- `esc` + no claims + working → `escape`
- `ctrl-c` + **every** claim configuration + working → `escape`
- `ctrl-c` + idle → `unbound` — the draft-clear survives
- `order_is_load_bearing`: two live rungs both claiming `esc`, swapped, change the winner — the deliberate negation of `selection_is_position_independent`. Without it the ladder is a set.

**Cut, and why:** `resolve_total` (functionhood dressed as a theorem); `prefix_invariance` (implied, unused); `nonsilence` (a `simp` weakening of `resolve_hatch`); any tier law (no tiers); any `wf`-hypothesis decoy (**false** under hatch-first `resolve` — `decide` would fail and `lake build` would go red).

**Sorry gate.** `lake build` exits 0 on a `sorry` and `spec/lint-behavior-map.py`'s `lean_decls` regex matches a `sorry`-bodied theorem, so "sorry-free" is a human assertion today. The crate's `formal.yml` adds one line: `! grep -rn '\bsorry\b' formal/ --include=*.lean`. File the same gap against newt's lakefile as a separate issue; do not fix it in this train.

### TLA+ — `spec/tla/KeyContexts.tla`, **in the newt repo**

The crate ships no TLA+. A pure function has no temporal behaviour, and a spec that constrains nothing is the artifact `spec/tla/README.md:18-22` calls worse than none.

What *is* temporal, and what a code change can genuinely falsify, is newt's **context stack** — the fact adversarial review surfaced and I verified: `newt-core/src/tty/modal.rs:143-146` maps Ctrl-C/Ctrl-D to `PromptLine::Exit`, and `newt-tui/src/interaction_view.rs:191-195` maps them to `ExitRequested`, in a frame whose own comment (`:174-175`) says it *"opens DURING a turn"*. Those are real Ctrl-C claimants outranking the presenter's ladder.

```
CONSTANTS Contexts, HasHatch          \* HasHatch[c] \in BOOLEAN
VARIABLES stack, work, last
Push(c) == HasHatch[c] /\ stack' = <<c>> \o stack /\ ...    \* THE load-bearing guard
Pop     == stack # <<>> /\ stack' = Tail(stack) /\ ...
Next == \E c \in Contexts : Push(c) \/ Pop \/ Press \/ WorkStart \/ WorkEnd \/ Done
Spec == Init /\ [][Next]_vars                                \* safety only, no fairness
```

| Invariant | Statement | What falsifies it |
|---|---|---|
| `TypeOK` | — | every spec here has one |
| `EveryActiveContextHasAHatch` | `stack # <<>> => HasHatch[Head(stack)]` | Deleting the guard in `Push`. |
| `PreemptionIsTotal` | `last = "press" => owner \in {Head(stack)}` — a press is resolved by the top context only | A panel that also polls the parent loop. |

**Non-vacuity decoy, mandatory, mirroring `PromptControls.cfg:4-17`'s `AllowSession`:** `Contexts` binds `hatchless` with `HasHatch[hatchless] = FALSE`, and `Next` quantifies over **all** of `Contexts`, so the guard inside `Push` is the single load-bearing check. Delete it and `Push(hatchless)` becomes enabled and TLC produces a real counterexample. The `.cfg` header carries the mutation table naming which guard falsifies which invariant, per `InteractionLifecycle.cfg:16-36`, pasted into the PR body.

**Two mechanical obligations:** `check.sh:113-119` runs bare TLC with no `-deadlock`, so deadlock checking is ON — the `Done` stutter is not optional. And **no liveness properties at all**: this model has non-terminating `Push`/`Pop` cycles, so `WF_vars(Next)` would be satisfied by an infinite mount/unmount behaviour while `Pop` never fires, and every liveness property in every input design would have reported a false counterexample on its first run. Safety only.

**Cut from TLA+, with reasons:**

- `HatchNeverStranded` over a single ladder — a **tautology** under hatch-first resolution. `Resolve(reserved) = escape` in every state, so no value of `mounted`/`claiming` can falsify it and a greedy decoy claimant is inert. The only mutation that breaks it mutates the spec's own definition. Correctly cited, correctly cut.
- `ClaimStateSurvivesSubmit` (#2006) — holds because the modeller writes `claimState' = claimState` into `Submit` by hand. With no trace-validation bridge (`spec/tla/README.md` says the `BehaviorEvent` alphabet is not built) it detects nothing about the code. The Rust regression test is the only real gate; a TLA+ row would duplicate the claim, not add a second gate.
- `NoHatchPressIsSwallowed` (#2010) — an action property, so it must be declared under `PROPERTY`. Verified: `spec/lint-behavior-map.py:316-321` collects `refs.tla` invariants only from `INVARIANT`/`INVARIANTS` lines, so it could never be cited by a BHV row anyway. It becomes a Rust test in newt (PR N5).

### CI wiring

- **Crate repo `formal.yml`** — elan, `working-directory: formal`, `lake build`, the `sorry` grep, `paths: ['formal/**', <self>]`, and the verbatim `HOOK PARITY: intentionally NONE` block from newt's `formal.yml:6-10`.
- **newt `spec/tla/KeyContexts.{tla,cfg}`** — drop the pair in flat. `check.sh:98-105` globs `*.cfg` and requires a sibling `.tla`; `behavior-formal.yml` already runs it on any `spec/**` change with a wiped `XDG_CACHE_HOME` and asserts `tla-checked-count >= 1`. **Zero workflow edits, no justfile recipe, no manifest.**
- **No vendoring in either direction.** `spec/lint-behavior-map.py:424-425` hardcodes `spec/tla/<spec>.tla` and `:369-372` silently skips a symlink resolving outside the repo, so newt's rows carry `lean = "none"` with `precedence-ladder v0.1` named in the description. Honest gap, not a fake green. Follow-up: a `refs.upstream { crate, version, symbol }` ref kind, ~20 lines of Python, not in this train.

### Golden vectors — the only cross-layer bridge, and it needs codegen

`spec/vectors/ladder.json` holds `(ladder, trigger, situation) → verdict`. Three consumers read it: Lean's `decide` block, newt's Rust `#[test]`, and the stdlib-only Python conformance script.

**A Mathlib-free Lean lib has no JSON reader.** So `just gen-vectors` regenerates the Lean `decide` block from the JSON, and a CI job re-runs it and fails on diff. Without that step the "one artifact, three consumers" claim is one artifact plus a hand transcription with no drift check — which was asserted, never designed, in the input. The `.cfg` is *not* a consumer (TLA+ constants are literal syntax and the TLA+ model is about contexts, not vectors).

Vector identity is minted through `content-addressable` 0.1.2 (matching newt's pin at `Cargo.toml:125` — **0.1.2**, not the 0.1.0/0.1.1 several docs still claim), with a **forward re-encode byte-compare** and `deny_unknown_fields`: `from_canonical_dagcbor` is a bare `from_slice` with no canonicality check, so decode-then-re-encode can mint a different id (newt #1831).

---

## 4. Distribution

### crates.io

Layout copied file-for-file from `content-addressable`: `[workspace] members = ["precedence-ladder-py"]`, `default-members = ["."]` so a plain `cargo test` never compiles PyO3; root = the core crate; `justfile` (`check = fmt clippy test doc leaf`, `verify-release`, `msrv`, `install-hooks`); `scripts/verify_release.py` and `scripts/check-leaf-deps.sh` **verbatim**. `tests/guard.rs` copied from `newt-interaction/tests/guard.rs` — walks the resolved closure **and** scans source for `std::{fs,net,process,env}`, each half with its anti-vacuous twin.

Features: `default = ["table"]` (serde + toml); `cid` off by default (content-addressable pulls blake3 + ipld-core + serde_ipld_dagcbor — unconditional inclusion would make the wasm artifact an order of magnitude larger than the "few KB" a pure predicate should be). Vectors run with `cid` on as a dev-dependency.

### PyPI via PyO3 — abi3-py39, **yes**

`precedence-ladder-py` is a separate **non-default** workspace member: `pyo3 = { version = "0.29", features = ["extension-module", "abi3-py39"] }`, `publish = false`, `doc = false` (the lib-name shadow otherwise breaks `cargo doc --workspace`). **No `python/` dir, no `build.rs`, no per-crate `pyo3` feature** — newt's `sys.modules` stitching (`newt-agent-py/python/newt_agent/__init__.py:52-68`), per-crate features (`newt-tools/Cargo.toml:37`) and macOS link-args `build.rs` exist only to fan eight crates into one module and to support a non-maturin `cargo build`. One crate needs none of it.

**The abi3 argument.** abi3 costs access to the version-specific CPython C API. This binding touches none of it: the entire surface is `str`, `bool`, `set[str]`, and a tuple. No numpy, no buffer protocol, no per-call perf floor. In exchange, **one wheel per platform covers CPython 3.9–3.13 across four platforms including Windows and macOS universal2** (`content-addressable/release.yml:186-201`), versus newt's non-abi3 matrix which builds four interpreters per platform and **skips Windows entirely** (`release.yml:552-560`, blocked on the iroh/quinn graph this crate does not have). A keymap library that cannot install on Windows is not reusable. Take abi3. The ceiling — a future rich stateful `#[pyclass]` API would hit it — goes in the crate's frozen-policy comment, the way `content-addressable` freezes its edition and MSRV.

### npm — wasm-bindgen, one package, deferred

`wasm-pack build --target nodejs` (plus `--target web`), one package `@gilamonster/precedence-ladder`, one `.wasm`. The crate has no fs/net/process/env — the guard already forbids them — so `wasm32-unknown-unknown` compiles with no shims, and the same artifact serves the browser (`newt-web/` already runs a Playwright tier).

**Honest recurring cost:** one CI job, a `wasm32` toolchain target, a ~30-line `Situation` marshalling shim, and — the real one — **a third published version number in lockstep forever**. That is a permanent tax, so it does not gate 0.1.0.

Rejected: **napi-rs** (6–7 platform packages plus a resolver shim, bought with native speed a keystroke predicate cannot need). **A thin JS reimplementation** (barred by reuse discipline, and it would leave the Lean proofs covering code npm users do not run).

The in-tree npm precedent does **not** apply: PR #1222 / `origin/feat/npm-shim` ships CLI *binaries* (`npm/platforms.json`, `optionalDependencies`, `lib/binary.cjs`) — a launcher, not a library. It is **draft on purpose** until 0.8.0 (`docs/ROADMAP.md:723-738`). Never un-draft it.

### Release gate

`content-addressable`'s shape, job for job:

```
provenance → {rust-gate, msrv, formal} → {build-wheels, build-sdist}
           → {install-smoke, sdist-smoke} → release-gate → {publish-pypi, publish-crate}
```

- **`provenance` is the authorization seam** (`release.yml:68-110`): `just verify-release <tag>` (all version declarations agree, via the *single* SemVer→PEP 440 implementation in `scripts/verify_release.py`, which is fail-closed on build metadata and non-canonical prerelease spellings and has its own test); tag == `v<Cargo.toml version>`; tag points at HEAD; `git merge-base --is-ancestor HEAD origin/main`.
- **Publishes never rebuild** — both `needs: [release-gate]` and `download-artifact` exactly what the gate validated (`release.yml:389-398`), so a partial failure is a one-job re-run, never a re-tag.
- **`install-smoke` is the anti-vacuous job**: installs the *built wheel* into a clean venv per OS×Python and asserts the import resolves under `site-packages`, not the checkout (`release.yml:262-263`).
- Top-level `permissions: {}`, third-party actions SHA-pinned with version comments, publishes inside protected `pypi`/`crates-io` environments with required reviewers.
- **PyPI Trusted Publishing (OIDC, `id-token: write`)**, not a token. newt's account-scoped `PYPI_API_TOKEN` exists only because a pending-publisher entry blocked a first publish; for one new repo that registration is a five-minute human step and the better posture. It is **owner-pinned**, so §7 Q1 must be answered before it is created.
- **GPG-signed annotated tag** via the `gpg-signed-tag` skill (an agent pane has no pinentry TTY; never `--no-sign`).
- **The Release is published, never drafted** — `prerelease: true` for an rc, notes CHANGELOG-verbatim. newt's `release.yml:280-287` uses `draft: true` + `generate_release_notes: true`, which makes every newt tag an unfinished release by construction. Do not inherit it.
- `.githooks/pre-push` mirrors `ci.yml` minus `msrv` and `python`, both documented, with a `PIPELINE PARITY:` header. This repo is small enough to mirror honestly. **Do not** inherit newt's ~50-minute whole-workspace hook (#1098).

---

## 5. Anti-regression

**Design constraint: CI is the only real gate.** Hooks are disabled repo-locally on this box (`core.hooksPath=/dev/null`, 2026-08-25) and `--no-verify` is a sanctioned standing exception until #1098. Nothing below depends on a hook firing.

### G1 — compile-time: dead code on the escape path

`fn escape_during_turn(&mut self)` (§2) is private with exactly one caller. Delete the ladder's `Verdict::Escape` arm and it becomes dead code; `cargo clippy -- -D warnings` fails the PR.

*Anti-vacuous twin, stated as a ceiling not a proof:* this guard is void if the fn is made `pub` or acquires a second caller. It is a lint, not a theorem. It is free, so it stays — but it is not the primary guard.

### G2 — per-PR real-PTY, **not `#[ignore]`d**: the primary guard

`newt-tui/src/esc_ladder_pty_test.rs`, following the only existing per-PR PTY precedent: `settings_form_pty_test.rs:53` is not `#[ignore]`d and its module is `#[cfg(all(test, unix, feature = "rich-tui"))]` (`lib.rs:67`), which workspace feature unification turns on, so it rides `cargo test --workspace` (`ci.yml:130`).

Two assertions, and the second is what makes the first mean something:

1. Esc during a running turn, vi NORMAL, nothing claiming → the interrupt label appears.
2. **Esc during a running turn, vi INSERT → the mode indicator changes and the interrupt label does NOT appear.**

*Anti-vacuous twin:* assertion 2 fails against a stub that always escapes, and assertion 1 fails against today's code. A guard that only asserts (1) would pass on "Esc always interrupts", which is the wrong contract and would silently delete rungs 2–6.

Its doc comment names which mocked behaviour it grounds — a real test that grounds nothing is just a slow test. Known hazard: this lane carries a recorded EIO terminal-ownership flake, and a flaky per-PR guard gets `#[ignore]`d within two quarters. That is why G5 lands **before** G2, not after.

### G3 — per-PR unit: registration conformance, both directions

`newt-tui/tests/ladder_conformance.rs`, in `cargo test --workspace` and therefore hook-parity-clean by construction (`just check` at `justfile:164-220` → `.githooks/pre-push:159`).

**(a) Claiming direction:** every name in `ESC_LADDER.claimants()` is answered by `MountedEditor::claim_set()` across the claim-set powerset. Adding a rung to the TOML without an accessor fails the PR.

**(b) Context direction — the two-sided ratchet.** `newt-tui/tests/input_context_ratchet.rs`, modelled on `newt-core/tests/markup_sprawl_ratchet.rs:1-45` (native scan, `#[cfg(test)]` skipped by brace depth, never `grep`, which produced false negatives during markup baselining). Verified baseline: **12 production `event::read()` call sites across 9 files** — `splash.rs:383,441`, `config_panel.rs:1001`, `lean_input.rs:336`, `presenter.rs:955`, `rich_input.rs:1574`, `interaction_view.rs:184`, `backend_panel.rs:1112`, `transcript_pager.rs:474,565`, `newt-core/src/tty/modal.rs:71,136`.

Three counters, one equality:

```
registered   (loops that construct a Ladder)   may only go UP
unregistered (loops that do not)               may only go DOWN
assert registered + unregistered == total_found
assert total_found >= 12
```

*Anti-vacuous twin:* a one-sided may-only-go-down count fails **open** — a move to `event::poll` + a differently-shaped read, an aliased import, or an async `EventStream` zeroes a file's count and reads as progress. The sum equality plus the `total_found >= 12` floor makes a pattern rename a red PR instead of a green one. Plus the standard positive read assertions: the armed file set is non-empty and every armed path exists.

This ratchet is not garnish. It is the hypothesis discharge for §2's scope note — the harness-level "operator can always get out" claim is exactly `unregistered == 0`.

### G4 — per-PR registry rows (weak, and labelled weak)

`behavior-registry.yml:19-23` fires on `**/*.rs` — pure Python, seconds, no JDK or Lean — and `spec/lint-behavior-map.py` is fail-closed on zero-match, ambiguous, duplicate, or missing refs.

```toml
[[BHV-ESC-001.refs.tla]]        spec = "KeyContexts" invariant = "EveryActiveContextHasAHatch"
[[BHV-ESC-001.refs.rust_tests]] path = "newt-tui/tests/input_context_ratchet.rs" symbol = "…" tier = "unit"
[[BHV-ESC-001.refs.production]] path = "newt-tui/src/cockpit/presenter.rs" symbol = "escape_during_turn"
# lean = "none": the theorem is `Precedence.declining_rung_is_transparent` in
# precedence-ladder v0.1. A symlinked refs.lean resolves outside this repo and
# is SILENTLY SKIPPED (lint-behavior-map.py:369-372) — a green proving nothing.
```

**Limitation, on the record:** `refs.production` resolves via `rust_definition_count` (`lint-behavior-map.py:231-235`), a regex for `fn <symbol>` — **definitions only, never call sites**. Deleting the match arm while leaving the fn defined keeps every row green. Verified. So the registry catches a *rename or deletion of the symbol*, and nothing else. G1 catches the arm deletion; G2 catches the behaviour. Do not write "deleting the escape rung breaks CI" in any PR body — it does not, on its own.

`conformance = "partial"`. Nothing in the repo is `full`.

### G5 — land the `tier` cross-check first (~20 lines of Python)

`tier = "unit"|"integration"` on `rust_tests` refs is **advisory only** today — `grep -n tier spec/lint-behavior-map.py` returns nothing, and `behavior-map.toml:555-562` says enforcement is "the tracked follow-up". Until it lands, a contract can cite a test that runs in **no lane** — which is exactly the state of `rich_input_pty_test.rs`, the vi/editor surface #2006 touches, whose four tests are all `#[ignore]`d and named by no workflow.

Rule: `#[ignore]`d ⇒ `tier = "integration"`; `tier = "unit"` ⇒ not `#[ignore]`d. This is the escape hatch for the escape-hatch guard, and it must be closed before G2 exists.

### UAT tier

`scripts/uat/tui-uat.sh` is **invoked by nothing** — verified, `grep -rl "tui-uat" .github/workflows/ justfile .githooks/` is empty. It landed in #2004 on-demand only; its own header (lines 8-11) asks for weekly + release, and neither half was wired.

New `.github/workflows/tui-uat.yml`:

```yaml
on:
  workflow_call: {}
  workflow_dispatch: {}
  schedule: [{ cron: "30 6 * * 0" }]   # 04:00/05:00/05:30/06:00 Sun are taken
```

Chained into the release gate exactly the way `setup-acceptance` is (`release.yml:41-58`): a `uses:` of the reusable workflow, with `build-binaries` gaining `needs: [tui-uat]`, so no publication path can bypass it.

Scenarios appended to `tui-uat.sh` via its existing `expect <name> <deadline> <ERE>` helper: Esc mid-turn from vi NORMAL (interrupts); Esc mid-turn from vi INSERT (mode change only, no interrupt); Ctrl-C mid-turn with the palette open (interrupts); submit a line and assert the mode indicator still reads NORMAL (#2006).

**Mandatory review item in the same PR:** the script's existing drives press Escape to close the palette before every Enter. Those drives run while a turn may be in flight, so landing rung 7 and wiring the UAT can break each other. Audit every existing scenario for accidental mid-turn interrupts.

### Hook parity

- G1–G5 all ride `cargo test --workspace` or the `lint` job's Python steps — already inside `just check` → `.githooks/pre-push:159`. **No hook edit.**
- `tui-uat.yml` carries a written **`HOOK PARITY EXCEPTION`** block copied from `newt-tui-pty.yml:17-26` (a real-binary/tmux tier is too slow and flaky for a push gate). That is the established written form, not a dodge.
- The crate's `formal.yml` carries **`HOOK PARITY: intentionally NONE`**, verbatim from `formal.yml:6-10`. Adding Lean or TLC to newt's pre-push would contradict a recorded decision and worsen #1098.
- Add `newt-tui/src/vi.rs`, `newt-tui/src/rich_input*.rs`, `newt-tui/assets/esc_ladder.toml` to `newt-tui-pty.yml`'s `paths:` (`:34-45`) **and** name `rich_input_pty_test`, `panel_raw_mode_pty_test`, `interaction_view_pty_test` in its run list — all three currently execute in no lane.
- **Fix the live lie while in the header:** `ci.yml:3-5` claims the hook mirrors CI "via `just shell-check` + `just check` + …". `just shell-check` **does not exist** — the recipe is gone, only the prose at `justfile:429-443` names it, and `pre-push` never calls it.

---

## 6. The build train

One issue per PR. Crate PRs are `C*` in the new repo; newt PRs are `N*`. #2006 sequences before #2005 (ADR §5: rung 7 makes the mode indicator load-bearing, so shipping #2005 first makes the amnesia *more* visible).

| # | Issue | Lands | Proves it red-first | Ratchet / golden / UAT |
|---|---|---|---|---|
| **N0** | #2011 | The ADR onto `main` | n/a — it is the citation target for everything below | — |
| **N1** | #2006 | vi state survives every editor reset. **Four sites, not two:** `reset_after_submit` (`rich_input.rs:1965`), `self.vi = Vi::new()` on idle Ctrl-C (`rich_input.rs:289`), `MountedEditor::new` on `SurfaceRequest::Reload` (`presenter.rs:766-772`), and `MountedEditor::new` per classic read (`rich_input.rs:1548` — where the filed "remounted per prompt/turn" diagnosis is *true*). Carry mode, `jback`/`jfwd`, `last_find`, `pending`, `count`. The real fix moves vi state out of `MountedEditor` into the surface/session. **No crate dependency.** | Three regression tests verified to FAIL on the old path: submit from NORMAL stays NORMAL; the jumplist survives submit; idle Ctrl-C clears the draft **without** changing mode. | — |
| **N2** | tier cross-check (G5) | `#[ignore]`d ⇒ `tier=integration`, `tier=unit` ⇒ not ignored, in `spec/lint-behavior-map.py` | The linter fails on a deliberately mis-tiered fixture row | — |
| **C1** | crate skeleton + `resolve` | `Hatch`/`Rung`/`Ladder`/`ClaimSet`/`Situation`/`Verdict`, `resolve`, `describe`, `collisions`, `claimants`, `from_toml`, `content_id` (feature `cid`). `tests/guard.rs`, `check-leaf-deps.sh`, `verify_release.py`, `justfile`, `.githooks/pre-push`, `ci.yml`. | Exhaustive truth table: `2^5` claim sets × {esc, ctrl-c, other} × {running, idle}. `collisions()` reports the rogue table. | golden vectors created |
| **C2** | Lean layer | `formal/Precedence/Basic.lean` + root module + `[[lean_lib]]` **and** `defaultTargets`; `just gen-vectors` codegen + its diff check; `formal.yml` with the `sorry` grep. | `lake build` exit 0, `sorry` grep clean, `decide` block green including `order_is_load_bearing` | vectors regenerated, diff-checked |
| **C3** | PyO3 + release | `precedence-ladder-py` (abi3-py39), `pyproject.toml`, `release.yml`, CHANGELOG, RELEASING.md, GPG-signed tag, **published** Release. Publish `0.1.0`. | `install-smoke` on 4 platforms; the stdlib-only Python consumer re-derives every vector independently of Rust | vectors are the Python conformance input |
| **N3** | #2005 | `precedence-ladder = "0.1"`; `newt-tui/assets/esc_ladder.toml`; `Vi::claims`/`Editor::claims` (edit-gated)/`MountedEditor::claim_set`; presenter arm + `escape_during_turn`; `esc_ladder_pty_test.rs` (**not** `#[ignore]`d); `spec/tla/KeyContexts.{tla,cfg}`; `BHV-ESC-001..002`. | G2's negative assertion fails against an always-escape stub and G2's positive fails against today's code; TLC red when `Push`'s guard is deleted (mutation table in the PR body) | G3(a) armed; TLA+ added |
| **N4** | mode-hint truthing | `rich_input.rs:395-398` `mode_hint` driven by `Ladder::describe`. Today it returns `&'static str` advertising `^C interrupt` in vi INSERT, vi NORMAL, emacs and nano — including at an idle prompt where Ctrl-C only clears the line (`:280-296`). | A test asserting the advertised affordance equals `describe()` for the same `Situation`; it fails on today's constant strings | — |
| **N5** | #2010 | Consumer-side only. `hard` has exactly one non-test reader — `chat.rs:7706` — consulted *after* `response` already returned, purely to pick `"⊘ stopped"` vs `"⊘ interrupted"`. Nothing aborts, so press 2 is a store nobody reads. Make `hard` abort at the wire checkpoints and swap the spinner label the way `set_interrupt_pending` already does (`spinner.rs:71-86`). | A unit test that press 2 produces an observable state change, not a relabel; red before the fix | UAT `repeated-ctrl-c-escalates` |
| **N6** | convert the first context | `transcript_pager` (2 loops → 1 registered ladder). **This is what makes the crate's second consumer real** and moves the ratchet down for the first time. | Ratchet: `unregistered` 12→10, `registered` 0→1, sum equality holds | G3(b) armed at 12, first decrement |
| **N7** | UAT wiring + PTY lane | `input_context_ratchet.rs` at the verified baseline; `tui-uat.yml` (`workflow_call` + `cron "30 6 * * 0"` + `HOOK PARITY EXCEPTION`); `build-binaries needs:`; three PTY modules added to `newt-tui-pty.yml`'s tests **and** `paths:`; the `just shell-check` correction at `ci.yml:3-5`; audit existing UAT drives for accidental mid-turn Esc. | Ratchet fails when a 13th unregistered loop is added, and when the scan finds fewer than 12 total; one green manual `workflow_dispatch` **before** merge | ratchet armed; UAT wired |
| **C4** | npm/wasm — deferred, not gating | `wasm` feature, `wasm-pack build`, one package | node smoke test resolving the same vectors | — |

---

## 7. Adversarial findings — resolution

Every fatal and serious, with fix or refutation and the evidence.

| # | Finding | Resolution |
|---|---|---|
| F1 | *Ctrl-C is already claimed by `modal.rs` and `interaction_view.rs` during a turn, so the invariant is false of newt today.* | **Partly refuted, partly fixed.** Refuted: I traced it — `prompt_control` (`modal.rs:143-146`) → `PromptLine::Exit` → `interaction_terminal.rs:121` → `HumanQuestionOutcome::ExitRequested` → `permissions.rs:1458` `apply_control(PromptChoice::Exit)`, which **cancels the turn** and returns `OPERATOR_EXIT_REQUESTED` (`tools.rs:2474`) telling the model to stop. Ctrl-C in those frames reaches an escape; the operator is not stranded. Fixed: the finding is still right that "reaches THE interrupt rung" is false. The invariant is now scoped to **one ladder** (§2), and the harness-level claim is discharged by the two-sided context ratchet (§5 G3b) plus the TLA+ context-stack model (§3), which is the *only* place the real hazard lives. |
| F2 | *Hatch-first makes `HatchNeverStranded` a tautology; the greedy/rogue decoy is inert.* | **Fixed by deletion.** `HatchNeverStranded` is cut from TLA+ entirely. In Lean it is `resolve_hatch`, registered `lean = "spec"` (by-construction), never `"proven"`. TLA+ now models what a code change can actually falsify: the `Push` guard, with a `hatchless` decoy that a mutation genuinely enables. |
| F3 | *`greedy_rung_cannot_swallow_hatch` is the same lemma as `hatch_reachable`.* | **Fixed by deletion.** Cut. Non-vacuity moves to the `decide` demo block, where `esc + vi-pending → claimed "vi-pending"` proves the ladder is not "escape always wins," and `order_is_load_bearing` proves it is not a set. |
| F4 | *`wf_is_load_bearing` is FALSE under the shipped `resolve` — `decide` fails, `lake build` red.* | **Refuted by adoption of the other spine.** There is no `wf` hypothesis and no `Claim` witness. Confirmed correct: under hatch-first `resolve`, a rung is unreachable on the reserved branch, so any theorem asserting a rogue rung wins there is false. |
| F5 | *`trait Harness` cannot cross PyO3 or wasm-bindgen — the crate's differentiator is Rust-only.* | **Fixed by deletion.** No `Harness`, no `conformance` module, no `Session`. The consumer-side guard is G2 (a real PTY test in newt) plus G3, which fail on real wiring rather than on a mock the consumer implements. |
| F6 | *Design B's plan ships a knowingly-red test to main; `cargo test --workspace` will not merge it.* | **Fixed by deletion.** No red-test-then-green-later sequence. N5's regression test lands with N5's fix. |
| F7 | *The registry cannot see the regression: `rust_definition_count` counts `fn <symbol>` definitions only.* | **Confirmed and fixed by relabelling + G1/G2.** Verified at `lint-behavior-map.py:231-235`. The registry row is documented in §5 G4 as symbol-existence only. The arm deletion is caught by G1 (dead code + `-D warnings`) and the behaviour by G2. No PR body may claim the registry catches it. |
| S1 | *All liveness uses `WF_vars(Next)`, too weak for a model with Mount/Unmount cycles — every liveness property would report a false counterexample.* | **Fixed by deletion.** No liveness properties. The model is safety-only, and the finding is exactly why. |
| S2 | *"sorry-free" is a human assertion; `lake build` exits 0 on a `sorry` and `lean_decls` matches a sorry-bodied theorem.* | **Fixed.** The crate's `formal.yml` greps for `sorry`. newt's identical gap is filed separately, not fixed here. |
| S3 | *The #2006 TLA+ property cannot fail on any code change — the modeller writes the frame condition by hand and there is no trace validation.* | **Fixed by deletion.** Cut. N1's three regression tests are the only #2006 gate, and they are verified red-first. |
| S4 | *An action `PROPERTY` cannot be cited by a `refs.tla` row.* | **Confirmed and avoided.** Verified: `lint-behavior-map.py:316-321` collects only from `INVARIANT`/`INVARIANTS`. No action properties exist in the plan; #2010 is a Rust test. |
| S5 | *Tiers/`presses` in the crate freeze newt's two-tier policy while claiming the counter stays with the consumer.* | **Fixed.** No `Tier`, no `tier_of`, no `presses` field. `escape_during_turn` keeps the counter at `presenter.rs`, where it is already reset correctly at `:805`/`:880`. |
| S6 | *`describe -> Option<&'static str>` is newt's `mode_hint` signature smuggled into a reusable crate.* | **Fixed.** Per-rung `action: String`; `describe -> Option<&str>` borrowed from the ladder. Consumer owns the vocabulary. |
| S7 | *`Hatch { trigger: String }` permits exactly one reserved trigger.* | **Fixed.** `reserved: BTreeSet<String>`, non-empty by construction via `Hatch::new(action, first, rest)`. |
| S8 | *The `Claim` witness is unbranded — a `Claim` from one `Hatch` can be used with another; `LadderBuilder::build` errors on duplicate names only.* | **Fixed by deletion.** No `Claim`, no builder. Resolution order makes the property unconditional without a witness. |
| S9 | *`Claim` cannot cross FFI, so a Python consumer can only reach the ladder via `from_toml`.* | **Fixed by deletion.** Both `Ladder::new` and `from_toml` are reachable from both bindings; the whole surface is plain values. |
| S10 | *Edit mode is missing from every claim set — `vi-insert` claims Esc permanently under emacs/nano.* | **Confirmed and fixed.** Verified: `Editor` holds a `Vi` in all modes (`rich_input.rs:244`) and `Editor::input` gates on `self.edit == Edit::Vi` (`:255`, `:355`). §2's `Editor::claims` carries the gate. `cx_pending` (emacs `C-x C-c` → Eof, `:263-268`) needs no rung: today the presenter's Ctrl-C arm already preempts it during a turn, and the ladder preserves that exactly. |
| S11 | *#2006 has four editor-reset sites, not two; the real fix moves vi state out of `MountedEditor`.* | **Confirmed and fixed.** N1's scope is all four, including the classic path at `rich_input.rs:1548` where the filed "remounted per prompt/turn" diagnosis is true. |
| S12 | *The watcher adapter does not fit `watch_for_interrupt_fd`'s control flow (grace poll + nested read before classification).* | **Fixed by descoping.** The watcher is not converted in this train. Its single claimant and byte-level grace window make it a separate issue. |
| S13 | *"Two adapters or it's an interface with one implementation."* | **Fixed by N6.** The second consumer is `transcript_pager`, not the watcher — a genuinely different ladder, and it moves the ratchet down for the first time. If N6 is refused, see §8 Q2. |
| S14 | *`Claimed(name)` is stringly typed with no dispatch path; a typo yields a silent no-op.* | **Fixed by scope.** In the cockpit `Claimed` does not dispatch — the editor does, unchanged. The only consumer of the name is `describe` and the conformance test, and G3(a) catches a name with no accessor. Any future consumer that *does* dispatch on the name must pair it with an exhaustive match; noted in the crate README. |
| S15 | *Rung order is unobservable in newt — the crate's live output is one bit.* | **Confirmed, not disputed.** §1 states it plainly. The name earns its place through `describe` and the conformance test, not through dispatch. Ordering theorems about *newt's behaviour* are cut; `first_match_inv` remains as the definition's inversion lemma. This is also §8 Q2's substance. |
| S16 | *Golden vectors "read by three consumers" is unimplementable — no JSON reader in a Mathlib-free Lean, none in a `.cfg`.* | **Confirmed and fixed.** `just gen-vectors` regenerates the Lean `decide` block from the JSON with a CI diff check. The `.cfg` is not a consumer; the TLA+ model is about contexts, not vectors. |
| S17 | *The `event::read()` ratchet fails open on pattern drift, and permanently licenses `presenter.rs:955`.* | **Fixed.** Two-sided ratchet with a sum equality and a `total_found >= 12` floor (§5 G3b). `presenter.rs:955` moves from `unregistered` to `registered` in N3, so it is not licensed — it is converted. |
| S18 | *The conformance test is one-directional; it never checks that every escape-consuming path is registered.* | **Fixed.** That is exactly G3(b)'s `unregistered` counter. |
| S19 | *The crate ships no temporal spec; a second consumer gets Lean about one value and nothing about sequencing.* | **Refuted with the house rule.** A pure function has no temporal behaviour. TLA+ goes where the sequencing is — newt's context stack. Shipping a temporal spec with the crate would be the ceremonial artifact `spec/tla/README.md:18-22` forbids. |
| S20 | *newt's per-PR gate contains zero formal coverage under a crate-repo split.* | **Fixed by the split direction.** TLA+ is in newt (`tla = "checked"`, real, cited by `BHV-ESC-001`); Lean is in the crate (`lean = "none"`, crate named in prose). This is the reverse of what the inputs proposed and it puts the formal layer inside the gate the operator asked about. |
| S21 | *The single per-PR PTY guard rides a lane with a recorded EIO flake; it will be `#[ignore]`d within two quarters.* | **Fixed by ordering.** G5 (the tier cross-check, N2) lands **before** G2 (N3), so an `#[ignore]` on a `tier = "unit"` ref becomes a red PR. |
| S22 | *`content-addressable` unconditional drags blake3 + multiformats into wasm.* | **Fixed.** `cid` is an off-by-default feature. |
| S23 | *Design B's derived `reserved` silently repoints when a rung claims the current one.* | **Fixed by deletion.** `reserved` is an explicit set, never derived. |
| S24 | *`work_running: bool` encodes one flat work unit and will be passed `true` unconditionally under nesting.* | **Fixed as far as it can be.** The ceiling is a doc comment on the field (§2). A depth counter belongs in the consumer, not in `Situation`. |
| S25 | *Headless is sold as the justifying consumer and is not one — `interruptible` requires both TTYs (`chat.rs:7266-7268`).* | **Confirmed, removed from the pitch.** The crate's purity is what puts it in the fully-mocked per-PR tier; that is the benefit, not a headless consumer. |

Minor findings adopted without discussion: `pub(crate)` accessors rather than `pub`; the `KeyEventKind::Press` guard preserved; the dead `modal` rung omitted; the borrowed-`Verdict` FFI clone noted; the UAT's Escape-driven command sends flagged for audit in N7.

---

## 8. Open questions for the operator

**Q1 — Repo owner: `Gilamonster-Foundation` or `hartsock`?**
`content-addressable` lives under `hartsock/` and registers its PyPI trusted publisher with `Owner: hartsock`. That registration is owner-pinned and cannot drift afterwards. GMF gets `vars.GMF_LINUX_RUNNER` and any org-level secrets.
**Recommendation: `Gilamonster-Foundation`,** for the runners, and because newt/wyvern/gilamonster are the consumers. Decide before C3 creates the trusted-publisher entry.

**Q2 — Does the crate earn its keep, given that its runtime output in newt is one bit?**
This is the honest version of "does this need to exist at all." My answer: yes, but conditionally. The value is not the bit — it is (a) `describe` making the affordance and the behaviour share one table, which kills a live defect; (b) the claimant names giving the registration ratchet something to check, which is the mechanism that ends "one ladder, not six"; (c) a second harness getting the policy without newt's release train.
**Recommendation: build it, but gate the 0.1.0 *publish* on N6 landing.** If converting `transcript_pager` is refused, the ladder is a newt module with one consumer and should stay in-tree at `publish = false` — an interface with one implementation is not worth a second release train, two sets of secrets, and a second GPG ritual.

**Q3 — npm now or later?**
**Recommendation: later, C4, and only when a JS consumer asks.** The recurring cost is not the CI job; it is a third published version number in lockstep forever.

**Q4 — Should Ctrl-D become a hatch trigger (#2010 option 3)?**
`0x04` is already `TurnKey::HalfPageDown` in the vi nav decoder (`lib.rs:10786`), and `Ctrl-D` on an empty buffer is `Step::Eof` (`rich_input.rs:296-300`).
**Recommendation: no.** `collisions()` will report it at table load. If you want it anyway, it is a table row and a separate issue, not part of this train.

**Q5 — N1's scope: four sites, which needs moving vi state out of `MountedEditor`. Split it?**
**Recommendation: keep it whole.** Fixing two of four sites leaves classic-path amnesia intact at exactly the moment rung 7 makes the mode indicator load-bearing, which is worse than not shipping #2005. If the diff is too large for one review, split *by site* with the refactor first — never by "cockpit now, classic later."

**Q6 — Should an idle Esc or Ctrl-C ever escape?**
**Recommendation: no.** `resolve` returns `Unbound` for a reserved trigger while idle, preserving today's draft-clear (`rich_input.rs:280-296`). Changing it is a behaviour change the ADR does not authorize.

**Q7 — abi3 forecloses rich `#[pyclass]` options. Is a stateful Python API planned?**
**Recommendation: take abi3.** The current surface is plain values, and dropping Windows (newt's non-abi3 fate) is disqualifying for a crate meant to be pulled into other people's harnesses. The ceiling goes in the crate's frozen-policy comment.

**Q8 — Two pre-existing gaps this train uncovers but does not fix: newt's lakefile has no `sorry` gate, and `formal.yml` + `behavior-formal.yml` both run `lake build` on `formal/**` (two elan installs per change).**
**Recommendation: file both, fix neither here.** One-issue-one-PR, and neither blocks this work.