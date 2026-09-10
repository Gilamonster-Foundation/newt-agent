# Smart harness implementation

This is the implementation contract for the design proposed in
[PR #2243](https://github.com/Gilamonster-Foundation/newt-agent/pull/2243).
The reviewed baseline was `ddbf870f`; that proposal remains the historical
record of the corrections to its earlier claims. The contracts below replace
its open implementation questions. Configuration and invocation examples are
in [the operator guide](../guide/smart-harness.md).

## Reused boundaries

`agent-frame` retains `Unit`, `Derivation`, `RootEvent`, and `Packet`.
A unit still identifies a source span; a packet still has at most one previous
packet. `Event` adds causal parents and explicit derivation sources without
changing either existing identity. The `content-addressable` crate mints every
structured and raw identity, canonical encoding, and Merkle link.

`agent-harness` owns verified storage, session admission, projection, navigation,
and control records. It has no inference, HTTP, terminal, or async dependency.
Its memory and durable stores implement the same `NodeStore` boundary.
`agent-harness-py` exposes these libraries through independent `frame` and
`harness` modules. Its external consumer fixture builds a separate Python
extension without Newt's runtime. The umbrella `newt_agent` extension registers
the same modules, so other Python hosts can compose the leaf bindings directly.

Newt's adapter supplies existing provider loops, tool validation, spill storage,
disclosure filtering, cancellation, metrics, and inference. Smart mode is
explicitly enabled. The existing deterministic classifier remains available in
legacy mode and in the comparison evaluator; smart mode never falls back to it.

## Decisions and verification

| Decision | Implemented contract | Main evidence |
|---|---|---|
| D1: primitives | External observations, harness interventions, verdicts, retrievals, and elisions use `Event` over `MerkleNode`. Packets keep their chain. A session journal binds ordered packet occurrences to their distinct elision events. | `agent-frame/tests/events.rs`; `agent-harness/src/session/tests.rs` |
| D2: projection | Each dispatched request names ordered event entries, roles, spans, rendering template, provider format, and exact byte commitment. The bytes recorded are the bytes sent. | `agent-harness/tests/projection.rs`; four-provider HTTP replay tests |
| D3: host ownership | Tool-less relevance calls propose a strict JSON CID array. The host checks the entire selection, authority, required inputs, tool groups, and budgets before dispatch. Rejected proposals and auxiliary errors remain recorded. | `agent-harness/tests/navigation.rs`; `http_smart_harness.rs` |
| D4: harness origin | Host messages are explicitly registered at insertion. Auxiliary payloads and control text cannot impersonate operator input or become source relevance evidence. Parsed model text and decorated delivery records remain distinct. | Operator collision and model-answer evidence regressions in `session/tests.rs` |
| D5: depth | Admission recomputes generation depth from derivation sources, not all causal parents. Generation from depth one is rejected. Elision and retrieval preserve source origin and depth. Native provider tool envelopes retain retrieval provenance. | Kernel construction, JSON/CBOR admission, and native-provider receipt tests |
| D6: lifecycle | Each fresh invocation has an addressed root containing its configuration and occurrence nonce. Resume records its starting checkpoint and current authority. Hermetic input constraints are explicit and prohibit resume. | Shared configuration tests; real restart tests; CLI flag tests |
| D7: failures | Raw replies persist before auxiliary work. Failure records settle failed adjudication without inventing a verdict. Unavailable startup configuration is a visible admission error before any model reply. Missing or corrupt storage fails closed. | Timeout, cancellation, malformed reply, missing source, and failed checkpoint tests |
| D8: legal cuts | The last actual operator message, system constraints, and unseen tool results are protected. Call/result groups remain whole. Harness pins are fixed protocol constraints, outside relevance evidence. | Navigation and provider integration tests |
| D9: durability | Sources and addressed nodes persist before the journal publishes its checkpoint. Resume verifies the complete admitted closure and current configuration. Tool output leaves ephemeral spill storage before durable slices are returned. | Real filesystem, cold process, TUI locator, and shell spill restart tests |
| D10: CPU auxiliary | Embedded smart inference explicitly selects CPU and captures the exact model/tokenizer bytes named by the manifest. An external override requires a separate endpoint and declared CPU placement. No primary fallback exists. | Inference configuration and pinned asset tests; real CPU smoke |
| D11: mediated retrieval | `re_read` admits only session events or packets, checks byte intervals and budgets, and records a receipt parented on the followed pointer and retrieved artifacts. A partial slice states its continuation. | Receipt, tamper, access, and restart tests |
| D12: separate budget | Auxiliary calls, input/output sizes, generation tokens, and elapsed time have their own configuration and accounting. Navigation adds independent catalog, fetch, dereference, retry, and elapsed-work limits. | Runtime budget tests; emitted solve contract |
| D13: question | Strict verdicts are `answer`, `narration`, or `question`. A question is delivered with `awaiting_operator`. Narration gets a bounded continuation or an incomplete result. | Four-provider answer-after-nudge tests, question tests, and baseline comparison evaluator |

## Admission and request flow

1. The frontend establishes current workspace authority, tool caveats, private
   frame directory, provider format, input policy, and auxiliary configuration.
   It checks storage isolation before auxiliary startup and local MCP launch.
   A content address never grants access by itself.
2. The host records original messages and tool results. Explicit host messages
   carry harness origin. A real model reply after a nudge remains a new external
   observation, even though the nudge is one of its causal antecedents.
3. When context exceeds its budget, the host gives the auxiliary a bounded
   catalog of original source facts. Its proposed CID selection is untrusted.
   A rejected proposal cannot silently drop required material or change budgets.
4. Omitted originals receive existing elision proofs and an ordered packet.
   The projected context includes a recorded `re_read` pointer. Generated
   harness material is excluded from relevance evidence and generation inputs.
5. The host persists the projection and exact rendered request before network
   dispatch. It records the full provider reply before parsing or adjudicating
   it. Validated tool batches receive a separate dispatch record before effects.
6. A tool-less reply is parsed into original model text, then classified through
   a bounded auxiliary call. Its raw auxiliary prompt, output or failure,
   normalized verdict, and host control outcome are distinct records.
7. An answer still passes the existing task-evidence checks. Causal parentage
   and a classifier verdict prove neither factual correctness nor task success.
   A genuine answer after a nudge is deliverable. A requested completion slogan
   has no special authority. Exhausted narration is incomplete, including the
   final-round path formerly implicated in #2239.

Construction and restored admission share the same checks. Journal positions
identify occurrences; content equality alone does not identify membership.
Repeated equal units retain separate ordered occurrence links. A failed atomic
checkpoint publication aborts the live session; recovery requires verified
restoration, preventing later writes from using partially published state.

## Durability and replay

Newt stores frames outside every effective model filesystem grant, by default
under its user configuration directory at `frame/<workspace path RawContentId>`.
Admission checks both canonical and lexical overlap with read, write, and
implicit executor roots. Grant anchors and their intermediate symlink targets
must have no model-writable ancestor; redundant nested grants and grant strings
containing `..` are refused. The canonical directory is part of session authority.
Every tool dispatch and permission widening rechecks the boundary. A model
cannot read, rewrite, or traverse the graph through ordinary file or shell
tools; retained context is exposed through admitted `re_read` requests.

This runtime currently requires Linux object-bound native filesystem tools and
Landlock, and refuses unconfined launch modes. Local MCP children inherit an
admitted capability at startup. Remote servers are separately configured
authorities. Foreign consumers must enforce storage isolation in their own
tool and subprocess execution; the pure library verifies content and session
admission without providing an operating-system sandbox.

The crew workspace adapter and native recursive `find` cannot enforce this
boundary and are refused in smart mode; searches use the confined shell.
Code indexing binds metadata and content reads to the opened workspace.
The native Git adapter retains its existing scoped-read guard;
shell Git bypasses convenience routing but remains confined. Existing commit
attribution policy still refuses shell-created commits. Adapting those
capabilities to support attributed commits with bounded filesystem reads is
outside this frame implementation.

Structured objects are canonical DAG-CBOR files named by their `ContentId`.
Opaque material uses `RawContentId`. An atomically replaced `heads/<run CID>`
locator points to immutable journal state. The TUI keeps an immutable
conversation-to-run binding and verifies the checkpoint when reopening it.
Old conversations without that binding require a new conversation, or an
existing frame can be resumed through `newt solve`. Inherited text is not
treated as restored history.

Replay reconstructs a request from retained rendering inputs and compares it
with its stored commitment. It refuses missing, redacted, or substituted
material. The forensic CLI exposes immediate record links and exact request
replay. Read-only inspection reports its verification boundary explicitly;
it does not grant a resumable session or assert that immediate links constitute
complete graph admission.

Retrieval reports the pointer, receipt CID, byte interval, total length, and
next offset. Packet slices preserve the source event for each overlapping
occurrence. The host never presents a truncated prefix as a complete result.
Fetched source material is verified before use; bounded display is independent
of the retained original. An oversized original may require a larger configured
fetch budget even when the requested display slice is small.

## Inputs and limits

Hermetic mode admits the current task, system instructions, tool definitions,
workspace, configuration, and declared auxiliary model. It excludes inherited
conversation and ambient memory. It does not claim deterministic sampling,
immutable workspace files, or a network sandbox. Fresh graph origin alone
proves none of these execution properties.

Navigation elapsed time measures navigation work, including its auxiliary
calls. Waiting for the primary model or the operator does not consume it.
Auxiliary classification and navigation share the separate auxiliary call and
time budget. Limits are recorded with the run; changing them requires a fresh
session. Full restoration checks the caller's current configured history cap.
The authority-only foreign restore convenience API uses the default traversal
cap; callers with a larger contract must use `restore_with_config`.

Startup can fail before an invocation is admitted, for example when its CPU
model is absent. The operator receives that error; there is no raw reply or
successful session to claim. An in-turn auxiliary failure retains the observed
reply and records failure. If storage itself is unavailable, durable failure
recording cannot be promised; the operation stops and reports the storage error.

The model-backed classifier is an empirical hypothesis, not a theorem about
three classes. The comparison evaluator uses the production prompt and parser,
records baseline decisions and actual auxiliary outputs/errors, and identifies
its fixtures, assets, placement, and timing. Scripted provider tests establish
control behavior only. They do not establish model classification quality.
No superiority claim follows from a third class or from a clean test suite.

The new accept path applies when smart mode is enabled. With smart mode
disabled, the legacy rescue and classifier remain in use. The final-round
narration status correction applies to both paths; this change does not claim
to close every legacy accept-site behavior in #2239.

License: Apache-2.0, consistent with the workspace.
