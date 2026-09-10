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
The journal encoding is unchanged by writer ownership. Earlier checkpoints can
resume when they are current; callers that restored while retaining the old
session must now release it first. Mutable head publication is internal to
`Session`; `FrameStore` no longer exposes an unconditional replacement API.

Call `ensure_writer()` before host side effects. Every journal append checks the
same ownership and expected predecessor. Any failed append stops that session;
drop it and restore the actual locator. An error after atomic replacement can
leave the new head visible, so the failed session never advances its reported
committed head or attempts to roll the locator back.

Writer leases use native nonblocking Unix/Windows file locks through `fs4`, held
for the session's entire lifetime. Drop, unwinding, or process exit releases
them. Persistent `locks/<run CID>` sidecars must never be deleted or replaced.
Sessions inherited through `fork()` cannot write; the child must open its own
session. Filesystems must provide reliable local locking and atomic rename;
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
