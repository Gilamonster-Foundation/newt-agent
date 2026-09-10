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
reply, verdict, intervention, outcome, and normalized tool-dispatch recording.
It retains complete tool output, records navigation/adjudication requests and
raw replies or failures, builds bounded catalogs and validated selections,
and supports retrieval, exact replay, and restoration. `config`, `run_id`,
`head`, and `checkpoint_path` expose the stored run contract and locator;
`restore_with_config` refuses configuration drift. Python owns model calls,
per-call deadlines, tools, and orchestration.
`record_rendered_request` accepts canonical messages for the shared Anthropic
renderer; already-rendered native content blocks use `record_request`.
Invalid records, proposals, and stored evidence raise `ValueError`.

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
