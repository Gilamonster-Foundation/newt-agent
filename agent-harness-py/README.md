# agent-harness-py

Composable PyO3 bindings for `agent-frame` and the deterministic smart harness.
This library owns the Python conversion boundary. A foreign extension calls
`agent_harness_py::pyo3_module::register(py, module)` to add `frame` and
`harness` submodules, without a dependency on newt-core or a terminal UI.

Only the consumer's `cdylib` enables PyO3's `extension-module` feature.
The [foreign consumer fixture](tests/consumer/README.md) builds and imports a
real extension using this public seam.

```rust,ignore
#[pymodule]
fn _native(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    agent_harness_py::pyo3_module::register(py, m)
}
```

The `frame` functions exchange JSON records, CID strings, and Python bytes.
`unit_canonical` round-trips the complete transfer record, including lifecycle;
`unit_identity_canonical` returns the derivation bytes used for its CID.
`canonical_id` checks structured canonical bytes through the shared codec.
`frame.Event` admits immutable causal events with already-admitted parent
objects, including JSON and canonical byte imports. Structural admission checks
depth and source relationships; it does not grant host authority or verify the
truth of a generated claim.

`harness.Session` accepts JSON configuration and supplies synchronous request,
reply, verdict, intervention, outcome, and tool-lifecycle recording.
It retains complete tool output, records navigation/adjudication requests and
raw replies or failures, builds bounded catalogs and validated selections,
and supports retrieval, exact replay, and restoration. `config`, `run_id`,
`head`, and `checkpoint_path` expose the stored run contract and locator;
`restore_with_config` refuses configuration drift. Python owns model calls,
per-call deadlines, tools, and orchestration.
`record_rendered_request` accepts canonical messages for the shared Anthropic
renderer; already-rendered native content blocks use `record_request`.
Send the returned receipt's `bytes` string encoded as UTF-8 unchanged. Its
committed serialization can differ from the caller's input JSON text; do not
serialize the input again for transmission.
Invalid records, proposals, and stored evidence raise `ValueError`.

Tool batches use the same durable contract as native Rust consumers:

```python
# messages includes the original assistant tool-call envelope.
invocations = session.begin_tool_batch(reply, calls_json, messages_json)
session.start_tool_call(invocations[0])  # before the host starts the effect
result = execute_tool()                # the host owns execution and disclosure
session.record_tool_return(invocations[0], result)  # exact observed bytes
session.record_tool_delivery(invocations[0], provider_message_json)
```

`calls_json` contains normalized `{id, function: {name, arguments}}` records
with object-valued arguments; keep the original provider envelopes in
`messages_json`. Commit each return before scheduling the next sequential tool.
Independently started calls may return in any order; deliver their provider
envelopes in the original call order. `kind="failed"` records a typed error actually received
from the tool; an ordinary string beginning with `Error:` remains an observed
return. `kind="host"` identifies a host-authored result and `kind="retrieval"`
requires this session's admitted `re_read` result. `retained_sources_json`
links already-retained complete tool outputs to an observed return or failure.
When output includes a transient spill pointer, first commit the raw return,
then retain its external sources and attach them with `record_tool_sources`
before delivery. A missing or invalid spill must not erase the observed return.
`resolve_tool_call` closes a queued call with an explicitly host-authored
substitute. `tool_call` returns the invocation's state and evidence CIDs.

On cancellation, `interrupt_tool_batch(reason)` commits synthetic harness
messages for unfinished protocol slots and returns the full recovery history.
Cold restoration performs the same repair: observed returns and failures
remain reachable, a started call without a return becomes `uncertain`, and a
queued call becomes `not_started`. Recovery never reruns them. A return retained
before its presentation remains reachable through the synthetic message's
evidence pointer. A cancelled await does not prove external work stopped; a
process can die between an effect and the commit of its return. This contract
does not provide exactly-once external effects.

Stop scheduling tools after a persistence error. Preserve the execution result
alongside the exception in host error reporting, release the failed session,
and recover from the actual current locator. The session refuses further
recording after a failed publication.

New runs use journal schema 2. The former `record_tool_dispatch` API is replaced
by the lifecycle methods above. Writable restore refuses schema-1 runs because
their checkpoints cannot establish whether unrecorded calls started; inspect
their immutable evidence and explicitly create a new run instead. Existing
read-only inspection and exact request replay remain available through the Rust
forensic API and `newt frame` CLI. The Python `Session` restore methods open
writable sessions and therefore refuse schema 1.

A durable session holds exclusive execution ownership of its run. Release all
references to the current session before restoring it; in CPython, `del session`
releases the owner when no other references remain. Restore requires the current
checkpoint head and publishes a new head, which the host must retain for the next
restore. A live competing writer or a stale head raises `ValueError` containing
`run writer conflict` without replacing the checkpoint. Independent runs may share
the same store. Verifying records does not transfer execution ownership.
Call `ensure_writer()` before effects performed by the Python host. A failed check
stops further recording on that session; release it and restore a verified current
checkpoint. A session inherited through `fork()` cannot record or validate writer
ownership in the child.
After `fork`, the child must release its inherited session before opening one
of its own. An inherited open descriptor can keep the parent's Unix lock held
until that copy is closed, even after the parent exits.

The foreign host must protect durable frame storage from its model's tools.
Before opening or restoring a session, require disjoint scopes for the store
and every effective tool filesystem read/write grant. Resolve path aliases and
enforce this boundary in file operations and subprocess execution. Merely
placing the store outside the workspace, changing the shell's working directory,
or hiding its path does not confine an unrestricted shell. Refuse durable
operation when the host cannot enforce the separation. Keep the full `Session`
object in trusted host code and expose retained-frame retrieval through
`re_read`; direct file access bypasses its budgets and retrieval records.

The host supplies `directory` on creation and restoration; `checkpoint_path`
returns a locator inside it, or `None` for an in-memory session. `config` exposes
the recorded authority context and budgets. Derive the current authority
context from the host's effective execution policy and pass the current
configuration to `restore_with_config`. Stored configuration cannot authorize
today's tools: the host must independently validate their grants and enforce
confinement. The deterministic library does not control foreign tools or
subprocesses.

License: Apache-2.0.
