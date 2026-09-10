# agent-harness

Host-controlled context projection, bounded navigation, and recorded agent
sessions over `agent-frame`. Models propose relevance and classifications;
the host validates them before changing a projection or delivering a reply.

This library has no inference, HTTP, terminal, or async runtime dependency.
Consumers supply model calls and authorization. `agent-harness-py` exposes
the same operations through PyO3 for other Python hosts.

Durable sessions exclusively own their run until dropped. Restore requires the
current `heads/<run CID>` value; an older valid checkpoint is a conflict, never
an implicit fork. `Error::Conflict` identifies a busy run or mismatched head.
Release the old session, read the current locator, and restore with the current
authority/configuration. Create a new session for a separate run; historical
inspection and request replay remain read-only and do not require a writer.
Mutable head publication is internal to `Session`; `FrameStore` no longer
exposes an unconditional replacement API.

Tool execution uses `begin_tool_batch(reply, calls, messages)` to admit the
normalized calls and original assistant envelopes, then `start_tool_call(id)`
before each external operation. Record its exact disclosed return immediately
with `record_tool_return`, before presentation or another tool, and attach the
existing provider result through `record_tool_delivery`. Resolve and retain
additional spill sources after the raw return, then attach them with
`record_tool_sources(id, sources)`. A bad hint cannot erase the observed return;
the attachment validates admitted external sources from the same batch without
changing the return CID. `resolve_tool_call`
records an explicit host substitute for a queued call. Invocation event CIDs
distinguish repeated calls even when the provider has no correlation IDs.

`ToolReturn::Observed` retains returned bytes without claiming success; a native
string beginning with `Error:` is still a return. A host with an actual typed
tool error can explicitly use `ToolReturn::Failed`. Neither result proves that
an operation had no side effects. Retrieval receipts and known host refusals
use separate provenance variants. Frame persistence errors stop the writer;
they are never relabelled as tool failures.

`interrupt_tool_batch` closes unfinished protocol slots in original order:
queued calls become `NotStarted`, started calls become `Uncertain`, and durable
returns/failures remain observed even if presentation failed. Recovery notices
are attributed to the harness and point to retained sources. Restore performs
this closure before returning. It never reruns a call. Requests and ordinary
transcript replacement refuse an incomplete batch. A same-process host must
close abandoned work before starting another turn.

This unmerged experimental interface now writes session schema **2** and removes
the old dispatch-only API. Writable restoration of schema 1 is explicitly
refused because it cannot prove per-call completion; start a new run. Read-only
forensic inspection and exact request replay of retained older objects remain
available. No missing old record is interpreted as a never-started operation.

Call `ensure_writer()` before host side effects. Every journal append checks the
same ownership and expected predecessor. Any failed append stops that session;
drop it and restore the actual locator. An error after atomic replacement can
leave the new head visible, so the failed session never advances its reported
committed head or attempts to roll the locator back.

Writer leases use native nonblocking Unix/Windows file locks through `fs4`, held
for the session's entire lifetime. Drop, unwinding, or process exit closes that
owner's handle. On Unix an inherited descriptor can keep the lock held until
its final copy closes, even after the parent exits. Persistent `locks/<run CID>`
sidecars must never be deleted or replaced. Sessions inherited through `fork()`
cannot write; the child must drop the inherited session before opening or
restoring its own. Filesystems must provide reliable local locking and atomic rename;
network/distributed filesystems and hostile modification of the private store
are outside this contract. Lock errors fail closed without an unsafe fallback.

Objects and staged locators are flushed before publication. Unix also flushes
the containing directories, including the store when creating `heads`. Hosts
must provision the store directory and its ancestors durably. Windows provides
atomic locator replacement and file flushing, but this implementation does not
promise persistence of the rename across power loss. Both platforms preserve
immutable records and verify their hashes on reads; unselected orphan objects
never count as a committed checkpoint. These history guarantees do not install
an execution sandbox or promise exactly-once external effects.

License: Apache-2.0.
