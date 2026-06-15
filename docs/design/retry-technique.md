# The `retry` technique — verify-gated revert-retry, scoped honestly

**Status:** design note (pre-build) · **Composes with:** [`technique-library.md`](technique-library.md) (the technique abstraction + spec sheet), [`model-family-profiles.md`](model-family-profiles.md) (profile = composition) · **Presupposes:** [`verify_gate`](technique-library.md#verify_gate) (R2 — the gate that produces the revert set) · **Grounded by:** [`../findings/2026-06-15-retry-and-the-honest-gate.md`](../findings/2026-06-15-retry-and-the-honest-gate.md) (why this technique's worth is *bounded by its gate*) · **Knob:** `retry.max_retries` (already in `ProfileConfig`)

## Abstract

`retry` is the **destructive, loop-altering** half of the verify-gate pair. Where
`verify_gate` (shipped warn-only in #368) *observes* — it runs the gate after a
turn and prints which produced files import outside the FFI surface — `retry`
*acts*: it reverts exactly the gate's revert set and re-prompts the model with the
fabrication named, up to a cap, then gives up cleanly. This note pins the contract
before any code: what gets reverted and to what prior state, the corrective
re-prompt, the cap + give-up behavior, the permission-gate interaction, and the
honest caveat carried over from the retry-Goodhart finding. The two techniques
before it (`knowledge_base`, `verify_gate`) were additive and safe — text in,
warning out. `retry` changes control flow and mutates the model's files, so it
earns its own PR and this note first.

## Why a separate note (and a separate PR)

`knowledge_base` injects a system-prompt block; `verify_gate` prints a warning.
Neither touches the model's output or the loop's control flow. `retry`:

1. **deletes/restores files the model wrote** (destructive), and
2. **re-enters inference inside a turn** (loop-altering — a turn can now run the
   model N+1 times).

Both properties make it the one technique where a sloppy implementation can lose a
user's work or spin the loop. The mechanism was already *measured* by an external
shell instrument (`rig_retry_loop.sh`); bringing it **in-loop** is the new work,
and the in-loop revert is strictly harder than the rig's because the rig only ever
created files that did not pre-exist.

## The revert contract (the load-bearing decision)

The rig instrument reverted with `rm -f $bad_file` — correct *there* because every
corpus file was freshly generated, so "revert" = "make it not exist." In the real
loop a flagged file may be an **edit of a pre-existing file**, so `rm` would
destroy the user's prior content. The contract must restore the *pre-turn* state of
exactly the revert-set files — three cases:

| pre-turn state of file | revert action |
|---|---|
| did not exist | delete it (rig's case) |
| existed, unchanged this turn | never in the revert set if untouched; if touched, restore bytes |
| existed, edited this turn | restore the captured pre-turn bytes |

### How pre-turn state is captured — a turn-scoped write ledger

The loop has **no pre-turn snapshot today**: `write_file`/`edit_file` call
`std::fs::write` directly (`newt-core/src/agentic/tools.rs`). We add a
**copy-on-first-write ledger** scoped to the turn:

- The first time a path is written *during a turn*, record its prior content
  (`Some(bytes)`) or `None` if it did not exist. Subsequent writes to the same path
  in the same turn do **not** overwrite the ledger entry (the *pre-turn* state is
  what we restore to, not the last intermediate write).
- On a gate revert, for each path in `report.revert_set()`: look up the ledger and
  restore (`fs::write` the bytes back, or `fs::remove_file` if the entry is `None`).
- The ledger is cleared at turn end (accept or give-up). It is **turn-local
  state**, never persisted.

This is git-independent (newt runs on non-git workspaces and the gate must not
presuppose a clean tree) and exact (byte-restore, not a diff). *Alternative
considered:* `git stash`/`git checkout -- <file>`. Rejected as the default — it
requires a git repo, a clean-enough index, and would revert *unstaged user edits*
outside the revert set. A git fast-path may be added later as an optimization, but
the ledger is the contract.

### Scope of the revert set

Only files the gate flags (`!FileVerdict::is_clean()`). A clean file the model wrote
the same turn is **kept** — `revert_set()` already excludes it. We never revert a
file the model did not write this turn (it won't be in the ledger; assert and skip
with a warning if a gate path is somehow absent from the ledger — that's a bug, not
a silent `rm`).

## The corrective re-prompt

After reverting, the loop re-prompts with the fabrication named, so the retry is
*grounded*, not a blind re-roll (the rig's grounded re-prompt is why it moved
1/5 → 2/3, not back to 1/5). Template:

```
The file `{path}` imported `{module}` (line {line}), which this project does
not expose. The authoritative import surface is:

{surface_block}

Rewrite `{path}` using only modules from that surface. Do not import `{module}`.
```

- `{surface_block}` is R1's `FfiManifest::render_block()` — the **same** authority
  `knowledge_base` injects. `retry` therefore *presupposes the surface is
  available*; it composes naturally with `knowledge_base` (inject up front) but does
  not require it (it can attach the surface to the corrective message alone).
- One message lists **all** reverted files and their fabrications, not one message
  per file — a single corrective turn, not a storm.

## Cap + give-up (no infinite loops)

- The cap is `profile.retry_knobs().max_retries` (default **2**, already in
  `RetryKnobs`). A turn runs at most `1 + max_retries` model calls.
- After each retry the gate runs again. **Accept** as soon as `report.accept()` is
  true.
- On exhausting the cap with the gate still failing, **give up honestly**: leave the
  files reverted (do *not* re-apply the last fabrication), and emit a banner —
  `⚠ retry: gave up after {max_retries} retries; {n} file(s) left reverted — the
  model could not ground [{modules}]`. This mirrors the existing cap-exit honesty
  (#73's honest banner): a labelled absence beats a fabricated presence. The turn's
  `end_reason` records `retry_exhausted` for the forensics rig.
- **Zero-progress guard:** if a retry produces the *same* fabrication set as the
  prior attempt, count it but do not loop faster than the cap — the cap is the only
  termination authority; we do not add a second heuristic that could mask a
  regression.

## Permission-gate interaction

`retry`'s revert is a `newt`-initiated `fs::write`/`remove_file`, not a model tool
call — so it bypasses the per-tool `PermissionGate` (the human already consented to
the writes that produced these files; the revert is *undoing newt's own action*,
strictly less authority than the original write). Constraints:

- Revert touches **only** paths already in the turn's write ledger — i.e. paths the
  permission layer **already allowed** this turn. It can never reach a path the
  model was denied.
- The corrective retry re-enters the **same** turn with the **same** caveats /
  `PermissionGate`; the model's rewrite is gated exactly as the first attempt was. A
  retry grants no new authority.
- Under a read-only / `Only`-scoped caveat where no write was permitted, the ledger
  is empty, the gate has nothing to flag, and `retry` is inert — consistent with
  `verify_gate` being a no-op there.

## Module placement (avoid the `retry.rs` collision)

`newt-core/src/retry.rs` **already exists** and is the unrelated HTTP backoff module
(`with_backoff`, `RetryPolicy`). The technique must **not** live there. It lands in
`verify_gate.rs` (it is the gate's action arm) or a new `revert_retry.rs`; the
config technique name stays `"retry"` (user-facing), but the code symbol is
`revert_retry` / `apply_revert_retry` to keep the backoff module unambiguous.

## Spec sheet

| field | value |
|---|---|
| **buys** | turns a fabricating turn into a grounded one within the cap — measured 1/5 → 2/3 grounded, 1/3 honest no-output (post-hardening), `rig_retry_loop.sh` |
| **failure mode** | **gate-gaming under retry** — once the gate is a *control signal* not a *measurement*, the model drifts into any gate blind spot (the Goodhart finding). Bounded by the gate's adversarial completeness. |
| **caveat / context** | worth = `verify_gate`'s worth. Acceptable when the gate **is** the spec (leaf-exact `SurfaceMatch::Exact`); dishonest when sold as proof of correctness against a coarse `Prefix` surface. |
| **knobs** | `retry.max_retries: u32` (default 2). Inherits `verify_gate.surface_match`. |
| **presupposes** | (1) `verify_gate` enabled — `retry` is its action arm; (2) the import surface available for the corrective re-prompt (`knowledge_base` or attached inline); (3) an adversarially-honest gate, or an explicit "gate-as-spec" context. |
| **composes with** | `knowledge_base` (front-load the surface) + `verify_gate` (produce the revert set). Ordering: `knowledge_base` → turn → `verify_gate` → `retry`. |
| **measured by** | `docs/testing/results/scripts/rig_retry_loop.sh` + the retry-Goodhart finding's hardened re-measurement. |

## Build plan (the follow-up PR)

1. **Write ledger** — turn-scoped copy-on-first-write recorded at the
   `write_file`/`edit_file` seam in `agentic/tools.rs`; threaded into the turn
   context. Unit-tested in isolation (records prior bytes / `None`; restores all
   three cases).
2. **`apply_revert_retry`** in `verify_gate.rs` — given a `GateReport`, the ledger, a
   re-prompt sink, and `max_retries`: revert → corrective prompt → re-run → re-gate →
   accept/cap. Pure-ish, mock the re-run as a closure for determinism.
3. **Wire into `run_chat`** — gated on `active_profile.enables("retry")`, in the
   post-turn arm next to the existing `verify_gate_summary` call. `verify_gate`
   alone stays warn-only; `retry` upgrades it to revert+re-prompt.
4. **Tests** — deterministic loop test with a mock re-run (fabricate→ground→accept;
   fabricate×∞→give up at cap with the honest banner + `retry_exhausted`); ledger
   restore for create/edit/delete cases; ≥80% floor.
5. **Live smoke** — nemotron on the PyO3 corpus: a fabricating turn reverts and
   grounds within the cap; confirm the honest banner on a forced-fail.

## Implementation status

Built in three increments rather than one PR, to keep the destructive/loop-altering
work isolated and reviewable:

- **Increment 1 — the pure mechanism (#371, merged).** `WriteLedger`,
  `apply_revert_retry` (revert → corrective prompt → re-gate → accept/cap, re-run
  behind a mockable `RetryRerun`), `corrective_prompt`, `RetrySurface`, in
  `verify_gate.rs`. Fully unit-tested, no loop change.
- **Increment 2a — capture + live revert (this PR).** Wires the destructive arm into
  `run_chat`: a per-turn `WriteLedger` is lent to the loop (`ChatCtx.write_ledger`,
  `Some` only under a `retry` profile); the loop records each `write_file`/`edit_file`
  target's pre-write bytes at the dispatch seam (`ledger_note_write`, just before
  `execute_tool`); after the turn the TUI gates and calls
  [`revert_only`](../../newt-core/src/verify_gate.rs) (= `apply_revert_retry` with
  `max_retries = 0`) to revert the flagged set, with an `↩ retry: reverted …` banner.
  - **Per-write ledger, NOT a pre-turn snapshot.** An earlier draft populated the
    ledger from a whole-workspace pre-turn `.py` snapshot and treated "absent from
    snapshot ⇒ delete". An adversarial review proved that **unsafe**: a snapshot
    cannot tell a file *newt wrote* from one that merely *appeared* (build output,
    `run_command` codegen, files reached through a symlinked dir), so the delete rule
    destroyed files newt never authored — including data **outside** the workspace via
    symlinks. The per-write ledger records only newt's own writes, so revert restores/
    deletes exactly those and **skips anything untracked** — the safety property
    `apply_revert_retry` already had. Defence-in-depth added alongside: `collect_py_files`
    no longer follows symlinks and skips vendored/build dirs (`.venv`, `site-packages`,
    `node_modules`, `target`, …); `apply_revert_retry` refuses any path that resolves
    outside the canonicalized workspace and reports only files actually reverted.
- **Increment 2b — the re-prompt loop (next).** Upgrade revert-only to revert+re-run
  so a fabricating turn becomes a grounded one within the cap. Either drive
  `apply_revert_retry` live (extract a turn-runner so its `RetryRerun` re-invokes a
  turn) or re-prompt via a `run_chat` task-queue + cap counter; the honest give-up
  banner + `retry_exhausted` land here.

## Known gaps (deferred, from the 2a adversarial review)

These were confirmed at **low/nit** severity and are deferred deliberately — none is
data-loss (the two data-loss blockers and the two mediums were fixed in 2a):

- **Revert runs only on a successful turn.** A turn that *errors* after writing a
  fabrication leaves it on disk (the `Ok` arm reverts; the `Err` arm drops the
  ledger). Folds into **2b**, which restructures the post-turn path for the re-prompt
  loop anyway.
- **Full `.py` walk + parse each turn under `retry`.** Mitigated by the symlink-skip
  + `SKIP_DIRS` exclusions; a large monorepo could still want an incremental gate.
  Pre-existing in `verify_gate` (#368); a shared follow-up.
- `retry` intentionally **supersedes** the `verify_gate` warning (it acts instead of
  warning) — by design, not a regression.

## Out of scope

- A git fast-path for revert (ledger is the contract; git is a later optimization).
- Reverting non-`.py` languages — the gate is Python-surface today; the ledger is
  language-agnostic but the gate that drives it is not yet.
- Per-family `max_retries` auto-tuning (that's the R5 self-tuning technique).

Toward #79. Refs #73, #74, #360, #368.
