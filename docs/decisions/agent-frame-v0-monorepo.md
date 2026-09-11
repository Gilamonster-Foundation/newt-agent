# agent-frame v0 is incubated in this repository

**Status:** accepted, 2026-09-08 · **Supersedes:** the separate-repo plan in the
private `Gilamonster-Foundation/agent-frame`

## Decision

`agent-frame` v0 lives here as a workspace member (`agent-frame/`), with its
machine-checked core at `formal/` and a CI job that builds it. It graduates to
its own public repository and crates.io at **0.1.0**.

## Why not the separate repo

The separate repo could not ship, for a reason no engineering step there could
clear. Its `docs/EMBARGO.md` binds *publication*, not merely text:

> This repository does not become public, and no crate built from it publishes
> to any registry, before the prior work's own disclosure.

That disclosure waits on an external party. A visibility flip, a text scrub, and
a history rewrite are all equally blocked by it, because the binding is on the
act of publishing rather than on any particular file. The repo also has **zero
lines of Rust** — there was no crate to release as 0.0.1 even if the gate were
open.

**The code was never what was embargoed.** The separate repo's own decision
record settles it:

> **Can agent-frame be built without the embargoed material? — Yes, completely.**
> What v0 loses: a footnote, and a private map of potholes someone already
> walked. No theorem, no invariant, no API decision, no priority claim.

So the kernel is written here, fresh, from the design record. What stayed behind
is prose — the embargo document itself, an architecture paragraph naming the
prior work's domain, and a history that is permanently reachable from that
repo's main. None of it is load-bearing for the crate, and none of it came
across. `formal/` was scanned for the six forbidden categories before the copy
and is clean.

The private repository is **archived, not deleted.** It holds the invariant
ledger, the embargo audit, the v0 decision record and the non-vacuity check —
the reasoning behind this crate. Deleting it would buy tidiness at the cost of
the record we will want when clearance lands.

## What v0 is

A library. No daemon, no socket, no async, no wire format, no storage — plus one
consumer, `newt frame`, which is a CLI over the library and adds no capability
to it.

**A unit's address is self-validating.** That is the property the crate exists
to provide, and it is the one the first revision did not have (see the
correction below). A `Unit`'s content id is the id of its `Derivation` —
`{ op, source, span, elided, root }` — so:

* two elisions over **different sources** get different ids;
* two elisions over **different ranges of one source** get different ids;
* given the source bytes, a consumer who did not run the build recomputes all
  three claims and compares. That is `agent_frame::verify_unit`, and it is
  `checkOf .elide = .rederive` with an implementation behind it.

**Identity is the derivation, not the lifecycle.** `life` is outside the hash:
superseding a root is a fact *about* a derivation, not a different one, and if
it moved the id then marking a unit superseded would fork every reference to it.

**A root is an addressed event, not a category.** `RootKind` is still the three
kinds and still has no `ModelOutput` — a model's assertion is never a root — but
the kind is now a field of a `RootEvent` that has its own content id, and a unit
references that id. "Caused by an operator prompt" became "caused by *this*
operator prompt", which is the question a forensic reader actually asks. The
event carries a session-local `seq` so the same words typed twice remain two
turns.

**The admission boundary is closed.** `Unit` does not derive `Deserialize`.
Foreign bytes decode to a transparent `RawUnit` and become a `Unit` only through
a fallible `TryFrom` that enforces the v0 contract: elision only, declared depth
equal to the depth the operation implies, and a span that covers at least one
byte. Whether the span lies inside the *real* source needs the bytes, so it is
`verify_unit`'s job, not admission's — the two are tested apart.

**v0 mints elision only**, now on both paths. `Op::Concise` and `Op::Generate`
exist in the type — the tables mirror the Lean exhaustively, with no wildcard
arm — and both `Unit::seal` and the admission boundary refuse them.

The refusal is principled rather than not-implemented-yet. `checkOf .concise =
.ground` leaves concision inside the fence with a verification class that has no
implementation and no calibrated threshold; a mintable unit whose check nobody
can run reproduces *"unverified evidence is indistinguishable from no evidence"*
inside the product.

## Correction — the first revision's kernel did not do what this record claimed

Two independent reviews found the same two defects, and both are confirmed. They
are recorded here rather than edited away, because a decision record that
quietly repairs its own claims is worth less than one that shows where it was
wrong.

**C1 — the address named no material, so it distinguished nothing.** The first
`Unit` was `{ op, depth, root: Option<Root>, life, addressed: bool }` and its id
was the hash of exactly that. No field names any source. Every fresh elision
with the same root kind therefore serialised identically and minted **the same
content id**, regardless of what it elided. Measured directly against that
struct:

```
OLD  elision over "the licence header text"      = bafyr4ih3u5g6dgnd…zts47fm
OLD  elision over "a completely different …"     = bafyr4ih3u5g6dgnd…zts47fm   <- identical
NEW  elision over "the licence header text"      = bafyr4iezckxz767…s7yzbi
NEW  elision over "a completely different …"     = bafyr4ibj7egunem…zgjtsvy
```

`addressed: true` was a **declaration that a unit was addressed, not an
address** — nothing could be fetched from it. So `Rederive` had nothing to
re-derive, and the word was decoration. `agent-frame/tests/addressing.rs` is the
regression suite; every test in it is unwritable against the old type, which is
the defect stated another way.

**C2 — `depth <= 1` was not unreachable.** This record and the crate's own docs
said the bound could not be broken because `seal` was the only constructor and
every field was private. `Unit` derived `Deserialize`. Decoding
`{"op":"generate","depth":0,…}` produced a `Unit` the constructor would have
refused, so the mint was one path of two and the decoder was the unchecked one.
The claim held for the half nobody attacks. `agent-frame/tests/admission.rs`
drives each refusal.

**C3 — the ADR cited the content-addressable first-principle rule while breaking
it.** `Packet { prior: Option<PacketId>, units }` hashed over its whole body is
a hand-rolled Merkle node: a payload plus a parent link, with identity over
both. The rule this record invoked says every persisted structure derives its
identity through `content-addressable`. A packet is now
`MerkleNode<PacketBody>`, the predecessor is a parent link, and genesis is the
empty parent set — the same fact the old `prior: None` encoded, in the
vocabulary the rest of the line speaks.

**C4 — `UnitId` and `PacketId` were type aliases for `ContentId`,** so they were
one type and a packet id fitted a unit slot with nothing to say about it. They
are newtypes now.

**C5 — C2 was fixed on `Unit` and left open on `Packet`.** Closing the unit's
decoder while `Packet` still derived `Deserialize` fixed one instance of a
defect class rather than the class. `Packet` is a `MerkleNode`, which carries a
parent **set**; v0 mints a **chain**, and the referent's `Chain` is
`genesis (units)` or `sealed (prior : Chain) (units)` with no multi-parent
constructor. Foreign bytes could hand a packet two parents, and the result
satisfied both halves of a contradiction at once:

```
parents      = 2
is_genesis() = false        <- it has parents
prior()      = None         <- ...but not exactly one, so no link is named
```

Neither `genesis` nor `following` can mint that. The decoder could, and the
consequence reached the forensic surface: `newt frame parents` reported
`outcome: "genesis"` — *"no parents: this frame is an origin, and the chain ends
here"* — of a packet with two. That is the **three outcomes, never conflated**
table in *Forensics is a first-class surface* below, violated in its worst
form. An absent parent meaning "origin" and one meaning "I could not resolve it"
are the two this record set out to keep apart; this was a third, worse than
either: *"I dropped the links I did resolve."*

The repair is the one `Unit` already had. `Packet` no longer derives
`Deserialize`; bytes decode to `RawPacket` — deliberately the `MerkleNode`
itself, not a fresh DTO, since a raw packet **is** an untrusted DAG node — and
`TryFrom<RawPacket> for Packet` admits zero parents or exactly one, refusing
anything more with `PacketAdmitError::NotAChain`. `agent-frame/tests/admission.rs`
drives it across the real `serde_json` boundary, and
`newt-cli/tests/frame_cli.rs` proves the store now refuses a two-parent node
that is legitimately addressed and filed under its true id — so the store's own
id check passes and only admission can object.

**What admission proves about `root`, and what it does not.** Admission
establishes that `root` is present and is a well-formed dag-cbor `ContentId`.
It does not establish that the id resolves to a `RootEvent`, and neither does
`verify_unit` — a unit naming an id nothing ever minted admits and verifies
exactly like one naming a real event, which
`neither_admission_nor_verification_resolves_the_root` makes executable.
Resolving an id needs a store, and giving the kernel one to answer a structural
question is v0 growing the storage service its scope refuses. **The store owns
it**: `newt frame` reports `root_kind` / `root_seq` / `root_content` when it
resolves and `root_unresolved` when it cannot — never a silent absence, the
same discipline `parents` applies to a missing link, and now covered by
`a_root_that_does_not_resolve_is_reported_not_silently_dropped`.

Read against the referent this is a **weakening paired with a strengthening**:
`fabricated u := u.root.isNone` is unrepresentable in Rust because `root` is
mandatory, and what replaces the orphan is a root that may *dangle* — which the
Lean model cannot express, its `Root` being an inhabitant rather than a
reference.

## Forensics is a first-class surface

The operator requirement is that a consumer must be **able** to establish which
source material a unit represents and how it was derived — with the harness's
own tools, also reachable by a human. So the verification decision lives in
exactly one function, `agent_frame::verify_unit`, and `newt frame` calls it:

```
newt frame verify  <cid> [--source FILE] [--frame DIR] [--json]
newt frame explain <cid> [--frame DIR] [--json]
newt frame parents <cid> [--frame DIR] [--json]
newt frame replay  <request-cid> [--frame DIR] [--json]
```

The smart harness stores canonical `<cid>.cbor` records and raw source files.
Forensic reads compose with `agent_harness::store::FrameStore`, including its
unit admission and verified content-addressed reads. Earlier `<cid>.json`
fixtures remain readable only when the canonical record is absent; a damaged
canonical record cannot fall back to a JSON twin. New writes use canonical CBOR.

`replay` reconstructs a request from its projection and verified source closure,
then checks the recorded dispatch commitment and exact stored request bytes.
It writes the exact request bytes without a newline, or a receipt containing
the same body with `--json`. A missing or substituted input fails before stdout
receives request bytes. Replay reads evidence without granting session authority.

`verify` **exits non-zero on mismatch**, so a script can gate on it.
`newt-cli/tests/frame_cli.rs` asserts the CLI reaches the same verdict as the
library on both the passing and the failing case — a wrapper that only agrees
when things pass is two verifiers.

## One link is the whole obligation

**The verification obligation is one step.** Given a unit, establish which
source it names, what span of it, which operation and which root event — and
check *that* derivation by recomputing it against the source bytes. Whether the
source is itself a derived thing is the source's business. `verify` does not
resolve the source's ancestry and does not fail because the source is derived.

**This is complete, not merely pragmatic, and the reason is the depth bound.**
`Unit.wf` says `depth <= 1`: nothing is more than one derivation away from
source material. A frame with no parents is genesis, the defined bottom. So
there is no deeper chain to regress into **by construction** rather than by
policy — the forensic obligation and the depth bound are the same constraint
seen from two directions. That is also why refusing `concise` and `generate` in
v0 is not merely caution: it is what keeps regress impossible.

**No walker ships.** An earlier draft of this work built a walk-to-genesis loop
for `trace`; it was removed rather than left unused, because dead generality is
the thing this line is trying not to ship, and an unbounded walk over data we do
not control is a promise that terminates only if the data is well-formed —
which is precisely what forensics may not assume.

**Regress is still fully available, by composition.** `verify` reports the
source id it checked against, so the next link is the caller's one-liner:

```
newt frame verify $(… --json | jq -r .source)
```

The recursion lives in the caller. Nothing forbids depth; we simply do not
implement a walker on anyone's behalf. No `--depth` parameter is exposed — if
one is ever added it must be a countable integer defaulting to 1, never a
`--recursive` boolean, and it must keep the three outcomes below distinct.

**Three outcomes, never conflated.** `newt frame parents` reports the
*immediate* link and nothing further:

| outcome | meaning | how it is reported |
|---|---|---|
| `genesis` | no parents; the chain ends here | `outcome: "genesis"`, stated in the data — **not** an absent field |
| `parent` | one link, named but not followed | `outcome: "parent"` plus the id to call again with |
| unreadable | a subject that should exist could not be read | **not an outcome value at all**: a non-zero exit and a message on stderr |

An absent parent meaning "origin" and an absent parent meaning "I could not
resolve it" are different facts, and rendering them alike is the forensic
version of silent truncation. `parents_of_a_genesis_packet_says_genesis` and
`an_unreadable_subject_is_an_error_not_a_genesis` hold that apart.

One sharp edge, recorded because a caller will hit it: a **source** id is
`raw`-profile (opaque bytes) and a **unit** id is `dag-cbor`-profile. They are
different types on purpose, so piping `.source` back into `verify` is not
automatically one link deeper — if a source is itself a derived artifact, the
unit that derived it has its own separate id, and mapping between them is the
caller's step. The report carries `source_profile` so that is visible rather
than discovered through a parse error.

Per CRAFT-20 each subcommand builds one `Serialize` report and either prints it
as JSON or renders it; the human form is a projection of that report rather than
a second assembly of facts, and a test asserts every id in the JSON also appears
in the rendered form.

**The store is a directory of content-addressed files**, and it honours the two
id profiles `content-addressable` mints. A structured node (`<content-id>.json`)
is read back through decode → admit → recompute the id → compare to the
filename; the stored bytes are never trusted. Source bytes
(`<raw-content-id>`) come back **unverified**, because hashing them is exactly
what `verify_unit` does — the `NodeStore::get` / `get_unverified` split, kept
because collapsing it would make the mismatch case unreachable.

`NodeStore` itself is not used for sources: it is keyed by `ContentId`
(dag-cbor), and a source is an opaque byte string keyed by `RawContentId`
(raw). The two are deliberately different types with no conversion between them,
so `SourceResolver` is the second profile's lookup rather than a second copy of
the first.

**Still elision-only.** `concise` and `generate` remain unmintable and
inadmissible; their verification classes still have no implementation, and
nothing here changed that.

## Consequences

**It is a LEAF, and that is enforced.** `agent-frame/tests/no_service_edge.rs`
asserts no HTTP or inference dependency edge (invariant 7.4's static half,
landed day one so the crate stays daemon-able for free) and pins the closure at
a measured ceiling of 40. Living in this workspace must never give it
`newt-core`'s 644-crate closure. The test carries an anti-vacuous twin: the same
walker, pointed at `newt-core`, must come back finding `tokio`.

**Correction — the nine existing libraries were already gated.** An earlier
draft of this ADR claimed `formal/` held nine machine-checked libraries that no
CI job built. **That was wrong.** `.github/workflows/formal.yml` has been
running `lake build` over them, and `behavior-formal.yml` runs Lean plus a TLC
harness. Both are **path-filtered** to `formal/**`, which is why they do not
appear on an ordinary Rust PR — "built only when they change" is not the same
as "built by nothing", and the difference is the whole claim.

The path filter is the right design, not a gap: `lake build` checks the Lean,
while the Lean↔Rust correspondence is checked by
`agent-frame/tests/kernel_laws.rs`, which runs in the ordinary `test` job on
every push.

So this change adds **no new Lean job**. It adds the two agent-frame libraries
to the existing lakefile, and one anti-vacuous step to `formal.yml`: `lake
build` on a mis-configured package exits 0 having checked nothing, so the job
now requires an `.olean` for every library the lakefile declares — the list
derived from the lakefile rather than duplicated in the workflow, since a
hand-copied list is the next thing to go stale.

That step is not hypothetical. Copying agent-frame's `formal/` in initially
**replaced** this repo's lakefile and silently dropped all nine libraries from
the build, and nothing in CI would have caught it.

**The correspondence is checked on both sides.** `formal/` proves the laws;
`agent-frame/tests/kernel_laws.rs` asserts the Rust obeys the same ones, naming
the Lean theorem per test. Either half alone is a claim. The `formal` CI job
makes the Lean half real — a `formal/` folder nobody builds is decoration.

**Where the Rust deliberately differs from the referent.** Matching theorem and
test names are not correspondence; these are the places the predicates actually
diverge. Most are strengthenings; the three that are not are marked, and each is
either unreachable in v0 or owned by a named layer above the kernel.

| Lean | Rust | direction |
|---|---|---|
| `sealUnit` accepts every `Op` | `Unit::seal` and admission accept `Elide` only | stronger — `no_generation_all_checkable` is a mode in Lean, the only mode here |
| `Unit.addressed : Bool`, set by the constructor | no such field; `{source, span, elided}` is a *resolvable* address | stronger — `seal_is_addressed` becomes "the address resolves", checked by `verify_unit` |
| `root : Option Root`; `fabricated u := u.root.isNone` | `root : ContentId`, mandatory | stronger on presence — `orphan_is_fabricated` has no Rust twin because the orphan is unrepresentable |
| `Root` is an inhabitant | `root` is a *reference* that may dangle | **weaker** — resolution is the store's, not the kernel's; see C5 above |
| `Chain.wf` is a hypothesis carried on the chain theorems | every `Unit` that exists has `depth <= 1`, on both construction paths | stronger — `chain_depth_le_one` holds unconditionally rather than under a premise |
| `Chain` = `genesis \| sealed (prior)` | `Packet` = `MerkleNode` admitted at 0 or 1 parents | equal, **as of C5**; it was weaker before |
| `depthAfter` over `Nat` | `depth_after` over `u32`, saturating | weaker only at `u32::MAX`, unreachable in v0 — `the_depth_arithmetic_saturates_rather_than_wrapping` pins it |
| `render`, and its four theorems | no counterpart | not implemented — v0 has no selection API, so the Lean proves more than the Rust claims |

**It breaks the stack's shape, deliberately and temporarily.** `agent-bridle`,
`agent-mesh` and `agent-store` are separate public repos on crates.io.
`agent-frame` is the odd one out until 0.1.0. Read this as an incubator, not as
a new convention.

**Extraction later is cheap and already specified.** Promoting a workspace
member to its own crate is the pattern in
`knowledge/board/newt-agent/2026-09-08_crates-into-parts-PLAN.md`, and the
closure ledger (#2233) is the guard that keeps this crate extractable while it
lives here.

## Alternatives rejected

**Flip the private repo public.** Blocked by the embargo binding, which is on
publication and not on text — a scrub cannot unblock it, and that is by design:
*"a scrub is the chore that gets skipped."*

**Rewrite its history and re-release.** Same block, plus it destroys the record.
Force-pushing away the decision documents to publish a crate that does not exist
yet trades the reasoning for nothing.

**Wait for clearance.** The clearance is external and has been pending for most
of a year. The kernel does not need it, so waiting spends time to buy a footnote.

---

## Appendix — how the `formal` job nearly did not land

The first attempt to push this job was rejected:

```
refusing to allow an OAuth App to create or update workflow
`.github/workflows/ci.yml` without `workflow` scope
```

The cause was not the credential's scopes. This repository's `.git/config`
carried

```
[url "https://github.com/"]
	pushInsteadOf = git@github.com:
```

which silently rewrote every **push** from SSH to HTTPS, onto an OAuth token
without `workflow` scope, while fetches continued over SSH. The same section
sat next to a `[user]` block setting `codex@openai.com` — both left behind by an
earlier agent session, and both inherited by every worktree of this repo.

Removed on 2026-09-08. Pushes go over SSH, where the workflow-scope restriction
does not apply, and identity falls back to the global
`hartsock@users.noreply.github.com`. `gilamonster-agent` carried the same
rewrite and was corrected with it.

Worth recording because the failure mode is silent and misattributes its own
cause: the error names a missing OAuth scope, and the real defect is a local
config rewriting the transport.
