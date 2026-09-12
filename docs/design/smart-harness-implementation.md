# Smart harness implementation

This is the current contract for the experimental `feat/smart-harness` branch
in [PR #2260](https://github.com/Gilamonster-Foundation/newt-agent/pull/2260).
It implements the proposal in
[PR #2243](https://github.com/Gilamonster-Foundation/newt-agent/pull/2243),
including the external auxiliary placement amendment in
[PR #2263](https://github.com/Gilamonster-Foundation/newt-agent/pull/2263).
The [design record](smart-harness.md) preserves the proposal's historical
assumptions and review. The contracts below describe implemented behavior;
configuration and invocation examples are in
[the operator guide](../guide/smart-harness.md).

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
| D1: primitives | External observations, harness interventions, verdicts, retrievals, and elisions use `Event` over `MerkleNode`. Packets keep their chain. A session journal binds ordered packet occurrences to their distinct elision events. | [Event tests](../../agent-frame/tests/events.rs): `identity_commits_to_ordered_occurrence_and_every_causal_link`; [Packet admission](../../agent-frame/tests/admission.rs): `a_decoded_two_parent_node_is_refused`; [Session tests](../../agent-harness/src/session/tests.rs): `packet_slots_preserve_occurrences_of_identical_units`. |
| D2: projection | Each dispatched request names ordered event entries, roles, spans, rendering template, provider format, and exact byte commitment. The bytes recorded are the bytes sent. | [Projection tests](../../agent-harness/tests/projection.rs): `cold_render_preserves_order_roles_and_all_request_inputs`, `missing_or_redacted_inputs_refuse_byte_perfect_replay`; [Four-provider HTTP tests](../../newt-core/src/agentic/mod_tests/http_smart_harness.rs): `all_four_wires_deliver_a_real_answer_after_a_nudge_without_regeneration`. |
| D3: host ownership | Tool-less relevance calls propose a strict JSON CID array. The host checks the entire selection, authority, required inputs, tool groups, and budgets before dispatch. Rejected proposals and auxiliary errors remain recorded. | [Selection admission](../../agent-harness/tests/navigation.rs): `proposal_cannot_drop_pins_unseen_results_or_half_a_tool_pair`; [Runtime tests](../../newt-core/src/agentic/smart_harness.rs): `malformed_navigation_never_falls_back_to_a_deterministic_selection`, `cancelled_initial_navigation_retains_a_request_linked_failure`. |
| D4: harness origin | Host messages are explicitly registered at insertion. Auxiliary payloads and control text cannot impersonate operator input or become source relevance evidence. Parsed model text and decorated delivery records remain distinct. | [Origin regressions](../../agent-harness/src/session/tests.rs): `operator_text_matching_auxiliary_output_stays_operator_input`, `unsent_host_message_cannot_claim_next_turns_operator_text`, `observed_answer_remains_relevance_evidence_after_outcome`. |
| D5: depth | Admission recomputes generation depth from derivation sources, not all causal parents. Generation from depth one is rejected. Elision and retrieval preserve source origin and depth. Native provider tool envelopes retain retrieval provenance. | The harness has no separate Unit-depth logic: Units cross the single `agent-frame` admission seam and inherit `depth == op.depth_after(0)` from `TryFrom<RawUnit>`. [Unit admission](../../agent-frame/tests/admission.rs): `a_decoded_depth_two_is_refused`; [Event admission](../../agent-frame/tests/events.rs): `generation_over_generated_material_is_refused_at_both_boundaries`, `retrieval_preserves_generated_origin_and_depth`, `elision_keeps_the_existing_unit_proof_and_inherited_provenance`; [Native receipt regression](../../agent-harness/src/session/tests.rs): `native_provider_tool_receipts_never_become_operator_or_source_evidence`. |
| D6: lifecycle | Each fresh invocation has an addressed root containing its configuration and occurrence nonce. A writable session holds exclusive run ownership and checks its expected head against the current locator. Resume admits only that current checkpoint with matching authority; hermetic input constraints prohibit resume. | [Writer tests](../../agent-harness/tests/writer_ownership.rs): `separate_runs_can_write_in_the_same_store`, `stale_restore_cannot_replace_a_closed_writers_current_head`; [Process test](../../agent-harness/tests/writer_process.rs): `competing_process_cannot_mutate_an_owned_run`; [Configuration test](../../newt-core/src/config/smart_harness.rs): `restore_rejects_changed_authority_settings_and_hermetic_resume`; [Root test](../../agent-harness/src/session/tests.rs): `restored_run_root_must_commit_its_actual_configuration`. |
| D7: failures | Raw replies persist before auxiliary work. Failure records settle failed adjudication without inventing a verdict. Unavailable startup configuration is a visible admission error before any model reply. Missing or corrupt storage fails closed. | [Observation and restart tests](../../agent-harness/tests/session.rs): `observation_precedes_verdict_and_request_replays`, `restart_verifies_closure_and_keeps_unadjudicated_observation`; [Failure tests](../../newt-core/src/agentic/smart_harness.rs): `auxiliary_timeout_and_cancellation_are_bounded_and_recorded`, `cancelled_initial_navigation_retains_a_request_linked_failure`; [Failure-recording method (not a test)](../../agent-harness/src/session.rs): `record_adjudication_failure`; [Checkpoint test](../../agent-harness/src/session/tests.rs): `failed_checkpoint_publication_aborts_the_live_session`; [Startup test](../../newt-cli/tests/solve_cli.rs): `smart_solve_admits_private_storage_before_loading_the_auxiliary`. |
| D8: legal cuts | The last actual operator message, system constraints, and unseen tool results are protected. Call/result groups remain whole. Harness pins are fixed protocol constraints, outside relevance evidence. | [Legal-cut test](../../agent-harness/tests/navigation.rs): `proposal_cannot_drop_pins_unseen_results_or_half_a_tool_pair`; [Operator-pin test](../../agent-harness/tests/session.rs): `actual_operator_stays_pinned_after_harness_nudge`; [Provider-adapter test](../../newt-core/src/agentic/smart_harness.rs): `accepted_navigation_keeps_pins_and_retains_retrievable_source_after_restart`. |
| D9: durability | Sources and addressed nodes persist before checkpoint publication. Schema 2 records each batch, distinct call occurrence, start, observed return, and model-facing delivery. Resume admits the complete closure and current configuration, preserves observed returns, and explicitly closes uncertain or unstarted slots without rerunning tools. | [Lifecycle tests](../../agent-harness/tests/tool_lifecycle.rs): `cold_restore_closes_each_wire_without_losing_a_completed_return`, `returned_without_delivery_stays_returned_and_retains_its_source_closure`; [Four-provider recovery](../../newt-core/src/agentic/mod_tests/http_smart_completion.rs): `all_four_abrupt_future_drops_preserve_completed_calls`; [Schema and publication tests](../../agent-harness/src/session/tests.rs): `pre_lifecycle_runs_refuse_writable_restore_but_keep_read_only_replay`, `interrupted_publication_requires_drop_and_restore_of_the_actual_locator`. |
| D10: auxiliary placement | The default embedded auxiliary runs on CPU and captures the exact model/tokenizer bytes named by the manifest. An external override pins its endpoint, protocol, and model and declares placement, which the client records without verifying remote hardware. It may share the primary origin; the manifest records `shares_primary_origin`. Non-CPU placement requires an external backend. No silent primary fallback exists. | [Embedded tests](../../newt-inference/src/embedded.rs): `new_cpu_pins_the_device_passed_to_generation`, `cpu_assets_remain_bound_to_the_recorded_bytes_after_path_substitution`; [Placement tests](../../newt-inference/src/smart_harness.rs): `unsafe_or_incomplete_placement_never_falls_back_to_primary`, `a_same_origin_auxiliary_is_accepted_and_its_manifest_says_so`, `embedded_auxiliary_refuses_a_non_cpu_declaration_before_loading_assets`; [Opt-in real-model smoke (ignored by default)](../../newt-inference/src/embedded.rs): `smoke_generate_on_cpu`. |
| D11: mediated retrieval | `re_read` admits only session events or packets, checks byte intervals and budgets, and records a receipt parented on the followed pointer and retrieved artifacts. A partial slice states its continuation. | [Bounded receipt and restart test](../../agent-harness/tests/session.rs): `elision_reread_is_recorded_bounded_and_restorable`; [Receipt provenance test](../../agent-harness/src/session/tests.rs): `native_provider_tool_receipts_never_become_operator_or_source_evidence`; [Tamper test](../../agent-harness/tests/storage.rs): `disk_restart_retains_sources_and_refuses_tampering`. |
| D12: separate budget | Auxiliary calls, input/output sizes, generation tokens, and elapsed time have their own configuration and accounting. Navigation adds independent catalog, fetch, dereference, retry, and elapsed-work limits. | [Auxiliary budget tests](../../newt-core/src/agentic/smart_harness.rs): `auxiliary_system_instruction_shares_the_input_byte_budget`, `oversized_auxiliary_replies_are_retained_before_budget_rejection`, `auxiliary_timeout_and_cancellation_are_bounded_and_recorded`; [Navigation-work budget test](../../agent-harness/src/session/tests.rs): `idle_time_does_not_exhaust_navigation_work_budget`; [Selection budget test](../../agent-harness/tests/navigation.rs): `proposal_cannot_drop_pins_unseen_results_or_half_a_tool_pair`; [Emitted contract test](../../newt-cli/src/solve_contract.rs): `smart_harness_configuration_is_declared_with_a_permitted_v1_outcome`. |
| D13: question | Strict verdicts are `answer`, `narration`, or `question`. A question is delivered with `awaiting_operator`. Narration gets a bounded continuation or an incomplete result. | [Strict verdict test](../../newt-core/src/agentic/smart_harness.rs): `verdict_protocol_accepts_only_an_exact_class`; [Four-provider control tests](../../newt-core/src/agentic/mod_tests/http_smart_harness.rs): `all_four_wires_deliver_a_real_answer_after_a_nudge_without_regeneration`, `all_four_wires_preserve_questions_and_honestly_stop_exhausted_narration`; [Terminal accounting test](../../newt-cli/src/solve_contract.rs): `final_round_narration_preserves_legacy_outcomes_and_accounts_for_smart_runs`. |

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
   it. Before effects, schema 2 commits the validated batch and a distinct start
   for each dispatched call. Each observed return persists before display or
   another tool; its later model-facing delivery is recorded separately.
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
A writable session owns an exclusive run lock until release and refuses a
changed expected head. Writable restoration accepts only the current locator
and schema 2; schema 1 lacks the per-call evidence needed for safe continuation.
Interrupted calls with no observed return remain explicitly uncertain, while
admitted calls that never started remain unstarted. Restored provider slots are
closed with recorded host notices or retained-return pointers, without replaying
external tool effects.
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
session. Sharing an external inference server may cause resource contention;
separate accounting does not reserve hardware capacity. Embedded sessions keep
their immutable asset bytes until released. Newt's CLI/TUI restores compare the
current resolved configuration, including auxiliary manifest fields, with the
recorded configuration; incompatible earlier manifests require a fresh run.
Full restoration checks the caller's current configured history cap.
The authority-only foreign restore convenience API uses the default traversal
cap; callers with a larger contract must use `restore_with_config`.

Semantic-gather manifests count extension-matching candidates that pass the
workspace-bound open and metadata checks. Entries denied or unreadable at those
checks are omitted from `candidate_count`, `candidate_hash`, and `cuts`; caps
describe that admitted set. A later content-read failure can also leave a planned
kept entry without returned source text. Accounting for every discovered or
unreadable entry is outside this repair.

Startup can fail before an invocation is admitted, for example when an embedded
model is absent or an external override is incomplete. The operator receives
that error; there is no raw reply or
successful session to claim. An in-turn auxiliary failure retains the observed
reply and records failure. If storage itself is unavailable, durable failure
recording cannot be promised; the operation stops and reports the storage error.

The model-backed classifier is an empirical hypothesis, not a theorem about
three classes. The comparison evaluator uses the production prompt and parser,
records baseline decisions and actual auxiliary outputs/errors, and identifies
its fixtures, assets, placement, and timing. Scripted provider tests establish
control behavior only. They do not establish model classification quality.
No superiority claim follows from a third class or from a clean test suite.

The [Harbor adapter](../../scripts/eval/harbor/README.md) supplies a separate
task-level experiment: the same branch binary runs with smart mode off and on,
and its recorded outcome is compared with the task verifier. The runner checks
the binary's glibc requirement against a configured floor and reads results
from the configured job directory.
The documented eight-task subset excludes four tasks whose reference solutions
failed the local verifier. These apparatus checks and curated fixtures do not
establish that smart mode improves task completion or reduces false completions.

The new accept path applies when smart mode is enabled. With smart mode
disabled, the legacy rescue, classifier, and #2218 final-round narration outcome
remain in use. Smart final-round narration is incomplete; this change does not
claim to close every legacy accept-site behavior in #2239.

License: Apache-2.0, consistent with the workspace.
