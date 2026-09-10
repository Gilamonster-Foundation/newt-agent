# Smart harness — design record

Implementation contracts and verification evidence are recorded in
[smart-harness-implementation.md](smart-harness-implementation.md).

> **Canonical copy.** Mirrored on the knowledge board as
> `board/newt-agent/2026-09-08_smart-harness-DESIGN-RECORD.md`; on drift, this
> file wins because it is versioned with the code it describes. Prior decisions
> this builds on: `docs/decisions/1528b3-cid-spill-identity.md`,
> `docs/decisions/1528b3-proactive-compaction.md`.

**Status:** DESIGN UNDER REVISION. Revised against `main` at `ddbf870f`
(2026-09-09). The elision kernel and its forensic CLI landed in **#2246**;
the smart-harness integration described here is not implemented.

An earlier revision of this file marked all thirteen decisions **LOCKED** and
said implementation could begin. **That status was not supported by the
evidence** and is withdrawn. A design review checked the claims against source
and found seven that are false or overclaimed, one of which inverts the central
before/after argument.

**The body of this document now says what the code actually does.** The first
revision recorded the corrections in §6b but left the original claims standing
in the inventory, the diagrams and the prose above it — so the document
asserted and denied the same things in one file, and a reader who stopped
before §6b was misled by every one of them. That is fixed: the inventory, the
§3 and §4 diagrams, and §5 have been rewritten to the verified behaviour, and
§6b is now a **retrospective** of what the failed revision claimed. Nothing is
lost by making the present tense accurate — git history holds the revision that
got it wrong, and §6b holds the accounting of how.

D10–D13 remain the operator's decisions and stand as *choices*. What does not
stand is my characterisation of what the code already does, and several
"grounded in existing law" claims that the law does not actually support.

**Implementation of the smart-harness integration does not begin from this
document in its current state.** The existing kernel is inventoried below;
D1–D3 still owe integration contracts. The separately-shippable half of the
#2239 fix (§6c) never depended on
it and has already shipped: **#2251 merged 2026-09-09 (`44ff61c8`)**, so an
exhausted narration rescue is no longer reported as a completion. **#2239 itself
remains OPEN** — the accept site, the `NarrationFinalRound` decision and TUI
behaviour are all outside what #2251 did (§6c residuals 1-3), and they continue
independently of this document too.

Companion: knowledge board `2026-09-08_harness-as-a-tiny-llm-DESIGN.md` (the
thesis and its recovery). This document is the accounting.

---

## 0. Inventory — what is reused, not designed

Per the provenance-audit rule: enumerate what exists before designing the gap.
The current-main baseline is [#2246's merged tree](https://github.com/Gilamonster-Foundation/newt-agent/tree/ddbf870facbba1b895dfd66793b157c973b0c0fd).
The [monorepo decision](https://github.com/Gilamonster-Foundation/newt-agent/blob/ddbf870facbba1b895dfd66793b157c973b0c0fd/docs/decisions/agent-frame-v0-monorepo.md)
supersedes the separate-repository release plan.

| already exists | where | role here |
|---|---|---|
| `MerkleNode<T>`, `ContentId`, `RawContentId`, `NodeStore`, canonical dag-cbor | `content-addressable` 0.1.2, already a workspace dependency | **the node, identities, and store trait.** Parents are a `BTreeSet`: canonical CID order, not presentation or insertion order. Structured values use `ContentId`; opaque bytes use `RawContentId` |
| `Op = concise \| elide \| generate`; `depthAfter: generate ⇒ d+1`; `Unit.wf: depth ≤ 1` | `formal/ContextOps/Basic.lean` | **the derivation laws.** Lean's abstract `addressed` field is not the Rust representation |
| `Unit`, `Derivation`, `RootEvent`, `UnitId`, `Packet`, `PacketId`; `verify_unit` | `agent-frame/src/{unit,derivation,root,packet,verify}.rs`, landed in #2246 | **the elision kernel.** `Unit` addresses `{ op, source, span, elided, root }`; `life` is outside that identity. `RootEvent` identifies a particular event. `Packet` already wraps `MerkleNode<PacketBody>` and admits a chain, not multiple parents. Construction and decode admission enforce elision-only/depth-zero rules; byte verification is separate (§6) |
| invariants §1–§8; `INVARIANTS.md` and `V0-DECISION.md` | historical records in the archived separate agent-frame repository, not paths in this checkout | **background obligations.** Numbered invariant references below refer to those records, not a fresh conformance count; the local code and monorepo decision define what shipped |
| `SpillStore` + `SpillProvenance::CompactionSpan` — content-addressed, redact-on-store, fail-closed on collision | `newt-core/src/agentic/content_spill.rs:296-298` | **storage for a live session.** `SessionSpillStore` keeps records in an in-memory `Mutex<HashMap<...>>`. It supplies no cross-process restoration path; D9 remains open |
| `FrameStore`, `SourceResolver`; `newt frame verify/explain/parents` | `newt-cli/src/frame_cmd.rs`, landed in #2246 | **verified reading of on-disk records.** Reuses kernel admission and verification, and reports unresolved roots. It does not write session history or restore a session's graph closure and authorization context |
| `adjudicate.rs` — one bounded, tool-less side call; `AdjudicationFailure` returned so the harness *tells* the operator | `newt-core/src/agentic/adjudicate.rs`, live at `newt-tui/src/chat.rs:6436` | **the auxiliary call shape.** The parse is **not** strict: `parse_adjudication_reply` strips a fence, then takes `find('[')`..`rfind(']')`, so prose on either side of the array is accepted. The "does NOT hunt for a decision inside prose" assurance is a doc comment the code does not implement (C3) — an explicit parser contract is owed |
| `BackendKind::Embedded` (#639), `BackendRef` | `newt-core/src/config.rs`; device selector at `newt-inference/src/embedded.rs:9` | **a separately addressable backend** and its override. Not automatically a *non-contending* one: it is CPU **by default**, not CPU-only — `NEWT_EMBEDDED_DEVICE = cpu\|metal\|cuda\|auto`, plus `embedded-metal` / `embedded-cuda` features — so "never contends with the primary" is a constraint D10 must specify and enforce, not a property inherited from the enum (C4) |

**What remains:** integrate observations, interventions and verdicts with the
existing kernel; record ordered projections; implement host-controlled
navigation and a bounded classifier; and provide the session lifecycle. These
are integration obligations, not permission to rebuild identities, elision,
packet admission or verification beside the implementations above.

---

## 1. The four layers

```
agent-frame  <->  host harness (assisted by a small LLM)  <->  context  <->  LLM
```

```mermaid
flowchart LR
    F[("agent-frame<br/>Merkle DAG of primitives<br/><i>what do I know, and how</i>")]
    H["host harness<br/><i>validate · navigate · record · render</i>"]
    K["auxiliary LLM<br/><i>propose relevance · classify replies</i>"]
    C["context<br/>this turn's projection<br/><i>a view, never the frame</i>"]
    M["LLM<br/>the harnessed model<br/><i>do the work</i>"]
    F <-->|"catalog + fetch / append nodes"| H
    H <-->|"bounded input / untrusted proposal"| K
    H <-->|"assemble / read back"| C
    C <-->|"prompt / reply"| M
```

**The host harness alone traverses and writes the frame.** Both models see
bounded inputs and return untrusted outputs. The auxiliary model proposes
relevant material and classifies replies through tool-less calls; it cannot
fetch, append, grant access or declare a cut legal. Host code computes legality,
validates proposals, records observations and renders the projection. The main
model's `re_read` request crosses the same host-owned boundary (D11).

---

## 2. One turn, end to end

```mermaid
sequenceDiagram
    participant F as agent-frame
    participant H as host harness
    participant K as auxiliary LLM
    participant C as context
    participant M as LLM
    H->>F: read catalog (node ids + one-line cards)
    H->>H: compute legal cuts and bounded candidate catalog (5b.1)
    H->>K: bounded candidates + task, no tools
    K-->>H: proposed relevant selection
    H->>H: validate access, bounds, pairs, unseen results, pinned message
    H->>F: fetch nodes the projection needs
    H->>F: record ordered projection + rendering inputs (3a)
    H->>C: render the recorded projection
    C-->>H: exact request prepared for dispatch
    H->>F: record request commitment bound to projection
    C->>M: prompt
    M-->>C: reply (or tool call)
    C-->>H: reply + projection record CID + antecedent node ids
    H->>F: append node { reply, parents: antecedents }
    F-->>H: reply CID — the observation is now recorded
    H->>K: recorded reply + antecedents — bounded, tool-less adjudication
    K-->>H: proposed answer / narration / question verdict
    H->>H: validate verdict schema and referenced reply
    H->>F: append node { verdict, parents: [reply CID], op: generate, depth: 1 }
    Note over H,F: harness-origin nodes are tagged and never enter summarizer input (2.4)
```

The diagram assigns ownership as well as ordering.

**Legality is computed and enforced by host code** (5b.1). The model chooses
relevance only within those constraints. Before any projection is sent, the
host checks the complete proposed selection: tool pairs stay atomic, unseen
results stay visible, the last operator message stays pinned, and access and
size limits hold. An invalid proposal is rejected before rendering; the host
does not repair it by silently dropping protected material or consult the model
to waive a rule. Rejection is surfaced and recorded. Any retry consumes the
separate navigation budget; its limits and termination policy remain owed in
§6b. Model output cannot authorize frame reads or writes.

**The verdict is a node with the reply as its parent** — which is what lets a
later reader ask "why was this turn recorded as done?" and get a CID, not a
`⚠` glyph.

**Record before adjudicate.** The reply is an *observation*. It is appended
before the adjudicator is called, and the adjudicator is handed the recorded
CID plus the antecedents it needs — it is not the operation that decides
whether the reply deserves to exist in the frame. Order the other way round and
an adjudicator that fails, times out, is cancelled, or returns unparseable
output takes the observation with it: the reply existed, influenced nothing
durable, and is absent from the accounted history. That is a provenance hole,
and it is the one an implementation agent would dig by following a diagram
literally. The failure behaviour that follows from the ordering:

| what fails | consequence |
|---|---|
| recording the projection or request commitment | a **storage/integrity** failure before dispatch. Do not send an unrecorded projection |
| appending the reply node | a **storage/integrity** failure. The turn does not proceed, and it is reported as storage failure — never as an adjudication outcome |
| adjudicator unavailable, timeout, cancellation, unparseable output | the reply node **stands**. The failure is itself surfaced and recorded per **D7** (`AdjudicationFailure`), never a silent fallback |
| — | there is no path that yields a verdict about an unrecorded observation: every verdict is parented by an already-recorded reply CID |

This is the designed lifecycle, not a description of live code; the residual
implementation obligations (interruption mid-turn, and the limits of replay over
redacted content) are in §6b.

---

## 3. The frame is a DAG; context is a projection

This is the proposed full integration graph. The shipped v0 `Packet` is a
single-predecessor chain; this diagram does not widen its admission rules. D1
must specify how these causal relationships reuse and extend the kernel.

```mermaid
flowchart TB
    G["G · genesis<br/>system + task<br/><i>no parents</i>"]
    U1["U1 · operator turn"]
    T1["T1 · tool call"]
    R1["R1 · tool result"]
    A1["A1 · assistant reply"]
    U2["U2 · operator turn"]
    E1["E1 · elide<br/>parents: [T1, R1]<br/>payload: CIDs + re-read directive<br/><i>op: elide, depth 0</i>"]
    G --> U1 --> T1 --> R1 --> A1 --> U2
    T1 -.-> E1
    R1 -.-> E1
    classDef proj fill:#0F7C8A22,stroke:#0F7C8A,stroke-width:2px
    class G,U1,E1,A1,U2 proj
```

Highlighted nodes are **this turn's selected material**: `{G, U1, E1, A1, U2}`.
The intended presentation order is `[G, U1, E1, A1, U2]`, which must be recorded
separately (§3a). The
elided pair `{T1, R1}` is still in the DAG; the projection carries `E1`, a
pointer to them with a re-read directive. Nothing was deleted — which satisfies
invariant 3.4's *name what was compacted with a re-read directive*.

**Retention is not the difference from compaction.** Today's compaction already
retains the span and already advertises a resolvable content-addressed handle
(§4, C1). What this picture adds over that is narrower, and it is worth stating
without inflation:

- **typed provenance across observations and derivations**, reusing the landed
  kernel; additional payloads and their relationship to its types remain D1;
- **a derivation constraint** (`op`, `depth`, parent set) that is *to be*
  checked at construction and at decode, so an elision cannot silently become a
  generation. The elision kernel already checks construction and admission;
  extending this across generated verdicts and retrieval edges remains owed —
  D5 is **OPEN**;
- **an auditable projection**: selected CIDs are necessary but insufficient.
  Ordered entries and rendering inputs must also be recorded (§3a).

A genesis node has no parents. That identifies graph topology, not whether an
invocation is fresh or resumed: an existing genesis node can itself be a resume
target. The run record must state the invocation mode and selected starting CID;
parentage cannot recover that choice. Hermeticity is a different and stronger
property — which inputs are admitted, and what ambient state the run may read —
and no arrangement of parents establishes it (C7). So `--hermetic` is not a claim
about where the graph starts; it is an execution policy that a run at genesis may
or may not be honouring. D6 states the two separately, and the admitted-input
contract it needs is still unwritten.

### 3a. What an auditable projection must record

A CID set cannot reconstruct a prompt. Merkle parents discard insertion order,
and the graph above does not order `E1` relative to `A1` or `U2`. The same selected
nodes can therefore produce different prompts without changing their IDs.

Before dispatch, host code must record the ordered entries actually rendered,
including message roles, referenced CIDs and any selected byte spans; the
renderer/template version and all rendering options; and the task, system and
tool-definition inputs that affect the request. It must bind this record to the
request dispatched to the backend and to the resulting reply. Canonical
structured records reuse `ContentId`; a commitment to exact serialized request
bytes uses `RawContentId`. Neither needs a new hash or canonical encoding.

The verification obligation is a cold replay: resolve the retained inputs,
render in recorded order with the recorded configuration, and compare with the
recorded request commitment. Reordering entries or changing a role, selected
span or template must be detectable. A commitment alone does not retain inputs,
and redacted or unavailable inputs must produce an explicit replay limitation,
not a claim of byte-perfect reconstruction. The protocol records what the host
sent; it cannot establish hidden provider-side prompt transformations.

This states the missing contract, not a new implemented record type. Its schema,
admission rules, storage and replay tests remain **OPEN** under D2 and must reuse
the inventory before choosing an encoding. Recording an ordered projection
does not make the existing `Packet` multi-parent or change `Unit` identity.

---

## 4. Compaction vs navigation — what actually changes

**Both sides retain the span.** The difference is in the *type system around the
handle*, not in whether the bytes survive.

```mermaid
flowchart LR
    subgraph today ["TODAY — compaction (span retained, handle untyped)"]
        S1["span<br/><i>redacted, staged in SpillStore</i>"] -->|"summarizer<br/>op: generate, depth 1"| SUM["summary"]
        SUM -->|"REPLACES in prompt"| P1["prompt"]
        S1 -->|"stage_compaction_span<br/>content-addressed handle<br/>fail-closed"| H1[("SpillStore<br/><i>session-scoped, not persisted</i>")]
    end
    subgraph design ["DESIGN — navigation (projective)"]
        S2["span<br/><i>retained in DAG</i>"] -->|"elide<br/>op: elide, depth 0<br/>verified by re-derive"| PTR["pointer + re-read"]
        PTR -->|"appears in"| P2["projection"]
        P2 -->|"re-read follows CID"| S2
    end
```

`newt-core/src/agentic/compress.rs:2398` `stage_compaction_span` stages the
**redacted** verbatim span into a `SpillStore` under
`SpillProvenance::CompactionSpan`, returns a content-addressed handle, and is
fail-closed: *"A failed store must never name a handle that resolves to nothing
(BHV-SPILL-001)."* Its own doc calls it "the one minting site" and says a second
encoding "would be a content-addressable law violation".

So **retention and re-read references are not what this design contributes** —
they exist. Rebuilding them under another name is the mistake this line has
already made once (#1786). What actually changes across the two panels:

| | today | design |
|---|---|---|
| span survives | yes, redacted, in `SpillStore` | yes, in the DAG |
| handle | content-addressed, one bespoke provenance variant | existing `UnitId` / `PacketId` where applicable; additional payload identities and causal links remain open under D1 |
| derivation recorded | implicit — the summary does not name its `op` or depth | elision already checked; general `op`/depth/edge validation at construction and decode remains D5 |
| durability | session-scoped; discarded at `/new`, gone at restart (C2) | owed — D9 is **OPEN** for exactly this reason |
| projection auditable | no complete projection/replay record here | owed — ordered entries, rendering inputs and a request commitment, with explicit replay limits (§3a) |

The honest incremental contribution is those four rows, not the first one.

---

## 5. What the frame contributes to #2239

```mermaid
sequenceDiagram
    participant H as harness (nudger)
    participant M as LLM
    participant K as classifier
    rect rgba(181,85,27,0.10)
    Note over H,K: TODAY, after PR 2251 (merged 2026-09-09, 44ff61c8)
    H->>M: "…if genuinely finished, say so in one sentence"
    M-->>K: "I'm finished — the answer above is complete."
    K->>K: Jaccard(reply, prototypes) ≥ 0.28, margin 0.03
    K-->>H: EITHER final_answer → Completed → Terminal::Completed
    K-->>H: OR narration → NarrationCapExhausted → Terminal::StoppedShort
    Note over H,K: StoppedShort scores outcome "model_error" (solve_contract.rs:119, :191)
    Note over H,K: REPORTING is honest now — but the loop still ACCEPTED the narration as the answer
    end
    rect rgba(15,124,138,0.10)
    Note over H,K: DESIGN
    H->>M: same nudge — recorded as node N (harness-origin, generate, depth 1)
    M-->>K: same reply — recorded as node R, parents: [N]
    K->>K: adjudicate(R, parents) — sees N is a scripted request
    K-->>H: compliance with a harness script is not a deliverable → verdict node, loud
    Note over H,K: still needs §6c: the compliant one-sentence reply is stamped Completed at the accept site, so it never reaches the variant PR 2251 fixed
    end
```

The classifier gets **the antecedent**: the nudge is recorded as harness-origin
material. A scripted self-report such as "I'm finished" must not substitute for
a substantive deliverable or verified task evidence. The host uses recorded
origin to exclude its interventions from summarizer input (invariant 2.4).
Parentage makes the intervention inspectable; it does not establish whether the
reply is substantive. That still requires classification and the separate
completion policy below. A genuine answer after the same nudge stays deliverable.

**This does not, on its own, fix #2239 (C6) — and #2239 is still OPEN.** The
reasoning has to be restated, because the half the frame was contrasted against
has since landed. When this section was first written, `terminal()` put
`NarrationCapExhausted` in the same bucket as `Completed`, so a perfect
classifier still yielded a scored success. **That is no longer the code.** PR
**#2251** merged 2026-09-09 (`44ff61c8`) and moved `NarrationCapExhausted` in
with `RoundCap` / `Empty` / `Cancelled` → `Terminal::StoppedShort`
(`solve_contract.rs:119`), scored as `model_error` (`:191`). Reporting is honest
now.

What #2251 did **not** do is the reason #2239 remains open, and it is the part
the frame speaks to: **the loop still accepts the rescue nudge's narration as
the turn's answer.** `NarrationCapExhausted` is still produced at four accept
sites in `newt-core/src/agentic/mod.rs` (3048, 4816, 7051, 9025 on
`origin/main`) — #2251 changed how that outcome is *reported*, not whether it
happens. So the split is no longer "evidence half vs outcome half"; it is:

| half | state |
|---|---|
| **reporting** — an exhausted rescue is not filed as a completion | **landed**, #2251 |
| **accept site** — the harness stops scoring text it dictated as the answer | **open**; the proposed frame records evidence for that decision (§6c), but the fix can proceed independently |

The frame improves the **evidence** available at the accept site. It does not,
and never did, fix the reporting; that was independent and shipped first.

One constraint the frame must respect, stated here because it is easy to get
backwards: **ancestry is evidence about causality, not a completion oracle.** A
genuine answer that happens to follow a nudge stays deliverable. "Parent is a
harness node" is an input to adjudication, never by itself a verdict of
incompleteness.

---

## 6. The depth bound

```mermaid
flowchart LR
    R0["root<br/>depth 0"] -->|"concise / elide"| D0["depth 0<br/>asserts nothing new"]
    R0 -->|"generate"| D1["depth 1<br/>e.g. a verdict, a summary"]
    D1 -->|"generate"| D2["depth 2"]
    D2 --> BAD(("✗ Unit.wf"))
    style BAD fill:#B5551B33,stroke:#B5551B
```

`Unit.wf: depth ≤ 1`. A harness verdict over a reply is depth 1. A summary of a
summary is depth 2 and is **illegal by the design law** — which is the
derivation-depth bound this line has already concluded is the only novel claim
in the context-management literature it surveyed.

The **law** is machine-checked in `formal/ContextOps/Basic.lean`. **Runtime
enforcement exists for the narrower elision kernel:** `Unit::seal` refuses
`Concise` and `Generate`, computes depth zero and checks the source span;
`TryFrom<RawUnit>` enforces elision, matching depth and a nonempty span. A unit
cannot bypass admission through ordinary `Deserialize`. `verify_unit` separately
checks the source hash, actual byte bounds and elided-content hash. Admission
alone does not verify those bytes, and neither operation resolves the root or
establishes whether source bytes were themselves generated.

**The general bound remains unimplemented.** A reply/verdict pipeline cannot
use `Generate` through the current v0 admission boundary. Its edge semantics and
checks must be specified at construction and decode before widening that
boundary. Across retrieval, observing an access must not reset the retrieved
artifact's origin or depth (§6b, "depth laundering"). D5 stays **OPEN** for
those extensions, not for a missing elision check that has already shipped.

---

## 6b. RETROSPECTIVE — claims the failed revision made that source does not support

**These claims are no longer live anywhere in this document.** The body above
has been rewritten to the verified behaviour; this section is kept as the
accounting of how a revision of this file came to assert seven things the code
does not do, and as the standing list of what an implementation still owes.

Each row was checked against `origin/main` by opening the file, and re-verified
on 2026-09-09 during the consistency pass. "Obligation" is what an
implementation still owes. The "corrected in" column names where the live text
now says the right thing, so a reader can check that the retraction actually
took.

**One row's evidence has since been superseded by a merge, not by a rewrite.**
C6's cited mapping was accurate when observed and is now historical — #2251
landed later the same day. It is dated in place rather than deleted, because the
distinction matters to a reader: a claim withdrawn *because the code was fixed*
is not the same as a claim withdrawn *because it was never true*. C6 is the
second kind, with evidence of the first kind attached; the row says so
explicitly. No other row's evidence has changed.

| # | The claim the failed revision made | What source says | Corrected in | Obligation |
|---|---|---|---|---|
| C1 | Today's compaction **drops** the span; navigation would retain it | `compress.rs:2398` `stage_compaction_span` already stages the redacted span into `SpillStore` and advertises a content-addressed handle, fail-closed (BHV-SPILL-001) | §3 (contribution restated), §4 (diagram + table) | Restate the contribution as typed provenance + derivation constraints + auditable projection, **not** retention. Do not rebuild recovery infrastructure that exists |
| C2 | `SpillStore` is the frame's storage; `--resume <cid>` resumes a session | `SessionSpillStore` is `Mutex<HashMap<String, SpillRecordV1>>`; the module doc states the store "was NEVER persisted, so an old handle could not survive a process/session restart" | §0 inventory (`SpillStore` row), §4 table (durability row) | Either name a durable path that restores the graph closure, schema **and** authorization context after restart, or scope resume/navigation **explicitly to a live session**. Do not promise cross-process resume on ephemeral state |
| C3 | The adjudication side call has a **strict** parse | `adjudicate.rs:58` does `find('[')` / `rfind(']')`, so prose on either side is accepted. The "does NOT hunt inside prose" assurance is a *comment*, not the code | §0 inventory (`adjudicate.rs` row) | State the parser's actual accepted language and choose an explicit contract. Any production parser change is a **separate** PR with its own tests |
| C4 | Route the adjudicator to an "auxiliary / CPU-local" backend so it never contends with the primary | `newt-inference/src/embedded.rs:9` is **CPU by default, not CPU-only** — `NEWT_EMBEDDED_DEVICE = cpu\|metal\|cuda\|auto`, with `embedded-metal` / `embedded-cuda` features | §0 inventory (`BackendKind::Embedded` row) | D10 must specify placement constraints, timeout ownership, cancellation propagation, unavailable-at-startup behaviour, and an explicit fallback. An override must not silently violate placement |
| C5 | A third class "cannot be carried" by the Jaccard matcher because the margin is 0.03 | A winner/runner-up margin does not bound how many classes a classifier can represent. The reasoning is invalid | not present in live text; D13 rationale withdrawn | Remove the impossibility claim. Justify model-backed classification as an **empirical hypothesis** about discrimination and context-sensitivity, with a comparison plan against the deterministic baseline |
| C6 | Fixing the classifier (or adding causal parentage) fixes #2239 | **Evidence as observed, pre-#2251 (`origin/main` at `e3f42a36`, 2026-09-09):** `solve_contract.rs:106-115` mapped `NarrationCapExhausted` into `Terminal::Completed` — the same bucket as a genuine completion, so a correct classifier still yielded a scored success. **That mapping is HISTORICAL:** `44ff61c8` (#2251, merged 2026-09-09T14:26:41Z) moved it to `Terminal::StoppedShort` / `model_error`. **The claim in column 2 is still false**, and was withdrawn because it was *wrong*, not because it was *fixed* — the classifier was never the whole defect, and the accept site is still open (§6c residual 1) | §5 (restated for the post-#2251 code), §6c | Reporting half **landed** in #2251. Still owed: the accept site (§6c residual 1), the `NarrationFinalRound` decision (residual 2), TUI behaviour (residual 3). #2239 remains **OPEN** |
| C7 | `--hermetic` "reduces to always genesis" | A genesis node establishes a graph **origin**. It says nothing about admitted inputs or ambient state | §3 (genesis ≠ hermetic) | Narrow the claim: specify admitted inputs and ambient-state assumptions separately from parentage |

**Integration obligations still open:**

- **Ordered projection and host/model protocols.** §3a states what must be
  replayable, but the record schema and admission/replay tests remain owed.
  §2 assigns legality to host code; navigation proposal grammar and rejection,
  cancellation and termination tests remain owed. Neither is settled by
  correcting the diagram (D2/D3).
- **Bounded navigation as a workflow** (not just a bounded call): per-request
  catalog size, fetched bytes, dereference count, auxiliary calls, retries,
  total time, and a deterministic no-progress condition. A catalog that emits
  one card per historical node does not survive history growth.
- **Oversized unseen results.** Following a pointer can yield more than fits
  while unseen-result protection forbids eviction. Needs bounded slices or
  continuation with truthful completeness metadata — never a silent truncation
  presented as a complete reread.
- **Depth laundering through retrieval.** Observing that a retrieval happened
  must not reset the retrieved artifact's origin or depth. The depth-one rule
  needs a stated edge relation and validation at **both** construction and
  decode.
- **Record-before-adjudicate: ordering settled, edges owed.** The *ordering* is
  now stated normatively in §2 — the observation is appended first, the
  adjudicator is handed the recorded CID, and adjudicator failure cannot erase
  the reply. What an implementation still owes: behaviour when the turn is
  **interrupted or cancelled between the append and the verdict** (the reply
  node stands with no verdict — a reader must be able to tell that state from
  "adjudicated as an answer"), and the limits of replay: **no claim of
  byte-perfect replay over redacted content** survives, because the span the
  handle points at is redacted (C1).

## 6c. The real defect behind #2239 — the reporting half has landed, the accept site has not

**The reporting half is fixed.** PR **#2251** merged **2026-09-09T14:26:41Z** as
`44ff61c8`, verified present on `origin/main`. **Issue #2239 is still OPEN**, and
this section is the accounting of why.

What the code does **now**, `newt-cli/src/solve_contract.rs:106-119`:

```rust
match end_reason {
    Some(
        reason @ (TurnEndReason::RoundCap
        | TurnEndReason::Empty
        | TurnEndReason::Cancelled
        | TurnEndReason::NarrationCapExhausted),
    ) => Terminal::StoppedShort(reason),
```

and at `:191`:

```rust
Terminal::StoppedShort(TurnEndReason::NarrationCapExhausted) => "model_error",
```

So an exhausted narration rescue is **`Terminal::StoppedShort`, scored
`model_error`** — no longer in the same bucket as a genuine completion, and
`status_label` puts every `StoppedShort` in `"incomplete"` (`:213`).

**What it used to do, and when.** Before `44ff61c8` — that is, on `origin/main`
up to and including `e3f42a36`, observed 2026-09-09 — the same site read:

```rust
Some(
    TurnEndReason::Completed
    | TurnEndReason::NarrationCapExhausted
    | TurnEndReason::NarrationFinalRound,
)
| None => Terminal::Completed,
```

Exhausted narration was classified as an ordinary successful completion. That is
kept here dated rather than deleted, because it is the observation the correction
was built on and a later reader needs to be able to tell a claim withdrawn
*because it was fixed* from one withdrawn *because it was wrong*.

**Three things #2251 explicitly did not do.** Each is a reason #2239 stays open,
and none is closed by this design document either.

1. **The accept site.** The loop still accepts the rescue nudge's narration as
   the turn's answer. `NarrationCapExhausted` is still produced at four sites in
   `newt-core/src/agentic/mod.rs` (3048, 4816, 7051, 9025 on `origin/main`).
   #2251 changed how that outcome is **reported**, not whether it happens. This
   is issue ask #4, a separate change in `newt-core`, and it is the half this
   frame would make auditable; fixing it does not require waiting for the frame.
2. **`NarrationFinalRound` was deliberately not moved with it.** It still maps to
   `Terminal::Completed` (`solve_contract.rs:126-127`). The comment at `:120-125`
   states why: it is "a different exit — the round limit arrived while the model
   happened to be narrating — and no report stands behind reclassifying it… a
   separate decision with its own bench-row consequences, not a wildcard to sweep
   along with this one." Do not treat that arm as an oversight; it is an open
   decision with a bench-row cost.
3. **TUI behaviour is outside #2251's scope.** The fix is in `newt-cli`'s
   contract mapping. What the interactive surface shows for an exhausted rescue
   was not touched and is not settled here.

The remaining fix is in completion semantics and needs three concepts kept
apart:
**response classification** (is this text an answer, narration, or a question),
**control outcome** (continue, await operator, deliver, terminate incomplete,
cancel, fail), and **task-success evidence** (did the claimed external actions
actually happen). Delivering a response is not evidence that a claimed
repository modification occurred.

Two constraints on that fix:

- A genuine answer that follows a nudge **stays deliverable**. Ancestry is
  evidence about causality, not a completion oracle.
- Exhausted rescue must **preserve prior observations** and surface the actual
  incomplete outcome, rather than discarding content or asserting completion.

**Why residual 1 is the larger half — measured, and reported in #2251 itself.**
Probing the OpenAI loop with a scripted reply to the rescue nudge, the recorded
`end_reason` is:

```
Completed              <- "Yes, I am finished."
Completed              <- "I am genuinely finished."
Completed              <- "I'm finished."
Completed              <- "Done."
Completed              <- "Yes — finished. The answer is three."
NarrationCapExhausted  <- "I'm finished — the answer above is the complete
                           deliverable; there is nothing to edit or run."
```

The nudge asks the model to "say so explicitly in one sentence"; the compliant
one-sentence replies never reach `NarrationCapExhausted` at all — they are
stamped `Completed` at the core accept site. #2239's transcript was the *verbose*
phrasing, the only one that trips the bag-of-words matcher into the warning.

The consequence, stated in #2251's own body: the landed mapping **"makes the
reported failure honest, but reaches a minority of the affected turns."** The
majority of exhausted-rescue turns are still stamped `Completed` before the
mapping ever sees them, which is exactly residual 1. Both halves are downstream
of one mistake: **scoring text the harness itself dictated.**

Note the lockstep hazard: PR #2242 (merged 2026-09-08) pins emitted outcome
values against `newt-cli/contract/bench_outcome_values_v1.txt`. A change to an
emitted outcome string must update that permitted set in the same PR.

## 7. Decisions

Statuses below were revised after the review in §6b. **CHOSEN** means the
operator picked the option and it stands; it does not mean the semantics,
failure behaviour and verification obligations are all specified. **SETTLED**
means those are specified for the stated rule; it does not mean its integration
is implemented. **RESTATED** means the decision stands but its
*claim* was rewritten because the original was not supported. **NARROW** means
an over-broad claim was withdrawn and the decision now says less than it did.
**OPEN** means work is owed before implementation. A corrected wording never by
itself moves a decision to a stronger status.


| # | decision | grounded in | status |
|---|---|---|---|
| **D1** | Reuse `agent_frame::Unit` / `Derivation`, `RootEvent` and `Packet` identity and admission boundaries. A unit addresses source material and a span; `life` does not change its derivation identity. Additional payloads and causal links must extend these abstractions using `content-addressable`, with an explicit relationship to v0's single-predecessor packet chain. A separate `MerkleNode<Primitive>` representation is not settled by the Lean fields alone. | `agent-frame/src/{derivation,unit,root,packet}.rs`; #2246 | **RESTATED** — reuse boundary established; full payload and edge contract **OPEN** |
| **D2** | Context is an ordered projection of the frame. The host must record ordered entries, roles, selected spans, rendering inputs and a request commitment (§3a); a CID set alone cannot establish reconstruction. The intended DAG is retained within a session. The contribution is uniform provenance and auditable projection over existing retention and elision. | invariants 3.1, 3.4; `MerkleNode` parent-set semantics; §3a | **RESTATED** — schema, admission and replay tests **OPEN**; durability D9 |
| **D3** | Deterministic host code owns traversal, access, cut legality, storage admission and rendering. The auxiliary LLM supplies untrusted relevance proposals and verdicts through bounded tool-less calls. Host validation rejects invalid proposals before dispatch (§2); neither model can waive constraints. | `adjudicate.rs` (#1749) call shape; §2 | **RESTATED** — ownership specified; proposal/parser and bounded navigation contracts **OPEN** (C3, §6b) |
| **D4** | Host-recorded interventions (nudges, adjudications, verdicts) carry explicit harness origin and are **never** summarizer input or evidence of model completion. Reuse `RootEvent` to name the triggering event; `life` remains lifecycle, not an origin tag. D1 must specify the payload representation. | invariant 2.4; `agent-frame/src/{root,unit}.rs` | **SETTLED** as an exclusion rule — does not fix #2239 alone, C6 |
| **D5** | `depth ≤ 1` is the intended full-frame law. V0 already enforces elision at depth zero at construction and decode admission, and `verify_unit` checks the addressed material. It does not establish the source's own provenance or preserve generation depth across retrieval. Generation, its edge relation and construction/decode checks remain owed. | `formal/ContextOps/Basic.lean`; `agent-frame/src/{unit,verify}.rs` | **OPEN** for the full frame — v0 elision enforcement landed in #2246 |
| **D6** | **Graph origin, invocation mode and execution policy are separate.** A fresh execution begins at genesis; `--resume <cid>` selects an existing node, which may itself be genesis. The run record names the mode and starting CID instead of inferring them from parents. `--hermetic` constrains admitted inputs and ambient state; genesis alone proves neither. Mutual exclusion of `--hermetic` and `--resume` at parse time remains a **product constraint for the first implementation** (operator ruling 2026-09-08), not a consequence of parentage. The admitted-input and ambient-state contract is still unspecified. | operator ruling 2026-09-08 (mutual exclusion); C7; §3 | **NARROW** — hermetic-input contract **OPEN** |
| **D7** | Adjudicator unavailable ⇒ `AdjudicationFailure` surfaced to the operator and recorded as a node. **Never** a silent fallback to Jaccard. A re-read whose CID is absent fails closed — absence is a finding. | `AdjudicationFailure`, invariant §8 | **SETTLED** — failure path owed for storage |
| **D8** | Host code computes legal cuts and validates the complete proposed selection: tool-pair atomicity, unseen results never elided, last operator message pinned. A model proposal cannot override these checks (§2). | invariants 5b.1, 2.3, 3.2 | **SETTLED** as host-enforced constraints; implementation remains owed |
| **D9** | Reuse the live-session `SpillStore` and disk-reading/verification boundaries in `newt frame`. The kernel remains a storage-free library. Durable session recording and resume still need a writer and restoration of graph closure, schema and authorization context; existing disk inspection does not supply them. | `content_spill.rs`; `frame_cmd.rs`; #2246 monorepo decision | **OPEN** — session durability and restoration owed |
| **D10** | The narration adjudicator runs on the **auxiliary / CPU-local backend** (`BackendKind::Embedded`, or the summarizer's CPU-local default), with a `BackendRef` override, and the run manifest records which adjudicator judged the turn. It never contends with the primary model or the round budget. The reason `config/shell.rs:113` gives for keeping *intake* adjudication on the steering model — *"adjudication reads operator intent, which is the steering model's own job"* — does not transfer: narration classification is mechanical classification of model output, which is the summarizer's kind of work. | `config/shell.rs:113`, `BackendKind::Embedded` (#639), `BackendRef` | **CHOSEN** — placement/fallback owed, C4 |
| **D11** | The main LLM **gets one `re_read(cid)` tool**. Retrieval is not harness-only: invariant 3.4's *re-read directive* is addressed to the model, so the model needs the affordance to act on it. **It is a mediated capability, not frame-traversal authority**: the model *requests* a CID, the harness validates and bounds the request, the access happens through harness-owned machinery, and the result is recorded and projected back. The model never gets to walk the graph, and an absent CID fails closed (D7). Results are appended to the frame as nodes whose parent is the pointer that was followed, so a retrieval is itself provenanced and a later reader can see what the model chose to re-read. | invariant 3.4 | **CHOSEN** — bounds owed (§6b) |
| **D12** | The adjudication side call **does not count against the round budget** — it is harness work, not model work, and charging it would penalise an arm for harness overhead and make round-cap comparisons across harnesses unfair. It **must** be declared in the run configuration and recorded in the contract record, or two runs are not comparable. This is #2227's "verify the instrument" applied to the classifier. | #2227 | **CHOSEN** — separate enforced budget owed |
| **D13** | The **`question` class ships** as a third verdict alongside answer / narration: a reply that asks the operator something is nudged to *ask*, not to *do*. This is #1020's original complaint (the model offered "Option A, B, or C?" and was nudged to act). The earlier rationale — that a Jaccard matcher "structurally cannot carry" a third class because its margin is 0.03 — is **withdrawn**: a winner/runner-up margin does not bound how many classes a classifier can represent (C5). Whether a model-backed classifier discriminates better is an empirical question, and it needs a comparison against the deterministic baseline rather than an impossibility claim. | #1020 | **CHOSEN** — C5 rationale withdrawn |

---

## 8. What this is NOT

- **Reuse the existing kernel and identities.** `Unit`, `Packet`, `MerkleNode`
  and the CID profiles already exist. D1 owes their integration with the new
  payloads, not a parallel implementation. Disk inspection also exists, while a
  durable session writer and restoration lifecycle remain open (D9).
- **Not a smarter summarizer.** The summarizer is demoted, not improved.
- **The smart-harness integration remains in the 0.9.0 stream.** The elision
  kernel and forensic CLI have already landed in #2246. The monorepo decision
  sets **0.1.0** as the kernel's graduation to a separate public crate; the old
  separate-repository 0.0.1 gate no longer applies. The earlier priorities stand: fix #979 so
  a turn cannot hang, fix #2239 so the harness stops reading its own script as
  evidence, and **spend nothing further on the compaction pipeline**.
- **Not a rewrite.** Every settled row names the thing it reuses — and §6b
  records, as a retrospective, where that reuse claim was wrong.

---

## 9. What "properly accounted" means here

The intended integration records every observation and intervention with a CID,
and names each derivation's source and depth. The bench record (#2227) names
which adjudicator judged which turn. A cold reader can re-derive an elision from
retained source bytes today; full projection replay requires the ordered record
and inputs in §3a. Generated verdicts still need the verification contract in D5.
An address makes a claim inspectable; it does not prove that claim true, retain
missing inputs or establish that a task succeeded. Those limits are part of
being properly accounted.
