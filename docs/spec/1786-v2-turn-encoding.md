# Spec: the v2 canonical turn encoding (#1786)

**Status:** DRAFT r5. Review history: r1 (33 findings, 29 confirmed), r2
(19/16), then the derivation edge moved from the turn to the **compaction
seal** — which dissolves D5 rather than answering it — and r4's encoding of
that move was killed by a third review aimed at the new mechanism (39
findings, five independent fatal flaws). r5 keeps the insight and replaces
the encoding; §5b states what failed at each rule. Phases A (#1799) and B
(#1801) are merged and unaffected.
**Depends on:** PR #1792 (`load_verified` — the chain must be *read* in
production before hashing anything new into it is meaningful).
**Retires:** both remaining conformance-suite violations, `KNOWN_VIOLATIONS`
2 → 0 — with both law tests **strengthened first** (§10.1).
**Unlocks:** #1787 (hallucination attribution).
**Absorbs from #1794:** the per-writer tip witness.

---

## 1. Why one bump

An encoding bump is the expensive kind of change: every v1 row must verify
under v1 rules forever (`content_hash()` dispatches on the row's recorded
`encoding_version` and refuses versions it does not know). Three changes
want the same epoch — provenance sources, `phantom_reaches` into the hash,
per-writer tip witnesses. Separately: three migration epochs. Together: one
v2 arm, one migration, one set of pinned vectors.

## 2. Two digests, two jobs

**The chain hash** (exists today): BLAKE3 over the canonical encoding,
including `conversation_id`, `writer_fingerprint`, `prev_hash`, `seq`,
`ts_claim`. It pins a turn **at its position in its chain** — transitively
the whole prefix. Right for tamper evidence; wrong for citation:
`import_one_record` re-chains rows (fresh seqs, new prev_hashes,
substituted fingerprint — all hashed), so a citation by chain hash orphans
on every import, permanently.

**The content id** (NEW):

```
turn_content_id = BLAKE3("newt-turn-content:v1"
                          ∥ len-prefixed STORED BYTES of:
                            user, assistant, events, phantom_reaches, sources)
```

* **Stored bytes, never re-serialization** (r2 finding): the preimage is
  the column bytes exactly as written at append. Recomputing from
  materialized structs would silently change ids whenever a serde type
  gains an additive field (documented-supported growth) — a false-orphan
  refusal across build versions on an untampered store. Consequence for
  implementation: `TurnRow` and the verification SELECT widen to carry the
  raw `phantom_reaches` and `sources` strings (today `TurnRow` deliberately
  omits reaches; that premise inverts here, on purpose).
* **`sources` is IN the preimage** (r2 finding). r2 excluded it and the
  review constructed, on the normal path, two byte-identical fallback
  summaries (the static fallback text is a pure function of the removed
  count) where the second's sources contained *its own id* — and a #1787
  walk over id-addressed edges self-loops or picks an arbitrary ancestor,
  because an id that excludes sources does not determine its outgoing
  edges. With sources inside the preimage: distinct derivations get
  distinct ids even with identical text, an id *does* determine its
  provenance edges, and self- or mutual-citation requires a BLAKE3 fixed
  point — computationally unconstructible. Witnessed rows still collide on
  identical content, which is harmless: they have no outgoing edges, and a
  citation of "what was said" is satisfied by any row that said it.
* Excludes every chain-position field, so it survives re-chaining, import,
  and export — the portable identity #1787 needs. Derived, never stored:
  computable from any row's columns at materialization.
* v1 rows: computed over the same stored bytes, including the backfilled
  `'[]'` columns. Those bytes are never *interpreted* on v1 rows (§3.2),
  but they are identity-bearing here — an SQL edit to a v1 row's dead
  columns breaks citations of it **loudly** (orphan), which beats silently.

### 2.1 The v2 canonical encoding

```
"newt-turn:v2"
  ∥ len-prefixed: conversation_id, writer_fingerprint, prev_hash,
                  user, assistant, events,
                  phantom_reaches,          ← NEW in the hash
                  sources                   ← NEW column, in the hash
  ∥ seq (i64 LE)
  ∥ presence-byte + i64 LE: tokens_in, tokens_out
  ∥ ts_claim (i64 LE)
```

Hash-the-stored-bytes rule applies to BOTH digests: hashed bytes are the
stored column bytes, written once at append, never re-serialized.

## 3. `sources` — the provenance edge

New column `turns.sources TEXT NOT NULL DEFAULT '[]'`. Empty ⇔ witnessed;
non-empty ⇔ derived. Hashed in v2 (chain) and identity-bearing (content id).

**Canonical bytes, pinned** (r2 finding — the column is hashed, so its
bytes need one producer-deterministic form): a compact JSON array (no
whitespace) of lowercase-hex content ids, **sorted lexicographically,
duplicates forbidden**. A v2 row whose sources bytes are not in canonical
form, or contain duplicates, is a chain violation at verify (well-formed
evidence or none).

**D1 resolved: content ids** (chain hash rejected — position-pinning,
orphans on import; coordinate hints rejected — no consumer, frozen cache).

**Verification rule:** for every v2 row with non-empty sources, every cited
id must equal the content id of some turn in the same conversation. Orphan
⇒ violation naming the citing row and the missing id.

**No ordering rule.** Cross-writer seqs are not a causal order (per-writer
Lamport ticks; the receive rule runs only at clock-row creation), so r1's
seq-ordering check produced only false positives — permanently bricking
legitimate conversations. Dropped. And honestly (r2 finding): existence is
checked **at verify time, not at cite time**. A derived row citing content
that only *later* appeared is representable and undetected — deterministic
content is predictable, so "citing the future is impossible" is NOT claimed.
What is unconstructible is self/mutual citation (fixed point, §2). Temporal
soundness of citations is a non-claim, in the sighting tradition: the record
proves what was reachable, not why or when the producer knew it.

**Derived-row shape invariant (enforced):** non-empty sources ⇒ empty
`events` and empty `phantom_reaches` (derived rows are harness-minted, not
model turns). Token counts permitted. Violation ⇒ chain violation.

**Sources are EXACT, not a lower bound** (superseding r2's framing): §5b
makes a summary's sources the `elided` half of a recorded partition, so the
producer no longer guesses which entries it consumed — the set is computed,
recorded, and cross-checked against the window manifest at verify time. What
r2 called "the residue" was an artifact of asking the producer to remember;
the window grain asks the record instead.

### 3.1 What sources do NOT claim

Derivation, not faithfulness. A summary citing the right turns can still
misrepresent them — #1787-family work. The chain makes the citation
tamper-evident; it does not make the summary true.

### 3.2 v1 rows and the new columns

The additive migration gives v1 rows `sources = '[]'` as writable, unhashed
bytes. On v1 rows these columns are never **interpreted** — not by
verification, not by #1787 — though they are content-id inputs (§2, loud
orphans on tamper). Pre-bump `phantom_reaches` remain forever unprotected
by chain *and* witness (the witness stores a v1 chain hash, which excludes
them). The retired law is scoped to v2 rows; its test constructs v2 rows
and its docstring states this residue.

## 4. `phantom_reaches` into the hash

Mechanics in §2.1. Re-classification is append, not edit: a later, better
classifier appends a derived record citing the turn it re-classifies.

## 5. Per-writer tip witnesses (D2 — recommended IN)

```sql
CREATE TABLE IF NOT EXISTS writer_tips (
  conversation_id    TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  writer_fingerprint TEXT NOT NULL,
  tip_hash           TEXT NOT NULL,
  tip_seq            INTEGER NOT NULL,   -- the seq this witness pins
  PRIMARY KEY (conversation_id, writer_fingerprint)
);
```

`tip_seq` exists because of the r2 rollback finding: a rolled-back binary
appends turns without maintaining `writer_tips`, leaving rows *stale but
honest*. Without the seq, staleness is indistinguishable from tampering and
re-upgrading would refuse legitimate history. With it, three verdicts:

* `tip_seq == writer's final seq` → hashes must match, else **violation**;
* `tip_seq <  final seq` → **stale, not violated**: the witness pins the
  interior turn at `tip_seq` (verify against that row — cheap, and interior
  rows are chain-pinned anyway); repaired by the writer's next append under
  a current binary;
* `tip_seq >  final seq` → **violation** (rows deleted out from under the
  witness).

Write path (`append_turn_full`), in order, one transaction:

1. **Check the appending writer's own row** (seq-aware, above); mismatch at
   equal seq refuses the append — without this, a tamper of a non-tip
   writer's final turn is laundered and its only evidence overwritten by
   that writer's next append (r1 finding). Absence = skip.
2. The conversations-row tip check (recorded tip writer), as today.
3. **Witness relocation on handoff** (r2 finding): if the appending writer
   differs from the recorded tip writer X and X has no `writer_tips` row,
   step 2 has *just verified* the conversations-row witness against X's
   final turn — copy it into `writer_tips[X]` before step 4 overwrites it.
   This is relocation of verified evidence, not backfill-from-rows: the
   witness existed and was checked a statement ago. Without it, the first
   post-migration handoff append destroys the only witness pinning a
   pre-v2 writer's final turn.
4. Insert the turn; upsert the appender's `writer_tips` row; update the
   conversations-row tip.

Read path, three directions: every writer with turns → its row (if
present) verifies seq-aware; every `writer_tips` row → its writer must
have turns **or** the row must equal that writer's genesis (zero-turn
conversations are a legitimate create/import shape — r2 finding; r2's
unconditional "witness for nothing refused" would have bricked them);
conversations-row tip and `writer_tips` must agree where they cover the
same writer at the same seq.

**`import_one_record` writes witnesses too** — it is the second
`insert_turn_row` call site, already writes the conversations tip in its
transaction, and "predates the table" is false for rows it freshly writes.

**Backfill rule:** a missing row is absence — never computed from turns at
migration (a witness derived from the rows it witnesses agrees by
construction; vacuous green). Relocation (step 3) is the one sanctioned
migration of a witness, because it moves *checked* evidence.

**Stated bounds:** on a fully-migrated conversation (blank conversations
tip, zero `writer_tips` rows) every witness check no-ops until the first
post-bump append — during that window the erasure bound is the old
single-edit one, and the "both must be erased" claim holds only once
`writer_tips` rows exist. Deleting a writer's row remains a one-DELETE
erasure (indistinguishable from pre-table absence). A writer retired
before migration whose conversation is never appended again by anyone
stays unwitnessed; if anyone else appends, step 3 rescues the recorded tip
writer only. Keyless witnesses beside the data; the boundary moves with
out-of-store anchors (agent-frame), not here.

## 5b. Compaction reversibility — SUPERSEDED, see 5b.0

### 5b.0 STOP: most of this already exists

r5 was reviewed (32 findings) and one of them is worth more than all the
rest: **a content-addressed store of the verbatim elided span already
ships**, and this spec spent three design rounds rebuilding it badly.

`SpillStore` + `SpillProvenance::CompactionSpan` (`agentic/content_spill.rs`,
`agentic/compress.rs:1033`) already:

* stores the **verbatim elided span**, content-addressed — the CID is a pure
  function of the content;
* redacts on store (the same closed table `spill:` uses);
* dedups idempotently and **fails closed** on a CID present with different
  bytes (`SpillError::IntegrityViolation`);
* advertises the handle inside the summary body, so it lands in the turn's
  `user` field — which is **inside the canonical encoding**, making the
  citation tamper-evident already;
* resolves through `memory_fetch("compaction:<cid>")`.

Its own comment states this spec's goal outright: *"The summary is demoted
from sole replacement to a catalog card over a retrievable span."*

**This is strictly stronger than anything §5b designed.** Seals recovered
turn REFERENCES; the span recovers the BYTES. And it dissolves the problem
that killed r4 and shaped r5: a mid-turn cut is fine, because a verbatim
span expresses any cut. The turn-alignment rule (§5b.1) exists only because
turn ids cannot express a mid-turn cut — with the span, it is unnecessary.

**Two gaps remain, and they are the whole job:**

1. `SessionSpillStore` is *"in-memory, session-scoped, discarded at session
   end"* — so reversibility does not survive a restart. A **durable,
   content-addressed home for compaction spans** is the actual missing
   piece.
2. The Summarizing provider — the ONLY path that persists a summary — passes
   `compaction_store: None` (`memory.rs:1238`), so on that path no span is
   created at all.

**On storing a rendering.** §5b.7 argued that storing wire messages makes a
rendering authoritative, which is backwards. That objection does not apply
here: the span is not a competing record of the conversation, it is a
**receipt for a lossy transformation** — evidence of what was consumed. The
turn rows remain the authority. A receipt records what was rendered; it does
not claim to be the thing rendered.

Everything below is retained as the record of what was tried and why it
failed. It is NOT the plan.

---

## 5b (superseded). Compaction seals — reversibility, at a grain the record can express

**The insight this section is built on:** a conversation window is derived
from a previous window, and if that derivation is recorded, compaction
becomes **reversible** — you can say what was removed and get it back.

r4 tried to record this as a *partition of window membership over content
ids* and a review killed it (39 findings). This revision keeps the insight
and replaces the encoding. What changed, and why, is stated at each rule —
the failures are more instructive than the design.

### 5b.1 The alignment rule: only cut where the record can express the cut

> **A compaction may cut only at a turn boundary.**

r4 assumed the cut was turn-aligned. It is not: `compute_boundary` walks
WIRE MESSAGES with `TAIL_MIN_MESSAGES = 3` over a pair-shaped history, so on
the normal path the boundary lands mid-turn.

The reflex is to call this a fidelity trade — record turns, lose the exact
window. It is the opposite. **A mid-turn cut is already irreversible**: the
store holds no half-turns, so a window split inside one can never be
reconstructed from the record, by anything. Aligning the cut to turns does
not cost fidelity; it is the only way to have any.

This is also not a new mechanism. `compute_boundary` already performs three
alignment passes, including:

```rust
// never start the tail inside a result group — pull the cut back to the
// assistant carrying the tool_calls so call/result pairs stay together
while tail_start > head && messages[tail_start]["role"] == Some("tool") { … }
```

It already refuses to cut where the result would be *incoherent*. This adds
one more pass refusing to cut where the result would be *unrecordable*.

**The rule, concretely.** In the provider's wire view a turn boundary is
identifiable: `SumTurn::to_wire` emits at most `[user, assistant]` per turn
and skips empty sides, and its own doc states that `system`/`tool` roles
never occur there. So a turn starts at every `user` message. The pass is:

```rust
// A cut inside a turn produces a window the record cannot express —
// pull back to the turn's own start. Exact parallel to the tool-group
// pass above, for a stronger reason: incoherent vs unrecordable.
while tail_start > head && messages[tail_start]["role"] != Some("user") {
    tail_start -= 1;
}
```

**Cost: at most ONE MESSAGE**, not one turn — the pair is `[user,
assistant]`, so a cut landing on the assistant moves back exactly one. The
earlier "bounded by one turn" was pessimistic. Every tail rule today
(`TAIL_MIN_MESSAGES`, the token-budgeted walk, the last-user anchor) is a
MINIMUM of protection, so growing the tail never violates their intent.

**This pass is OPT-IN, not global.** The agentic loop calls `compress` at
several sites over windows that are not provider history — system cards,
tool results, working-set cards, no pair shape — and those windows are never
sealed. Forcing alignment there would be meaningless at best. `CompressRequest`
therefore carries the alignment as a flag, set by the sealing producer only.
A summary entry is itself a lone `user` (compaction text, empty assistant),
so it reads as its own boundary and the pass lands on it correctly.

Aligning BEFORE summarizing also makes the summarizer's input exactly the
elided set, so the record describes what actually happened rather than
approximating it.

### 5b.2 The seal record

```sql
CREATE TABLE IF NOT EXISTS context_seals (
  conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  ordinal         INTEGER NOT NULL,   -- 1, 2, 3 … per conversation
  summary_writer  TEXT NOT NULL,      -- the summary turn, by its PRIMARY KEY
  summary_seq     INTEGER NOT NULL,
  elided          TEXT NOT NULL,      -- canonical JSON: [[writer, seq], …]
  seal_hash       TEXT NOT NULL,      -- chained; see 5b.4
  prev_seal_hash  TEXT NOT NULL,      -- genesis for ordinal 1
  PRIMARY KEY (conversation_id, ordinal)
);
```

Three deliberate departures from r4:

**Members are keyed by `(writer, seq)`, not content id.** Content ids
collide by design — Phase A's own doc says identical witnessed rows share
one, "harmless: they have no outgoing edges", which is true for CITATION and
false for MEMBERSHIP. A deduplicated set of content ids cannot express "turn
7 and turn 22 are both here", so a conversation with two identical turns
became unpartitionable and would have refused forever: fail-closed data loss
on untampered data. `(writer, seq)` is the `turns` PRIMARY KEY — unique by
construction.

**Seals are ordered by an `ordinal`, not by seq.** r4 defined membership
with a `sealed_at_seq` range, contradicting §3 of this same spec, which
rules that cross-writer seqs are not a causal order. A per-conversation
ordinal needs no cross-writer comparison.

**`carried` is gone.** It was derivable (everything before, minus everything
elided, plus prior summaries) and it was the source of r4's worst flaw: with
`elided = parent − carried`, the partition invariant held IDENTICALLY for
any `carried` whatsoever — no failure mode reachable by an honest producer,
which is the vacuous-green pattern in the check meant to be the centrepiece.
Recording only what was removed leaves claims that can actually be false.

### 5b.3 What is checkable, and what is not

**Checkable, with reachable failure modes:**

* Every `(writer, seq)` in `elided` resolves to a turn in this conversation.
* **Disjointness across seals**: no turn is elided by two seals. A turn
  removed twice was double-counted by the record.
* No seal elides its own summary turn, or any turn at or after it.
* Ordinals are dense from 1 — a missing ordinal is a deleted seal.
* The summary turn's `sources` (content ids, chain-protected since Phase A)
  are exactly the content ids of the turns `elided` names. Two independent
  descriptions of one set: one positional, one by content.

**NOT checkable, stated rather than faked:** that no turn was *silently
dropped* — left out of every seal and out of the window. Detecting a gap
needs the live window's membership, which is provider state, not store
state. r4 pretended otherwise by deriving membership from a seq range; that
derivation was the flaw, not the check. What the record does guarantee is
narrower and true: **anything a seal removed, it named.**

### 5b.4 Anchoring: seals are chained and witnessed

r4's `window_id` was an unkeyed BLAKE3 over the manifest's own public
fields. Edit the row, recompute the id, done — the whole graph could be
rewritten self-consistently without touching a turn. "Self-certifying" was
true against corruption and false against tampering.

Seals therefore carry the same machinery the turn chain already proved:

```
seal_hash = BLAKE3("newt-seal:v1"
                    ∥ len-prefixed: conversation_id, prev_seal_hash,
                                    summary_writer, elided
                    ∥ ordinal, summary_seq)
```

* Each seal chains to the previous (`prev_seal_hash`), genesis-anchored at
  ordinal 1 — so a seal cannot be altered without breaking every seal after.
* The conversation row carries a `seal_tip` witness, written in the same
  transaction as the seal — the §5 pattern, which already survived review,
  applied to the last seal (which nothing chains onto).
* The summary turn's `sources` remain chain-protected independently.

### 5b.5 Ordering with the trigger turn

r4 could not record the window it sealed: the summary is appended BEFORE the
triggering turn (`lib.rs:7134` then `7141`), so at seal time the turn that
is *in* the window has no row, no seq, and no id.

**The obvious fix is wrong.** Reordering to turn-then-summary breaks restore:
`Summarizing::restore_turns` finds the cut with
`rposition(|t| is_compaction_text(&t.user) && t.assistant.is_empty())` and
keeps `turns[k+1..]` — so a summary written LAST makes the restored working
set empty, silently discarding the trigger turn and everything after it. The
existing order is load-bearing, and `restore_turns`'s own docstring says so:
*"the record itself was appended just before the turn that triggered the
compression, so the turns after it are exactly the ones the live boundary's
last-user anchor guaranteed survived."* (Caught here rather than in review —
recorded because the near-miss is the point: a fix aimed at one invariant
walked straight into another one nothing was checking.)

**The actual fix: leave the turn order alone and move the SEAL.** Append the
summary, append the trigger turn, then write the seal — all in ONE
transaction. The seal goes last, when every row it names exists; the rows
keep the order restore depends on. r4's mistake was writing the seal at
summary-append time, not the order of the turns.

One transaction is load-bearing rather than tidy: a summary persisted
without its seal is a derived row whose provenance says nothing, and today
the two appends are separate transactions (`store.rs` opens and commits its
own per call), so that state is currently constructible.

### 5b.6 `/compress` seals through the same path

`/compress` today replaces the live window with `wire_messages_to_turns(...)`
— synthetic turns carrying no store identity — and persists nothing. That is
why it is the highest-probability route to an unsourced later compaction: the
next automatic seal draws its middle from material that can no longer be
named.

Under r5 it does not need a special case, because the divergence IS the bug:
`/compress` is a compaction, so it takes the same turn-aligned cut, splices
its surviving history by index (preserving ids rather than rebuilding from
wire), persists its summary, and writes its seal. One sealing path, two
triggers — automatic and operator-invoked.

### 5b.7 What reversibility means, precisely

From the record you can recover, for any seal: **which turns it removed**,
and **their content** (turn rows are insert-only; nothing deletes a turn
short of deleting its conversation). Re-render from those turns to
reconstruct the window.

You do NOT recover the exact byte stream: the rendering depends on the
system prompt and tool schemas in force at the time, which change with the
model. That is the same rule already settled for backend switching —
re-render, never re-summarize — and it is why the record keeps turns rather
than wire messages. Storing the wire would make a *rendering* authoritative,
which is backwards under the first principle, and would churn on every
backend switch.

## 6. `scratchpad` / `plan` (D3 — recommend A)

Mutable by design; cannot live in an append-only chain. **A: classify as
unprotected working memory now**, documented at the columns and on
`load_verified`'s coverage boundary (both are rehydrated into the restored
session and are model-visible). C (append-only working memory) is the
correct end-state and is agent-frame-shaped redesign. B (per-turn snapshot
hashes) rejected: a half-guarantee at full complexity.

## 7. Mixed epochs (D4 — allowed, unrestricted)

v1 and v2 rows coexist; `prev_hash` of a v2 row is its predecessor's hash
under the predecessor's own version; a v2 summary cites v1 turns by content
id (computable from v1 rows). No re-encoding of old rows, ever.

## 8. The producer: the record answers, not the producer's memory

The only summary persisted as a turn row is the Summarizing provider's
sync-time record; the mid-turn CONTINUATION compaction never persists
(confirmed by r2 review).

r3 specified a post-save tagging seam so the provider could remember which
history entries the summarizer ate. §5b removes the question. A seal records
a **partition of the previous window**, and both halves are answerable from
durable state:

1. `append_turn_full` returns the new row's content id (needed regardless —
   the summary turn's id goes in the manifest).
2. At a seal, the producer reports the **cut**: which of the previous
   window's members survive. Everything else in that window is `elided`.
   The previous window's membership is not remembered — it is *read*: the
   last seal's `carried` plus its summary, plus every turn appended since,
   by seq.
3. `ConversationRecord` turns carry their content id (computed by
   `load_record_on` from stored bytes), so a restored window's members are
   known ids from the moment they are materialized.

**The migration case, which drove D5, was never the problem** (evidence pass,
2026-08-22): the provider's history is process-local, so pre-upgrade material
cannot be in a new binary's provider except by RESTORE — and a restore
materializes from the store, where every member's content id is known. The
first seal in an old conversation is therefore fully sourced. What remains
true: `parent_id` is NULL at a conversation's first seal, which is exactly
right, and the store enumerates the pre-seal membership directly.

### 8.1 Three producer defects this must fix, not work around

The same evidence pass found the routes by which identity is actually lost.
All three are producer bugs, and the window grain fixes them rather than
tolerating them:

**P1 — tags are destroyed at every compaction.** `compress_via_pipeline` ends
with `self.history = wire_to_history(&messages, ...)`, rebuilding history
from wire messages that carry only `{role, content}`. Any id attached to a
history entry is discarded, recurring, every compaction from the second on.
FIX: the surviving tail is a CONTIGUOUS SUFFIX of the pre-compaction history,
so it is spliced by INDEX — `[summary] + old_history[cut..]` — instead of
reconstructed from wire. This is strictly more faithful than the current
rebuild (which is a lossy heuristic: 1–2 messages per turn, empty-assistant
skip), and it carries ids across for free. No content matching anywhere.

**P2 — `compress` does not tell its caller where it cut.** `Boundary` is
private and `CompressOutcome` carries no cut. Without it the provider cannot
know which entries formed the middle, and §8's rule forbids guessing by
content. FIX: `CompressOutcome` carries the cut (survivor count in history
terms). This is the one seam this work adds to the pipeline, and P1's splice
depends on it.

**P3 — `/compress` wipes store identity from the entire history.** It calls
`restore_turns(wire_messages_to_turns(&outcome.messages))` — wire-fabricated
turns with no store identity — and never persists a summary. The manual
command is therefore the highest-probability real route to an unsourced
later compaction: the NEXT provider-minted summary summarizes a middle whose
entries have no ids. FIX: `/compress` is a compaction, so it SEALS like one —
it persists its summary and records a manifest. That closes the hole at its
source instead of adding a "sometimes we can't say" branch downstream.

With P1–P3 fixed, an entry that corresponds to a persisted turn always
carries that turn's id, so **zero-citable is unreachable rather than
merely unlikely** — which is the standard §5b sets and D5 could not meet.

**Producer failure semantics.** The summary row and the trigger turn are
currently appended by two independent `append_turn_full` calls, each opening
and committing its own transaction — so a summary can commit while its turn
fails. FIX: one transaction for the cycle's rows and the manifest, so a
persisted summary without its seal cannot exist. A dropped cycle then
narrows what is carried; it can never fabricate an elided set.

## 9. Failure semantics

Inherited from #1792: refuse loudly, repair nothing, name what is known,
Display-visible. New classes: orphan source; malformed/non-canonical
sources; derived-shape violation; per-writer witness mismatch (writer +
seq); witness-over-deleted-rows (`tip_seq > final`); witness divergence.
Stale witness (`tip_seq < final`) is **not** a failure class — it is the
rollback residue, verified at its own seq and repaired by the next append.

**Seal classes (§5b):** broken seal chain (`prev_seal_hash` mismatch); seal
tip witness disagreement; unresolvable elided member; a turn elided by two
seals; a seal eliding its own summary or a later turn; a gap in the ordinal
sequence; `sources` and `elided` describing different sets.

**If an unsourced derivation is ever produced anyway** (a defect, not a
design state): the row is persisted with the reserved sentinel
`["0"*64]` in `sources` rather than an empty array. The sentinel keeps the
biconditional (non-empty ⇔ derived), is already inside both digests so it is
tamper-evident with no encoding change, and is excluded from the orphan
check by construction — it resolves to no turn ON PURPOSE, which is the
honest statement "derived, inputs not citable". It is a bug report in the
record, not a supported path: §10 asserts no production path emits it.

### 9.1 Version skew and downgrade

An old binary reading any conversation containing a v2 row: verify and
append refuse ("carries encoding_version 2 … upgrade newt"); plain `load`
still reads. **Fail-closed lockout, one-directional, stated and
accepted**: binaries sharing a store upgrade together, or the older goes
verified-read-only on touched conversations. An old binary also does not
maintain `writer_tips` — §5's `tip_seq` turns that from a re-upgrade brick
into a stale-and-repaired state. Two levers this spec owns: **the legacy
JSON import is pinned at v1** (legacy records carry no sources or reaches;
a post-import rollback then still verifies imported history — the import
retires its source tree and cannot re-run), and the unknown-version
primitive test re-pins its future vector at **3** (the bump consumes 2).

## 10. Acceptance — red-first

### 10.1 The two laws, strengthened BEFORE un-ignoring

* `derived_records_name_their_sources`: r1's Debug-grep flips green the
  moment the field exists. Strengthened: drive the REAL producer
  (Summarizing provider, compression fired, record persisted) and assert
  the persisted summary carries non-empty sources equal to the `elided`
  half of the seal's recorded partition. Red today; red after the schema
  change alone; green only when §5b/§8 are wired.
* `the_whole_record_is_covered_by_the_chain`: constructs a v2 row, tampers
  `phantom_reaches`, chain must break; docstring states the §3.2 residue.

### 10.2 Byte-format vectors — a repair, not an addition

r1's premise was false: **no pinned known-answer vectors exist for v1 at
all** (only relative assertions). This bump adds pinned input→hex vectors
for v1, v2, and the content id, in `turn_chain.rs`.

### 10.3 Regressions

Tampered sources → chain breaks; orphan refused; non-canonical sources
refused; derived-shape violation refused; self-citation unconstructible is
*documented* (not tested — one cannot test a fixed-point search); mixed
epoch verifies; v1-only verifies byte-for-byte (vectors); per-writer
witness catches a non-tip final-turn tamper at read AND at that writer's
own next append; stale witness accepted and verified at its own seq;
witness-over-deleted-rows refused; genesis witness on a zero-turn
conversation accepted; **the cut is turn-aligned (a compaction over a
pair-shaped history never splits a turn — asserted against the real
boundary function, since this is the rule everything else rests on); an
edited seal breaks the seal chain; an edited last seal breaks the seal tip
witness; a turn elided by two seals refuses; a seal eliding its own summary
refuses; a missing ordinal refuses; `sources` and `elided` describing
different sets refuses; and REVERSIBILITY AS A TEST — after two real
compactions, every elided turn is recovered from the record and its content
matches what was summarized;** handoff relocation preserves the outgoing writer's
witness (tamper X's final turn *after* a W handoff → caught);
import-then-verify with witnesses; fingerprint change after import stays
closed; downgrade proxy (version-3 vector); seam test extended to a
multi-writer tamper.

## 11. Deliberately not claimed

* Faithfulness of summaries (§3.1); temporal soundness of citations (§3).
* Byte-exact reversal of a compaction. §5b.6 recovers the elided TURNS; the
  rendering is re-derived, and a different model or tool schema renders the
  same turns differently.
* Detection of a turn silently dropped from every seal AND the window
  (§5b.3) — that needs the live window's membership, which is provider
  state. The record guarantees the narrower, true thing: anything a seal
  removed, it named.
* Working-memory integrity (§6); v1 rows' new-column bytes (§3.2).
* Witness erasure bounds (§5), including the migration window.
* Cross-conversation citation — representable later (content ids are
  store-agnostic), verified only same-conversation now.

## 12. Decisions

| # | Decision | Recommendation |
|---|---|---|
| D1 | Source reference | Content id, sources-in-preimage, stored bytes (both digests) |
| D2 | Per-writer witnesses this epoch | Yes — seq-aware, own-check on append, handoff relocation, import coverage |
| D3 | scratchpad/plan | Classify unprotected now; append-only redesign later |
| D4 | Mixed epochs | Allowed; never re-encode; downgrade lockout stated; legacy import pinned v1 |
| D5 | Summary with zero citable inputs | **DISSOLVED by §5b** — a seal always succeeds a seal, so the edge is always exactly one. No call needed. |
| D6 | Cut alignment | **Turn-aligned, pulled back** (§5b.1) — a mid-turn cut is already irreversible, so this costs no fidelity; the pipeline already aligns cuts for coherence, this adds one pass for auditability. |
