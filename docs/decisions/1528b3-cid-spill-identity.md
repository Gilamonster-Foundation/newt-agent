# #1528 B3 follow-up — content-addressed spill identity

Status: **in progress** (identity core landed; consumer migration + memory_fetch +
Lean/TLA+ pending). Stacked on #1538 (`feat/1528b3-proactive-compaction`).

## Problem

#1538 gave the compaction/tool-offload spill store an **allocated-identifier**
model (a store-issued reservation id). Three adversarial review rounds each found a
new failure mode of *identifier allocation under concurrency*: predicted ids from
`spills()`, cross-store reservation tokens, double-commit, a concurrent writer
claiming a candidate's next sequential id, phantom handles when the store is absent.
Each round guarded one more hole. The bug **class** — mutable id allocation — kept
regrowing.

## Decision

Replace allocated identity with **content addressing**. A spill handle is the
**BLAKE3 CIDv1 of the canonical dag-cbor** of a versioned, session-scoped record
(via the published `content-addressable` crate's frozen core). The handle is a pure
function of the content, so the entire allocator bug class becomes *unrepresentable*
rather than guarded:

| #1538 had to prove | Content addressing gives for free |
|---|---|
| reservation belongs to this store | a CID is not a store token — nothing to misbind |
| reservation not already consumed | `put_if_absent` is idempotent; re-commit is a no-op |
| `next_id` did not move | there is no next sequential id |
| no concurrent writer claimed the id | two identical payloads *converge* on one CID |
| no silent rebind to another payload | different bytes ⇒ different CID, by construction |
| rejected candidate rolled back its id | a rejected candidate's CID was simply never published |

The remaining obligation shrinks to one invariant: **`CID(stored bytes) == the
requested CID`** — smaller and stronger than the reservation protocol it replaces.

## Design

- **`SpillRecordV1 { schema, scope: Session(nonce), provenance, redacted_text }`** —
  a versioned, self-verifying record (not a hash-shaped filename). `impl
  ContentAddressable` via `canonical::to_canonical_dagcbor`, so equal records ⇒ equal
  bytes ⇒ equal CID. The schema tag versions the address space; a migration is a new
  schema, not an in-place rewrite.
- **`SpillCid(ContentId)`** — no constructor from an arbitrary string. `parse` goes
  through the crate's frozen `FromStr` (fail-closed on a malformed / foreign-codec
  handle), so "arbitrary strings never become a `SpillCid`".
- **`StagedSpill::from_record`** — *pure* CID derivation, store-independent. The
  candidate summary can render the handle before anything is live.
- **`SpillStore::commit_batch`** — idempotent `put_if_absent`, **all-or-none**
  (prevalidate the whole batch before installing anything), fail-closed on divergent
  bytes under a CID (`IntegrityViolation`) or a poisoned store.
- **`SessionSpillStore`** — in-memory, session-scoped; stamps the session nonce.

### Privacy — session scope (equality-leak seal)

A **global** plaintext CID would leak equality: anyone who can *guess* a
secret-bearing payload could compute its CID and confirm a match (a dictionary /
confirmation surface, even though the CID never reveals content on its own). The
record therefore carries a **per-session nonce**, so identical plaintext in two
sessions gets *different* addresses. Same-session dedup is preserved; cross-session
dedup, if ever wanted, is an explicit trusted-local-store policy — never the default.
The nonce is minted once at session start (random in production, fixed in tests); the
store owns it, so a caller can't forget to scope.

We deliberately do **not** use keyed BLAKE3 while still labelling the multihash as
plain BLAKE3 `0x1e` (that code means *unkeyed*). Domain separation is through the
canonical record (schema + scope + provenance), not a mislabelled keyed hash.

### Authorization stays separate from identity

A CID proves *"these bytes hash here"*, never *"this agent may read them"*. The read
path is mediated by the **session-scoped store** — a `memory_fetch("compaction:<cid>")`
resolves only if the CID is present in *this* session's store (membership) and only
under this session's nonce. A model cannot paste a foreign CID from another
conversation and retrieve it: it neither belongs to this store nor re-derives under
this nonce. Membership + nonce *is* the capability boundary; no separate token type is
needed at the in-memory tier.

### Publication is still transactional

Content addressing removes the *identifier-allocation* transaction, not the
*publication* transaction. The invariant the loop must still keep: **live `input`
references a CID only after `commit_batch` has installed it.** The candidate path
therefore stages CIDs (pure), validates provenance + budget, then commits the spill
batch **before** swapping in the compacted `input`. On any reject, the candidate —
including its (never-published) CIDs — is dropped whole.

### Orphan-blob GC

For the in-memory `SessionSpillStore`, GC is trivial: the map is dropped at session
end / `/new`, so a staged-but-never-referenced object simply disappears. A future
*filesystem* CAS could leave harmless immutable orphan blobs on a failed publication;
those are unreferenced and reapable later. Out of scope here.

### Metrics — dedup changes their meaning

Deduplication makes a single `spills()` count ambiguous, so it splits:
- `unique_objects()` — unique CIDs physically present.
- `logical_spill_refs()` — committed handle references emitted into transcripts (a
  re-commit of a present CID still counts as a reference).
- `offloaded_chars()` — chars of *unique* committed payloads elided from context.

## Reuse discipline

- Identity/canonicalization: the **published** `content-addressable = "0.1.0-alpha.1"`
  crates.io release (frozen 0.1.x core: `ContentId` + `ContentAddressable` +
  `canonical`). No git/path pin; no second CID implementation. (When the crate cuts a
  stable `0.1.0`, repin to it.)
- One `SpillStore` for *both* consumers — the tool-offload (`spill:`) and compaction
  (`compaction:`) paths share it, as they shared the #1538 store. The reservation
  machinery is deleted, not left beside the new store.

## Formal obligations

- **Lean** (`formal/CompactionSpill/Basic.lean`) is re-based from the reservation
  kernel to the **CID law**: `same_payload_same_cid` (determinism),
  `committed_handle_resolves` (a committed CID fetches its own payload),
  `rejected_candidate_not_published` (a rejected candidate's CID is not in the live
  transcript), `published_handle_is_committed` (every live handle is backed by a
  committed object whose bytes hash to it). The identity half stops being a mutable
  allocator protocol.
- **TLA+** keeps only the temporal obligation content addressing does *not* discharge:
  publication ordering + concurrent commit (`NoLiveInputBeforeSpillCommit`,
  `EveryPublishedHandleResolves`, `ConcurrentCommitsOfEqualContentConverge`,
  `IntegrityViolationNeverOverwrites`). The allocator-protocol properties from #1538
  (reservation binding, single-use, all-or-none batch) are retired — the design makes
  them vacuous.

## Out of scope

- The `content-addressable` `merkle` / `NodeStore` surfaces — newt builds its own
  session store; it needs only the frozen core.
- A filesystem/durable CAS tier (and its real orphan-blob GC).
- Cross-session deduplication (an explicit trusted-store policy if ever wanted).
