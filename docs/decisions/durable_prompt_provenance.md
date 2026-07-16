# ADR: Durable prompt provenance and compaction-safe objectives

Author: Shawn Hartsock and Codex
Proposed: 2026-07-16
Accepted: TBD
Status: Editing
Audience: Newt-Agent maintainers and arbiter reviewers

## Problem / Context

Newt currently derives the operator instruction after compaction by scanning the
assembled chat transcript. That transcript is a lossy presentation, not an
authoritative task record. In a multi-turn session, `turn_instruction()` selected
the first non-harness user message and `post_compaction_continuation()` labelled
it as the instruction for the current turn. The harness therefore told the model
to repeat an earlier MCP health check after it had already begun a later source
change. The model followed the harness instruction; this was deterministic
harness-induced task substitution, not spontaneous model amnesia.

The compactor reinforces the error in two ways. It protects the first
conversation user message in the frozen head, while its count-cap path may move
the tail boundary beyond the latest user message. The latest task then survives
only if a lossy summary reproduces it correctly. Count-only compression can also
fire far below the configured model context window.

Completed operator prompts are durable as `turns.user`, but only after a
successful assistant reply. A process failure, interruption, or inference error
can therefore lose the in-flight prompt. Existing `turn:`, `compaction:`, and
`spill:` handles do not provide an always-available exact current-prompt address.
Plans and scratchpad state are mutable snapshots rather than provenance rooted in
the request that caused them.

The immediate selector defect must be fixed independently of the larger design.
Durable storage alone is not sufficient: a model that does not know it has lost
the task may never choose to retrieve it. Repeating an exact prompt without a
durable address is also insufficient for crash recovery and auditability.

The loop also lacks an explicit prompt-comprehension phase. Its execution
nudgers largely treat prose without a tool call as failure to act. Asking a
blocking clarification, explaining an answer, and researching before deciding
are valid dispositions, not stalled execution. A monolithic prompt may contain
several dependent asks and unresolved decisions; the harness needs to represent
that structure before it decides whether mutation tools are appropriate.

## Proposal

Adopt a hybrid objective contract with five layers.

### 1. Caller-owned active prompt

The operator task accepted by the TUI is the sole mutation authority for the
current turn. Thread a typed `TurnPromptContext` into the agent loop; it keeps
the submitted attempt distinct from the nearest validated operator prompt and
never infers either from role messages, summaries, or harness guidance. Every
post-compaction continuation receives that value directly.

Exactly one compression-immune active-prompt card reaches every primary-model
request. The card names the active operator prompt and objective root, and names
the submitted attempt plus its chronological/semantic links when they differ or
lead elsewhere. The exact active operator text remains at user priority. If
system instructions, required schemas, and that exact prompt cannot fit the hard
context window, Newt refuses before dispatch rather than substituting a
paraphrase.

### 2. Write-before-work prompt receipts

Before retrieval, compaction, inference, or tool dispatch, atomically persist an
immutable prompt receipt in the existing conversation SQLite store. Each receipt
has a stable `prompt:<uuid>` address, conversation/writer identity, automatic
chronological predecessor, explicit semantic parent and objective root,
validated active-operator reference, receipt sequence, origin, and
domain-separated BLAKE3 digests.

Preserve two explicitly named representations:

- `raw_text`: the payload accepted by the TUI before its existing normalization.
- `model_text`: the byte-exact prompt sent to inference.

Hashes make the fidelity claim testable. A new operator submission starts a new
objective root by default; the conversation's first prompt is never implicitly
the root. Prior roots carry forward only through an explicit continuation,
harness retry, or future operator-selected relation. Persistent sessions retain
incomplete prompts. Ephemeral sessions provide the same in-memory semantics and
make no SQLite write. Conversation deletion cascades to prompt records.

### 3. Exact retrieval

Advertise a narrow, read-only `prompt_read` tool independently of general memory
disclosure. With no address it returns the active operator prompt; it also
accepts `submitted`, `root`, `parent`, `previous`, or an explicit
`prompt:<uuid>`, plus bounded offsets for long prompts. The result includes
exact `model_text`, total length, ancestry metadata, and a verified digest.

`memory_fetch` may resolve `prompt:` for compatibility when general memory is
enabled, but core task recovery must not depend on that optional feature.
Resume context names active and submitted handles plus predecessor, parent, and
root links without copying prompt bodies.

### 4. Prompt-rooted artifacts

Record derived work in an append-only prompt event/artifact ledger. An artifact
names both its originating prompt and objective root; immutable links express
`derived_from`, `updates`, `summarizes`, or `realizes`. Existing actions attach
provenance automatically:

- `update_plan` creates a `plan_revision`.
- compaction creates a `compaction_checkpoint`.
- a successful file mutation creates a `file_change` with a workspace-relative
  locator and before/after digests, not a duplicate file body.
- a completed response creates a `turn_outcome`.
- a commit or observed HEAD transition creates a `commit` artifact.
- an explicit decision operation may create a `decision` artifact.

Mutable plan and scratchpad views remain useful caches, but are not the only
record. Raw tool output is not persisted by default. Reads are fenced through
the conversation's workspace identity rather than a caller-supplied workspace.

### 5. Prompt comprehension and dispositions

After the prompt receipt is durable and before the general agent loop runs, a
structured intake pass decomposes the request into atomic asks and returns one
of four dispositions:

- `ask`: one or more blocking decisions need operator input;
- `act`: decisions are locked and mutations may begin;
- `explain`: answer or explain without an execution imperative; or
- `research`: gather bounded read-only evidence before choosing a later
  disposition.

Multiple asks do not imply ambiguity. Intake asks for clarification only when a
blocking decision lacks provenance from an explicit operator statement, a
deterministic harness or repository policy, or a low-risk assumption the
operator has authorized. A decision manifest records the source of each lock;
an LLM cannot mark a decision locked merely by asserting that it is.

The harness validates the structured result and applies capability gates:
`research` receives read-only tools, `explain` receives no mutation authority,
and `ask` emits a bounded clarification batch and ends the turn. Only `act`
enables mutation tools and execution nudges. Research may transition to ask,
explain, or act after its evidence budget. Clarification answers are new prompt
receipts explicitly linked to the pending decision manifest.

## Invariants

1. The active prompt originates at the caller and is never rediscovered from a
   transformed message list.
2. A persistent prompt receipt is committed before any model or tool work.
3. Prompt text and identity are immutable; summaries are derived references.
4. Every primary-model request contains exactly one protected active-operator
   prompt pair, and keeps submitted retry prose distinct.
5. Compaction may summarize background and progress, never the active prompt.
6. Exact active/submitted/root/parent/previous prompts remain retrievable after
   compaction, restart, or an incomplete turn.
7. Harness retries remain children of their immediate submitted attempt and
   inherit the nearest validated operator authority rather than replacing it.
8. Derived artifacts remain rooted in the prompt that caused them and are never
   silently reparented after a follow-up prompt.
9. Prompt text is absent from operational telemetry; IDs and digests are enough.
10. Mutation tools and act-now nudges are available only in the `act`
    disposition.
11. Every locked blocking decision has operator, policy, or
    authorized-assumption provenance.

## Rollout and ownership

The change lands as five reviewable PRs:

1. **Amnesia hotfix:** transcript-shaped regression and explicit current-task
   propagation through every compaction continuation path.
2. **Prompt ledger:** pre-dispatch receipts, typed active prompt, exact retrieval,
   resume integration, and crash/privacy semantics.
3. **Prompt artifacts:** append-only derived-work lineage and automatic hooks.
4. **Compaction policy:** headroom-aware triggering and objective-scoped
   diagnostics.
5. **Prompt dispositions:** structured ask decomposition, decision manifests,
   ask/act/explain/research transitions, capability gates, and nudge scoping.

### PR4 compaction-policy contract

`[context].compaction_trigger_policy` defaults to `headroom_aware`. Under this
policy, a message-count threshold is a fallback only when Newt does not know a
usable input ceiling. A configured nonzero token threshold, or a nonzero send
budget backed by a declared/believed/recovered window, is authoritative and
suppresses a *count-only* checkpoint until genuine token pressure arrives.
`max_ok_input` by itself remains a proven-good high-water mark, not proof of a
context window, so it does not suppress the fallback count guard.

`message_count` is an explicit compatibility policy that restores the legacy
count-only behavior. Neither policy changes hard token/send-budget compression,
manual `/compress`, recovered context-window 400 handling, silent-overflow
recovery, or the Responses API's current lack of an automatic compressor.

Every automatic compaction checkpoint records the policy, scalar trigger
inputs, authoritative-budget state, fired causes, and the selected cause under
its objective root. The artifact body includes its `root:prompt:<uuid>`
selector. This audit record never stores prompt text, message payloads, or tool
output; a deferred count-only decision writes no artifact to avoid ledger spam.

Newt-Agent owns storage, assembly, retrieval, and lifecycle behavior. Backend
adapters must preserve the active-prompt invariant. TUI code owns capturing raw
and model-normalized prompt forms. The operator owns retention through existing
conversation deletion and ephemeral controls.

## Test Strategy

The motivating regression uses two real user messages: an earlier MCP health
question and a later source-change request. It forces mid-turn compaction after
repository tools run and asserts that the continuation identifies only the
later task. A hostile canned summary may call the old question active; the
harness must still dispatch the exact current prompt.

Additional deterministic tests cover:

- all compaction entry points and supported backend shapes;
- repeated compression without duplicate active-prompt cards;
- multiline Unicode prompts longer than the former 400-character quote;
- byte-exact fetch and digest verification after store reopen;
- crash, interruption, inference error, and turn-save failure;
- objective follow-ups and harness-retry ancestry;
- cross-workspace denial, deletion cascades, and ephemeral zero-write behavior;
- prompt-to-plan-to-file-to-commit lineage and tamper detection;
- honest refusal when the irreducible system-plus-prompt input cannot fit; and
- count pressure with ample token headroom versus genuine hard-budget pressure;
- multi-part but fully specified prompts proceeding directly to `act`;
- ambiguous high-impact prompts producing one bounded `ask` batch;
- informational requests selecting `explain` without an execution nudge; and
- research using read-only tools before a validated disposition transition.

Every PR runs the repository acceptance contract. Live model evaluation is an
additional quality signal; deterministic wire/store assertions are the CI gate.

## Scenarios / Use Cases / Customer Stories

### Mid-task compaction

An operator asks Newt to modify a repository after an earlier informational
turn. Compaction occurs during tool work. The model receives the later prompt as
the sole active instruction and can retrieve its exact receipt by handle.

### Interrupted work

The process exits after edits but before a final answer. On restart, Newt finds
the incomplete prompt receipt and its derived artifact index instead of relying
on a missing completed turn.

### Short follow-up

The operator says "implement the approved plan." The current prompt remains
exact, while its root handle and artifact chain make the approved plan
addressable without promoting an arbitrary first conversation message to the
current instruction.

## Failure modes and residual risks

- Persisting failed and interrupted prompts expands retention beyond today's
  completed-turn behavior. The UI and documentation must disclose that change.
- The current local database is not encrypted at rest. Exactness and silent
  redaction are incompatible; encryption is a separate storage decision.
- A retrieval tool alone is unreliable for models that do not notice missing
  context. The protected exact prompt is therefore mandatory.
- Artifact capture must not become an unbounded second transcript. Store
  locators, hashes, and bounded internal artifacts rather than raw tool streams.
- Prompt parentage records chronology and explicit relations; the harness must
  not use an LLM guess to decide whether a follow-up semantically supersedes,
  continues, or forks an objective.

## Alternatives considered

| Alternative | Advantage | Why not selected alone |
|---|---|---|
| Change `.find()` to `.rfind()` | Tiny hotfix | Compaction may already remove the latest prompt, and later user-role messages can be harness noise. |
| Repeat raw prompt on every request | Strong attention anchor | No durable address, crash recovery, or artifact provenance. |
| Durable handle plus retrieval | Token efficient and restart safe | A confused model may never call the tool. |
| Prompt-rooted artifact graph only | Strong auditability | Storage does not place the right objective in model attention. |
| Treat the LLM summary as authoritative | Minimal code | Repeats the failure: a lossy model output cannot define operator intent. |

The hybrid is selected because attention, durability, retrieval, and provenance
cover distinct failure modes.

## Resources

- `newt-core/src/agentic/mod.rs` — continuation construction and agent loops.
- `newt-core/src/agentic/compress.rs` — boundary selection and triggers.
- `newt-core/src/store.rs` — durable conversation store and hash chain.
- `newt-core/src/agentic/memory_fetch.rs` — addressed retrieval substrate.
- `newt-tui/src/chat.rs` — prompt ingress and turn lifecycle.
- `docs/design/progressive-disclosure-compaction.md` — existing retrieval-first
  compaction design.
- `docs/research/context-management-modes.md` — context feature inventory.
