# D2b handover — #1895 (the spinner cutover)

> **Temporary artifact.** Delete this file before the PR — it is scaffolding for
> a context reset, not documentation the repo should carry.

**Branch:** `feat/d2b-spinner-cutover` @ `aa580f6c` (pushed to origin).
**Worktree:** `~/workspaces/.worktrees/d2b`.
**Rebased onto:** `816cf28c`. **`origin/main` has since moved to `17a89c91` —
rebase before resuming.**
Tree clean, no PR, nothing uncommitted.

---

## What commits 1 and 2 did

### `5957aa50` — `refactor(progress): a Frame carries measurable facts, not a rendering`

`Frame` changed from `{ label: String }` to:

```rust
pub struct Frame {
    pub label: String,      // what is happening
    pub elapsed: Duration,  // never a rendered "3.2s"
    pub units: u64,         // progress observed (the `· N chars` tail)
}
```

Two omissions, both load-bearing:

- **No pre-formatted string.** Fitting text presumes a column count, and a
  producer cannot fit a line for a surface it does not know. The web surface has
  no columns; a terminal that resizes mid-turn cannot re-fit a string it was
  handed. The next consumer would have to re-parse a rendering to recover what
  it meant. `markup::spans` settled the same shape one layer up: it emits
  emphasis ROLES and deliberately not styles.
- **No glyph index.** The glyph cycle *is* the animation, and transient
  animation stays view state. A producer emitting which glyph puts the animation
  in the producer, forces its cadence on every consumer, and forecloses a view
  that would rather show a bar or a percentage. **The view cycles its own glyph
  off its own clock.**

`advanced` (did units move since the last frame) was deliberately omitted —
semantically legitimate, additive later, but nothing needs it: the spinner's
purpose is to stay alive precisely when the covered work is stuck.

Guarded by `a_frame_carries_no_rendering_and_no_glyph` plus an anti-vacuous
twin. RED verified by adding `pub glyph: usize` **with the constructor updated
so the mutation compiles** — the first attempt did not build, and a mutation
that fails to build is neither red nor green.

### `aa580f6c` — `refactor(tty): extract EphemeralRow — row ownership as its own type`

Row ownership extracted from `SpinnerState` into `newt-core/src/tty/row.rs`.
Behaviour unchanged, nothing deleted. The spinner **still owns the row**, now
via `Arc<EphemeralRow>`, which is the registered `Ephemeral`.

All 11 spinner tests and 65 `tty` tests green; fmt clean; `clippy -p newt-core
--all-targets -- -D warnings` exit 0.

---

## Where commit 3 stops

**Before any edit.** No cutover code was written. The previous agent read the
F0b/F0d constraints to avoid discovering them mid-cutover, reported, and
stopped.

### Verified findings from that read — do not redo

- **No ratchet category counts spinner or row-ownership sites.** `spinner.rs`
  appears in **no** baseline and **no** destination. The cutover moves no
  existing ratchet number and updates no existing probe.
- **F0d's mechanism**: `DETECTOR_PROBES: &[(category, must-fire,
  must-stay-quiet)]` at `newt-core/tests/markup_sprawl_ratchet.rs:2421`. A
  category with no probe fails the build; stale probes (category gone) also
  fail.
- **`Category` fields**: `name`, `count`, `destinations`, `baseline`,
  `rationale`. `destinations` is an **exact ceiling checked in both
  directions** — too high means a second implementation growing inside the
  destination, too low means the convergence point itself went away and every
  other row is converging on nothing.

### The next edit that was going to happen

Commit 3, in this order:

1. The renderer acquires the `EphemeralRow`.
2. The spinner publishes `Frame`s instead of painting.
3. `format_spinner` composition + `fit_line` move to the renderer, which cycles
   its **own** glyph index from its **own** clock.
4. **Delete** the spinner's direct implementation.
5. Re-add the deletion gate from `scratchpad/gate.rs.txt` (see below).

Production callers are few: `ToolSpinner` (`newt-core/src/agentic/tools/live_output.rs`),
`newt-core/src/agentic/tools.rs:3970`, and two `with_spinner` sites
(`newt-core/src/agentic/mod.rs:2606,2778`).

Then commit 4 (ratchet) — see *Remaining work*.

---

## The ordering constraint, in plain terms

**The renderer takes the row, and only then does the spinner stop painting.**

This is **forced, not stylistic**. `Terminal::lease_with_caps` →
`Inner::line_held` is exclusive with a **50 ms wait-timeout**
(`newt-core/src/tty/arbiter.rs:300,323`). A renderer acquiring a lease while the
spinner still holds one does **not** briefly coexist — it times out and gets
`None`. There is no overlap window to work in.

So ownership has to move as **a type, in a single edit**, rather than being
negotiated between two live objects. And publishing from inside a
lease-holding spinner would put **two writers on one row** — the defect `tty`
exists to prevent, and the reason D2a explicitly deferred this cutover.

---

## The lease-handover question — the open risk

**UNRESOLVED. This is the thing to decide first.**

Is the handover a **straight move** of the row-ownership machinery, or does it
need a **change to the gate semantics**?

Evidence *suggesting* a straight move: the commit-2 extraction needed **no**
change to gate semantics, and the race tests passed unchanged apart from their
address. But that was extraction with ownership **staying put**. Commit 3 is
where the lease actually changes hands, and the previous agent never got far
enough to know.

**If it needs the gate semantics changed rather than moved, STOP AND REPORT.**
That is a design finding, and its failure mode is a **user-visible hang, not a
red test**.

### What must survive verbatim

Full text is in `newt-core/src/tty/row.rs` module docs. Every rule was bought by
an earlier fix:

- lock order is always **`paint_gate` → stdout**, in every holder;
- **`Ephemeral::erase` MUST take the gate** — otherwise a tick passes
  `LineLease::paint`'s `suspended()` check while the flag is still clear, loses
  the CPU, and flushes its frame *after* the erase, repainting the row a
  question is about to occupy. **That is the invisible-prompt hang** the arbiter
  exists to end, arriving through the one door it did not close;
- **`finish` is idempotent** — `Drop` and explicit teardown are the same
  operation and cannot double-erase a row someone else has taken;
- **a paint after `finish` must not land** (#1727 spinner → live-output
  hand-off);
- **`finish(before_erase)` runs its closure UNDER the gate**, because the
  spinner's trailing-detail flush must happen between "mark finished" and
  "erase";
- **`paint_gate` is `pub(super)` deliberately**, so the race tests can stand in
  for an in-flight paint by holding it. That is not a leak — those tests are why
  the gate exists and must be able to address it.

---

## Guard states

| guard | state |
|---|---|
| `nothing_may_add_a_conversion_from_a_frame_to_a_commit` | **GREEN, structural.** Ranked above the two behavioural tests because those would keep passing against a conversion nobody calls yet. **Do not weaken it to make the cutover convenient.** |
| `a_high_rate_producer_grows_neither_sink` | **GREEN**, but with no live producer yet. Commit 3 must keep it green with a real 10 Hz spinner behind it — that is exactly the traffic it models (10k frames + 10k non-advancing snapshots → one commit, one retained frame, the **newest**). |
| structural silence — `the_collector_owns_no_writer` | **GREEN** (merged in D2a). The collector owns no writer, so silence is a property of the type, not of reachability (cf. #1866). |
| `the_spinner_owns_no_ephemeral_row` | **NOT in the tree.** Saved at `/tmp/claude-1000/-home-hartsock-workspaces-newt-agent/f21e8de0-b1b1-41c0-a396-68cdc4b58bf2/scratchpad/gate.rs.txt` (2107 bytes). Paste into `spinner.rs`'s test module in commit 3, where it goes green. |

### Read before touching the deletion gate

It originally named `LineLease` and **went green on commit 2** — not because the
spinner had stopped owning the row, but because it had stopped *naming* the old
type. It still owned the row, through `EphemeralRow`. A deletion gate that
passes before the deletion is the vacuous-green failure.

Its needle is now **`EphemeralRow` — ownership, not spelling.** Do not relax it
back.

---

## Remaining work

### Commit 3 — the cutover
As listed above. **Deleting the direct implementation is the deliverable, not a
bonus.** A cutover leaving both paths live is the "old plus new" outcome F0
forbids, and F0 is now closed, so that gate is real rather than aspirational.

### Commit 4 — the ratchet
Declare the convergence point, mirroring the existing
`raw-mode owners outside RawModeGuard` category exactly (same shape: one owner
type, duplicates driven to zero):

```
name:         "ephemeral-row owners outside EphemeralRow"
destinations: &[("newt-core/src/tty/row.rs", N)]
baseline:     &[ ...surviving duplicates... ]
```

plus a `DETECTOR_PROBES` entry with a fires/quiet pair. This is what makes the
deletion **durable** rather than merely done — a second row owner appearing
later fails the build instead of being caught by a reviewer.

### Gates
fmt; clippy `-D warnings` both workspaces, with
`--features newt-interaction/schema`, and with `rich-tui` ON and OFF;
`-p newt-core` / `-p newt-tui` suites; `--no-default-features` build; A0–D3
suites unmoved; ratchets down or unchanged, never up.

**Ask the operator before taking a whole-workspace gate.** The bottleneck is the
disk, not CPU, and load average misleads because it is iowait.

### PR
Against main, `Fixes #1895`, `risk:high`. **Merge nothing.** No session
trailers. Push over SSH. Delete this file first.

---

## Still open from earlier work in this stream

- **#1871** — `dock_registry::concurrent_approvals_do_not_lost_update` is a
  **real test defect, not a load artifact**: 4/9 failures on a 95%-idle box. It
  passed once on a clean gate, which says nothing about the rate. The assertion
  is wrong for the property it names — a writer told "try again" has been
  correctly *serialized*, not lost.
- **#1879** — three `agentic::tools` tests fail under `-p newt-core --lib` and
  pass under `--workspace`, because they need the `newt-net-guard` binary built
  beside the test executable.
