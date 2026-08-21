# Spec: the v2 canonical turn encoding (#1786)

**Status:** DRAFT r3 — r1 reviewed (33 findings, 29 confirmed), r2 reviewed
(19 findings, 16 confirmed); every confirmed finding is addressed below or
carried as a stated bound. Ready for decision.
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

**Sources are a LOWER BOUND** (r2 finding): the array lists the citable
inputs, not necessarily everything the summarizer consumed. Window entries
with no persisted row — a turn whose save failed, wire-fabricated
`/compress` windows — are not citable and are omitted rather than guessed.
§10.1 asserts equality on the happy path; the residue is this stated bound,
never an orphan-producing fabrication.

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

## 8. The producer: ids flow from the store, one cycle late

The only summary persisted as a turn row is the Summarizing provider's
sync-time record; the mid-turn CONTINUATION compaction never persists.

r2's "tag at sync time" was **unimplementable in the real order** (r2
findings): the summary is minted *during* sync, the rows are appended
*after*, and ids exist only once `append_turn_full` runs. The corrected
mechanism — **post-save tagging, one cycle late**:

1. `append_turn_full` returns the new row's content id. The save path
   returns the cycle's appended ids (turn row; summary row when present)
   to the chat loop.
2. After a successful save, the loop calls one new provider seam —
   `attach_row_ids(turn_id, summary_id)` — and the provider tags the
   history entry it synced this cycle and (when a compaction fired) the
   compaction entry it minted this cycle. Deterministic: the provider tags
   the entries it just created, by position in its own history, no content
   matching anywhere.
3. `ConversationRecord` turns carry their content id (computed by
   `load_record_on` from stored bytes), so a store-fed `restore_turns`
   re-tags a rehydrated window — including prior summary entries, which
   are persisted rows like any other. The `/compress` path's
   wire-fabricated windows carry no identity and stay untagged (§3's
   lower bound).
4. Compression consumes only prior-cycle entries as its middle (the anchor
   rule keeps the trigger turn out), so by the time a summary is minted,
   every *citable* middle entry is already tagged. Untagged entries
   (failed saves, wire windows, pre-bump history never re-restored) are
   omitted per §3.
5. `take_compaction_record` returns `(record, Vec<ContentId>)`; the
   persist site passes them as the summary's sources.

**Producer failure semantics** (r2 finding): a failed save leaves that
cycle's entries untagged — omitted from later sources, never guessed. A
summary whose sources came out *empty* because nothing in its middle was
citable persists as a derived-shaped row with empty sources — which the
shape invariant reads as witnessed; to keep the biconditional honest, such
a record is persisted with sources = the sentinel it cites: nothing. This
is the one case where derived and witnessed are indistinguishable on the
wire, bounded to windows with no citable input, stated here rather than
hidden. (Review may prefer refusing to persist such summaries; that
trades a provenance gap for losing the recovery handle — decision D5.)

**Producer correctness is an acceptance obligation** (§10.1): a
wrong-but-existing source set passes every verifier check and is then
chain-protected permanently. The verifier cannot catch a plausible lie
about derivation; the test suite pins the producer.

## 9. Failure semantics

Inherited from #1792: refuse loudly, repair nothing, name what is known,
Display-visible. New classes: orphan source; malformed/non-canonical
sources; derived-shape violation; per-writer witness mismatch (writer +
seq); witness-over-deleted-rows (`tip_seq > final`); witness divergence.
Stale witness (`tip_seq < final`) is **not** a failure class — it is the
rollback residue, verified at its own seq and repaired by the next append.

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
  the persisted summary carries non-empty sources equal to the content ids
  of the turns actually summarized. Red today; red after the schema change
  alone; green only when §8 is wired.
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
conversation accepted; handoff relocation preserves the outgoing writer's
witness (tamper X's final turn *after* a W handoff → caught);
import-then-verify with witnesses; fingerprint change after import stays
closed; downgrade proxy (version-3 vector); seam test extended to a
multi-writer tamper.

## 11. Deliberately not claimed

* Faithfulness of summaries (§3.1); temporal soundness of citations (§3).
* Completeness of sources (§3's lower bound; D5's empty-sources case).
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
| D5 | Summary with zero citable inputs | Persist with empty sources (stated gap) — or refuse and lose the recovery handle? **Needs a call.** |
