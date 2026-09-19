---
name: provenance-audit
description: "Standing architectural audit for content addressing, provenance, and tamper-evident history. Use when designing or reviewing ANY data structure that stores, identifies, transmits, or records something — records, events, messages, caches, artifacts, logs, memory, checkpoints, wire formats, schemas, migrations. Also use when adding a hash, digest, id, seq number, or UUID to anything; when writing an undo/rollback path; and as a review pass on any PR that touches a store, a protocol, or an identifier. Answers: does this identify by content, is its history tamper-evident, is the evidence actually read, and is the change invertible."
---

<!-- MIRROR. This skill is carried in more than one place so the audit is
     discoverable from any working directory; when you edit it here, sync the
     other copies in the same change. The doctrine it enforces is canonical in
     `steward-charter/docs/CRAFT.md` §II — on any disagreement, the register
     wins. -->

# Provenance audit

**Founding principle, from Shawn, standing:**

> Data structures must be **tamper-evident**. Editing history without leaving
> **forensic evidence** and passing a **reversibility check** is against core
> values. Always look for ways to incorporate provenance, computational
> validation of data structures, and content addressing into a design.

This is deliberately **over-applied**. Reach for it more often than seems
necessary; a false positive costs one paragraph of reasoning, a false negative
ships a mutable blob with good intentions.

## STEP ZERO: inventory before you design

**Do this before writing a line of spec.** The expensive failure is not
designing badly — it is designing at all when the thing already exists.

Enumerate, in this order:

1. **What `content-addressable` already mints.** `ContentId` (canonical
   dag-cbor value), `RawContentId` (opaque bytes), `MerkleNode<T>` (payload +
   ordered causal parents, id over both — a DAG node, ready-made),
   `NodeStore` (+ `StoreError::Backend`, which anticipates a disk backend),
   `ClassifiedCid` (carry/compare foreign CIDs without minting them).
2. **What the target repo already has.** Grep for existing stores, spills,
   caches, ledgers, chains, and handles before proposing a new one.
   `grep -rn "blake3::hash(\|content_addressable\|SpillStore\|CID"`.
3. **Only then**, design the gap — and state explicitly what you found and
   are reusing.

### Why this step exists (2026-08-22, newt-agent #1786)

Three design rounds and ~70 confirmed review findings were spent building a
content-addressed span store, a dag-cbor identity scheme, and a Merkle DAG.
All three already existed:

* `SpillStore` + `SpillProvenance::CompactionSpan` already stored verbatim
  elided spans, content-addressed, redact-on-store, fail-closed on CID
  collision — its own comment stated the goal: *"the summary is demoted from
  sole replacement to a catalog card over a retrievable span."*
* `SpillRecordV1` already serialized via canonical dag-cbor through
  `content_addressable`, already in `newt-core`'s dependency tree.
* `MerkleNode` and `NodeStore` were already in that same crate.

Every review said "ground in real code", and the design *was* grounded in real
code — the code it was replacing. Nobody inventoried the neighbouring
machinery. **Grounding a design in the code it changes is not the same as
inventorying what is already available to it.**

Canonical law: `steward-charter/docs/CRAFT.md` §II — *Identity is derived, not
assigned* · *History is tamper-evident and invertible* · *Evidence nobody reads
is decoration*. On any disagreement, the register wins.

Reference implementation: **`content-addressable`**
(github.com/hartsock/content-addressable) — Rust core + PyO3 Python binding over
that same core, so an id minted in Python is byte-identical to Rust's.
`ContentId` (CIDv1 · DAG-CBOR · BLAKE3-256) names a canonical structured
**value**; `RawContentId` (CIDv1 · raw · BLAKE3-256) names an opaque **byte
string**; `ClassifiedCid` carries and compares foreign CIDs without minting
them. The profile is semantic: the two never compare equal even at identical
digest bytes.

---

## The four questions

Ask all four. Answer each with a **file:line**, or with "no" — never with a
plan.

### 1. Is identity derived from the bytes?

- Does every record, artifact, message, and span have an address **computed
  from its content**?
- Are `seq`, UUID, path, and timestamp used only as **locators** beside that
  address, never as the identity itself?
- Does the identifier **self-describe** its algorithm and codec (a multihash
  CID), or is it a bare hex string with the scheme in a comment?

**Fail signals:** `sha2`/`blake3` used directly to mint an id · `String` or
`[u8; 32]` as an id type · `<algo>:<hex>` or bare-hex dialects · a `Digest`
newtype that could hold any algorithm and is compared with `==`.

### 2. Is history tamper-evident?

- Is the record **append-only** and **hash-linked** (each row committing to its
  predecessor)?
- When something must change, is the change an **event** that names what it
  shadowed, leaving the prior state addressable?
- Or does the code `UPDATE` / truncate / rewrite in place?

**Fail signals:** in-place mutation of a stored record · a compaction or prune
path that leaves no durable trace of what it removed · "we'll just rewrite the
file."

### 3. Is the evidence actually read?

**The one people fail.** Writing the chain is not the obligation; checking it
is.

```bash
# every integrity function needs at least one caller outside tests
grep -rn "verify_chain\|verify_integrity\|check_digest" --include=*.rs . \
  | grep -v "/tests/\|/benches/\|cfg(test)"
```

- Is there a verifier on a **production path**, reachable without a test
  harness?
- Does a verification failure cause a **refusal**, or only a log line?

**Fail signals:** a verifier whose only callers are tests · a digest recomputed
and compared but the result `let _ = ` discarded · "we store the hash so we
could check it later."

**Known live instance:** `newt-agent`'s `ConversationStore::verify_chain`
(`newt-core/src/store.rs:2342`) is written by every append and read by **nothing
in production** — tests and one offline bench only, while restore feeds
unverified rows to `restore_turns`. Confirmed in the working tree 2026-08-20.
Assume this is the default state of any chain you did not personally wire a
verifier for.

### 4. Is the change invertible?

- Does the mutation carry an **inverse** the runtime can apply?
- Is the inverse **exercised** — a test that mutates, reverts, and asserts
  byte-equality with the pre-state?
- Do inverses **compose** when changes interleave?

Prior art to lift rather than reinvent: **revertible effects** in *A Programming
Paradigm for Spatiotemporal Composability* (github.com/cordiverse/paper, draft
2026-08-13) — every context transformation carries a tracked inverse, with a
calculus of dynamic composition that carries the property from one component to
a system of interleaved ones. It is the theory under Cordis, the framework
DeepSeek Harness is built on. Read before hand-rolling an undo path.

---

## Adoption ladder

Place each subsystem on this ladder and name the next rung. Do not skip rungs.

| Rung | State | Next move |
|---|---|---|
| 0 | No identity — position, path, or nothing | Give it a content address |
| 1 | Ad-hoc digest (`sha2`/`blake3` inline, bare hex) | Replace with `ContentId`/`RawContentId`; keep the old form only as a legacy parser |
| 2 | Vendored/forked copy of addressing code | Depend on the published crate/package; delete the copy |
| 3 | Real dependency, ids minted correctly | Hash-link the history |
| 4 | Tamper-evident history, verifier only in tests | **Wire the verifier to a production path** |
| 5 | Verified on read, refuses on mismatch | Add the inverse and prove it composes |

Rung 4 is where most systems actually sit while believing they are at 5.

---

## What this audit is not

- **Not a mandate to hash everything.** A cache key computed from content is
  content addressing; a UUID on a UI widget is not a provenance failure. The
  test is whether anything downstream would be harmed by a **substituted or
  silently altered** value.
- **Not an excuse to keep bytes forever.** Retention is a separate axis with its
  own privacy cost. Prefer **digests in the durable record** over raw payloads —
  a digest is tamper-evident without being a disclosure liability. (`newt`'s
  digest discipline vs. `dsh` logging raw tool args verbatim forever is the
  worked contrast.)
- **Not satisfied by a signature.** A signature proves *who said it*; a content
  address proves *what it is*. Authority attenuation and content addressing are
  orthogonal, and `agent-bridle` needs both.

## Output

Report per subsystem: current rung, the four answers with file:line, the single
next move, and whether that move is a **gate** (blocks merge) or a **backlog
item**. Prefer three real rungs climbed over twelve subsystems catalogued.

<!-- markdownlint-disable-next-line MD013 -->
Model: claude-opus-5[1m] | Harness: Claude Code | Operator: Shawn Hartsock | Time: 10:42 EDT | Date: 2026-08-20
