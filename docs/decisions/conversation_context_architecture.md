# Newt-Agent Conversation Context Architecture ("a folder is a conversation")

> **Status:** Proposed — **amended in part** by
> [`docs/design/context-memory-hermes-learnings.md`](../design/context-memory-hermes-learnings.md)
> (merged 2026-06-10 via #243; see its §4 and §6): storage is SQLite rather
> than the `turns.jsonl` FileStore floor; the `ConversationStore` *trait* /
> `JournalProvider` seam is deferred (YAGNI until a second store is real —
> and the trait name collides with the existing concrete struct); the
> "15.1-15.6" step numbers below are superseded by ROADMAP **Phases 17-19**;
> and **ordering is causal per §6** — signed per-writer ticks + BLAKE3
> content chains, wall-clock as a display claim only. The thesis, the
> workspace-identity scheme, and the eval-ephemerality requirement stand
> unchanged.
> **Date:** 2026-06-03
> **Scope:** `Gilamonster-Foundation/newt-agent` — this is the canonical home.
> (Originally relayed through `hartsock/gilabot` PR #1888 because the authoring
> session could not write here; relocated to the canonical path once write
> access was restored.)
> **Tracking issue:** hartsock/gilabot (Phase 15 — see linked issue).
> **Lineage / companions:** the swarm-side companions currently live in
> `hartsock/gilabot/docs/design/` (cross-repo links below); this doc is the
> local-first, single-agent projection of that lineage.
> - [`persistent-conversational-contexts.md`](https://github.com/hartsock/gilabot/blob/main/docs/design/persistent-conversational-contexts.md) — the **swarm-side** statement of "a worker is a persistent conversational context." This doc is the **local-first, single-agent projection** of that same idea: the conversation keyed by *folder/project* instead of a named swarm worker.
> - [`hermetic-vs-lived-agent-environments.md`](https://github.com/hartsock/gilabot/blob/main/docs/design/hermetic-vs-lived-agent-environments.md) — the hermetic (content-addressed, deterministic) vs lived (accumulative) tension. This design lands on **both sides of that boundary on purpose**: the `ContextResolver` (assembly) is hermetic/deterministic; the conversation journal (persistence) is lived/accumulative.
> - [`swarm-context-security-model.md`](https://github.com/hartsock/gilabot/blob/main/docs/design/swarm-context-security-model.md) — the trust-root + capability model the externalized (mesh) backend inherits.
> - newt-agent's own `docs/decisions/mesh_integration.md` (the `UserKey`/`AgentKey` trust root + `agent-mesh-bus`) and `docs/decisions/agentic_object_capability_security.md` (attenuated capabilities for externalized state).

---

## TL;DR

The defining idea for newt-agent is: **a folder is a conversation.** No matter
how newt is invoked against a directory — `newt` one-shot, the chat REPL, the
TUI, the ACP worker, or a mesh responder — it should pick up the conversational
context that belongs to *that project/folder*. This is the opposite of the
TUI-first agents in the family (monitor-agent / monty-tui), which hold their
state in a long-lived process with in-memory tabs.

It is also the **local-first projection** of gilabot's swarm-side
`persistent-conversational-contexts.md`: where that doc makes a *named swarm
worker* the persistent context, newt makes the *folder/project* the persistent
context. Same principle (contexts accumulate; they aren't ephemeral jobs),
different addressing.

Today newt cannot do this. `SessionContext.session_id` is a fresh random UUID
per process (`newt-core/src/session.rs`), conversation history lives only in RAM
inside the active `MemoryProvider` (`RollingWindow` / `TokenBudget` /
`Summarizing` in `newt-core/src/memory.rs`), and the only things that survive a
process exit are the **global** `~/.newt/NOTES.md` (`NoteStore`) and the
per-workspace `.newt/soul.md` (`SoulProvider`). Neither of those is the
conversation. Invocation #2 in the same folder starts from zero, and an
ephemeral container loses everything that wasn't committed to the repo.

This document proposes **three designs** plus a **recommended composite** for
making "folder = conversation" real, batteries-included, and extensible across
every GilaMonster repo. All three reuse the existing `MemoryProvider` /
`MemoryManager` seam so the chat loop itself does not change.

---

## What we already have (the seam to build on)

- **`MemoryProvider` trait + `MemoryManager`** (`newt-core/src/memory.rs`) — a
  fault-isolated, frozen-system-prompt, single-integration-point design lifted
  from hermes-agent. Providers already implement `initialize(&SessionContext)`,
  `system_prompt_block()`, `build_messages()`, `sync_turn()`,
  `on_pre_compress()`, `on_session_end()`, and `usage()`. **This is the entire
  attachment surface for persistence — we add providers, we do not rewrite the
  loop.**
- **`SessionContext { workspace, session_id }`** — already threaded into every
  provider's `initialize`. It is the natural place to make identity
  folder-derived instead of random.
- **The `.newt/` convention** — `SoulProvider` already resolves
  `.newt/soul.md` (workspace) → `~/.newt/soul.md` (global) → built-in. We
  extend the same convention rather than inventing a new one.
- **`newt.toml` `[memory]` config** — `MemoryProviderKind`, window,
  `context_tokens`, `soul_file`. The new pieces get a `[context]` section.
- **agent-mesh substrate** — `newt-mesh` already talks to `agent-mesh-bus`
  (NATS JetStream) with a `UserKey`/`AgentKey` trust root. This is the ready
  substrate for the externalized variant.
- **The knowledge-board** — the NFS board at
  `~/workspaces/knowledge/board/...` is the family's existing shared,
  cross-host scratch surface.

The gap is purely: **(a) stable folder-derived identity** and **(b) durable,
relocatable persistence + resume** of the turn history — plus **(c) a layered
way to *assemble* context per project** so this generalizes to any repo.

---

## Cross-cutting: folder-derived `ConversationId`

All three designs share one primitive. Replace the random
`SessionContext.session_id` with a deterministic id derived from the project
root, resolved once at startup from the cwd:

1. **Project root** = nearest ancestor containing `.newt/`, else the
   `git rev-parse --show-toplevel`, else the canonical cwd.
2. **Stable key** = the git remote URL + worktree branch when available
   (so the *same logical project* shares a conversation across clones and
   across containers), else the canonical absolute path.
3. **`ConversationId`** = a short BLAKE3 content-address of that key (mirrors
   the `content-addressable` patterns the family already uses — see gilabot's
   `docs/design/CONTENT_ADDRESSABLE_DATA.md`).

This is what makes "folder = conversation" hold across entry points: CLI, TUI,
ACP `new_session` (Step 9.1 already stores `workspace_path`), and the mesh
responder all call **one** constructor — `SessionContext::resolve(cwd)` — and
get the same id for the same folder. A `--ephemeral` / `--no-resume` flag and
the `NEWT_CONVERSATION_ID` env var override it (eval cases in `newt-eval/`
**must** run ephemeral so ambient journals never leak into a graded run).

---

## Design A — Local `.newt/` conversation journal (file-based, offline-first)

A new built-in `JournalProvider` (a `MemoryProvider`) that persists turns to an
append-only log keyed by `ConversationId`:

```
.newt/conversations/<conversation-id>/
  meta.toml        # created-at, project key, model, schema version
  turns.jsonl      # append-only: one {role, content, metrics, ts} per line
  summary.md       # latest compaction summary (from Summarizing.on_pre_compress)
```

- `initialize()` replays `turns.jsonl` into the active history provider
  (`RollingWindow` / `TokenBudget` / `Summarizing`) — **resume is automatic and
  transparent**; the history providers stay pure/in-memory and don't learn
  about disk.
- `sync_turn()` appends one line (non-blocking, matching the trait's contract).
- `on_pre_compress()` captures the dropped turns into `summary.md` so
  compaction is durable, not just in-RAM.
- Location policy is configurable: in-repo `.newt/conversations/` (shareable via
  git, `.gitignore`'d by default) **or** redirected to
  `~/.newt/conversations/<id>/` keyed by the same id (keeps the working tree
  clean). A file lock guards against two newts writing the same folder.

**Batteries-included story:** ships on by default; the user does nothing.
**Ephemeral-container story:** the journal is *relocatable* by id — mount a
volume at `~/.newt/conversations`, or commit `.newt/conversations/` into the
repo, and context survives container churn. This is the weak point: bare
containers still need a mount or a commit to truly persist (Design B fixes
this).

**Pros:** simplest; no network; offline; matches the existing `.newt/`
convention and the `MemoryProvider` seam exactly; trivially testable with
`tempfile`. **Cons:** durability across ephemeral containers depends on an
external mount/commit; concurrent writers need locking; large repos accumulate
journals (needs a GC/retention policy).

---

## Design B — Externalized content-addressable context store (mesh / board)

Same `ConversationId`, but the journal and compaction snapshots live in an
**external, content-addressed store** rather than the local FS. A
`RemoteJournalProvider` hydrates on `initialize()` and flushes on `sync_turn()`
against one of:

- **NATS JetStream KV / object store** via `agent-mesh-bus` — the substrate
  `newt-mesh` already speaks, with `UserKey`/`AgentKey` auth. Conversation state
  is a KV bucket keyed by `ConversationId`; turns are a JetStream subject the
  agent appends to and replays.
- **The knowledge-board NFS** — for a zero-broker fallback, snapshots land under
  `board/newt/conversations/<id>/` exactly like Design A but on shared storage.

**Batteries-included ephemeral container story (the headline):** container
starts → `SessionContext::resolve(cwd)` derives the id from git remote → pull
state from the store → run turns → push. The container is genuinely disposable;
the conversation is not. The same id from any host/clone resumes the same
conversation — this is the cleanest fit for "ephemeral container based agent,"
and it is exactly the durability story gilabot's
`persistent-conversational-contexts.md` §4.6 calls for, scoped down to a single
local agent.

**Extensibility across repos is structural:** because the key is the canonical
project identity (git remote + branch), *every* GilaMonster repo gets its own
conversation namespace for free — no per-repo wiring.

**Security:** externalized state is exactly the "confused deputy" surface from
the ocap decision doc and `swarm-context-security-model.md`. Reads/writes to a
conversation bucket must be an **attenuated capability** minted from the
session's `AgentKey`, scoped to that one `ConversationId` — a compromised agent
cannot read another project's conversation.

**Pros:** true ephemeral-container persistence; multi-host; reuses mesh +
board; structurally per-repo. **Cons:** needs a reachable store; heavier;
must degrade gracefully offline (fall back to Design A's local journal and
reconcile on reconnect); auth work ties it to the ocap roadmap.

---

## Design C — Layered `ContextResolver` (context as composition, not just a log)

Designs A/B persist the *chat log*. Design C addresses the other half of
"builds context by project or folder": the **assembly** of everything else that
makes a folder a coherent working context. Introduce a `ContextSource` trait and
a `ContextResolver` that composes a frozen context bundle from layered,
scoped sources, resolved global → repo → folder (nearest wins / accumulates):

| Layer | Source | Example |
|---|---|---|
| Identity | `SoulProvider` (exists) | `.newt/soul.md` |
| Repo facts | `RepoFactsSource` (new) | `CLAUDE.md`, `README`, `ROADMAP`, detected language + build commands |
| Folder facts | `FolderNotesSource` (new) | per-directory `.newt/notes.md` |
| History | `JournalProvider` (Design A/B) | `turns.jsonl` |
| Shared knowledge | `BoardSource` (new) | cross-repo entries from the knowledge-board |

A repo opts into / tunes the layers with a declarative `.newt/context.toml`
(which sources are active, scopes, budgets). This is what makes the design
**extensible over all GilaMonster repos**: a Rust repo, a Python repo
(`gilabot`), or a mixed repo each declares its own context recipe; newt ships
sensible auto-detected defaults so an undecorated repo still "just works." The
`ContextResolver` feeds the existing `MemoryManager.build_system_prompt_additions()`,
so this composes with — rather than replaces — the current frozen-prompt design.

This is also where the **hermetic/lived boundary** from
`hermetic-vs-lived-agent-environments.md` is drawn precisely: the resolver is
**hermetic** (same repo state + `context.toml` ⇒ byte-identical bundle, so the
arbiter-LLM CI gate stays honest), while the journal it reads from is **lived**
(accumulative). The boundary is the `ContextSource::resolve` return value:
deterministic in, deterministic out.

**Pros:** makes "context built by project/folder" literal, declarative, and
per-repo extensible; auto-detect means batteries-included for any repo.
**Cons:** orthogonal to persistence — needs A or B underneath; risk of prompt
bloat if budgets aren't enforced (reuse the existing `DEFAULT_CONTEXT_CAP_CHARS`
discipline from `newt-coder`).

---

## Recommended composite

Ship the three as layers, not alternatives:

1. **Identity (foundation):** folder-derived `ConversationId` via
   `SessionContext::resolve(cwd)`, used by CLI / TUI / ACP / mesh. One
   constructor, one source of truth, with `--ephemeral` and `NEWT_CONVERSATION_ID`
   escape hatches; eval runs forced ephemeral.
2. **Persistence (default + pluggable):** a single `ConversationStore` trait with
   **Design A (local `.newt/` journal) as the batteries-included default** and
   **Design B (mesh / board) as a drop-in backend** selected by
   `[context] store = "file" | "mesh"`. Local-first, with the remote
   backend reconciling against the local journal so offline degrades cleanly.
3. **Assembly (extensibility):** Design C's `ContextResolver` + `.newt/context.toml`,
   feeding the existing frozen system prompt. Auto-detected defaults per repo;
   declarative overrides where a repo wants them.

Everything attaches through `MemoryProvider`/`MemoryManager`; the chat loop is
untouched. Externalized state is gated by attenuated `AgentKey` capabilities per
the ocap doc.

---

## Buildable shape — v1 spec (the path we're taking)

Direction is decided: **agent-mesh is the substrate; the file backend is the
permanent bootstrap floor, not a temporary stand-in.** The mesh store is a
mirror *layered on top of* the file store — never an all-or-nothing dependency —
so we bootstrap into the full system with no flag day and no offline cliff.

Two new seams in `newt-core` (`context.rs`, `store.rs`), both attaching through
the existing `MemoryProvider` / `MemoryManager`. The chat loop does not change.

### Identity

```rust
/// Stable per-project conversation identity. Derived once at startup.
pub struct ConversationId(String);

impl SessionContext {
    /// Resolve identity + workspace from the working directory.
    /// root = nearest ancestor with `.newt/` → `git rev-parse --show-toplevel`
    ///        → canonical(cwd)
    /// key  = `<git remote url>#<branch>` → canonical(root)
    /// id   = blake3(key)[..16]
    /// Overrides: `--ephemeral` (random id, no resume) · `NEWT_CONVERSATION_ID`.
    pub fn resolve(cwd: &Path) -> anyhow::Result<SessionContext>;
}
```

For the file-local backend the *path* is already the key, so the id is
redundant there; we compute it uniformly anyway so swapping in the mesh backend
needs no re-keying.

### Assembly — `ContextSource` + `ContextResolver` (Design C)

```rust
pub enum Scope { Global, Repo, Folder }

pub struct ContextBlock { pub heading: String, pub body: String, pub priority: i32 }

#[async_trait]
pub trait ContextSource: Send + Sync {
    fn name(&self) -> &str;
    fn scope(&self) -> Scope;
    fn budget(&self) -> usize;             // char cap the resolver enforces
    /// Deterministic — no wall-clock, no RNG. Called once at session start.
    async fn resolve(&self, ctx: &SessionContext) -> anyhow::Result<Option<ContextBlock>>;
}

pub struct ContextResolver { sources: Vec<Box<dyn ContextSource>>, total_budget: usize }

impl ContextResolver {
    /// Frozen at session start. Order: Global→Repo→Folder, then priority.
    /// Budget-enforced and deterministic (same repo state ⇒ identical bytes).
    pub async fn resolve(&self, ctx: &SessionContext) -> String;
}
```

v1 sources: `SoulSource` (ports today's `SoulProvider`), `RepoFactsSource`
(auto-detect `Cargo.toml`→Rust / `pyproject.toml`→Python, read
`CLAUDE.md`/`README`/`ROADMAP`, surface build+test commands), `FolderNotesSource`
(`.newt/notes.md`), `JournalSource` (the resume summary), and later
`BoardSource`. The resolver output is appended via the existing
`MemoryManager.build_system_prompt_additions()` — one integration point, KV
cache preserved.

### Persistence — `ConversationStore` + layered backends (Design A → B)

```rust
pub struct StoredTurn { pub seq: u64, pub user: String, pub assistant: String,
                        pub ts: String /* injected, never Now() */, pub tokens: u32 }
pub struct Summary { pub cursor: u64, pub body: String }

#[async_trait]
pub trait ConversationStore: Send + Sync {
    async fn load(&self, id: &ConversationId) -> anyhow::Result<Vec<StoredTurn>>;
    async fn append(&self, id: &ConversationId, turn: &StoredTurn) -> anyhow::Result<()>;
    async fn load_summary(&self, id: &ConversationId) -> anyhow::Result<Option<Summary>>;
    async fn save_summary(&self, id: &ConversationId, s: &Summary) -> anyhow::Result<()>;
}
```

Backends:
- **`FileStore`** — `.newt/conversations/<id>/{turns.jsonl,summary.md,meta.toml}`
  (or `~/.newt/conversations/<id>/`). Append-only, file-locked. Zero deps,
  offline. **The bootstrap floor.**
- **`MeshStore`** — JetStream subject `newt.conv.<id>.turns` (append/replay) +
  KV `[<id>]` (summary+cursor), authed by an `AgentKey` attenuated to that one
  namespace. Behind a feature flag + the `newt-mesh` workspace-exclusion
  precedent so default-workspace CI stays green.
- **`LayeredStore { wal: FileStore, mirror: Option<MeshStore> }`** — writes hit
  the file WAL synchronously (fast, locally durable) and the mirror async; reads
  prefer the mirror, fall back to the WAL; on reconnect the WAL tail past
  `last_synced` is flushed. **This is "file backing as pragmatic bootstrap" made
  literal:** mesh is always file-backed underneath — no offline cliff, no
  migration event.

`JournalProvider` (a `MemoryProvider`) wraps any `ConversationStore`: `load` on
`initialize()` to seed history + feed `JournalSource`; `append` on `sync_turn()`;
`save_summary` on `on_pre_compress()`.

### Config (`newt.toml`)

```toml
[context]
store        = "file"   # "file" | "mesh"  (mesh ⇒ file WAL underneath)
location     = "home"   # "home" (~/.newt/…) | "repo" (.newt/, shareable)
budget_chars = 8000
resume       = true     # false == --ephemeral

[context.sources]       # omit ⇒ all auto-detected
board = false           # cross-repo knowledge (needs store = "mesh")

[context.mesh]          # only when store = "mesh"
subject_prefix = "newt.conv"
```

### Step sequence (each one `step-NN.M`, acceptance contract per PR)

Phase 15 — Conversation Context:
- **15.1** `ConversationId` + `SessionContext::resolve` (+ `--ephemeral`, env
  override; evals forced ephemeral). No behavior change.
- **15.2** `ContextSource` / `ContextResolver` + `SoulSource` + `RepoFactsSource`.
  Ships "context by project/folder" offline.
- **15.3** `ConversationStore` + `FileStore` + `JournalProvider`. Ships
  "folder = conversation" resume — the hidden-file MVP, done right.
- **15.4** Durable compaction: `on_pre_compress` → `save_summary`; resume =
  summary + tail.
- **15.5** `LayeredStore` + `MeshStore` (feature-flagged, ocap-scoped, offline
  reconcile). The full system, file-backed throughout.
- **15.6** `BoardSource` — cross-repo knowledge over the mesh/board store.

Steps 15.1–15.4 need no network and no agent-mesh checkout; each leaves the tree
green and is independently shippable.

## Open questions

- **Gitignore vs commit** for `.newt/conversations/` — default to ignored;
  allow opt-in commit for shared/auditable conversations.
- **Secret redaction** before a turn is journaled (tool output can contain
  tokens) — likely a redaction pass in `sync_turn`.
- **Multi-folder projects** — one id per git toplevel, or sub-conversations per
  subdir? Proposal: one per toplevel, optional named sub-scopes.
- **Schema/versioning** of `turns.jsonl` for forward-compat migrations.
- **Retention** — turn count / age / size caps and a `newt context gc` command.
- **Store location default** — `home` (`~/.newt/…`, clean tree) vs `repo`
  (`.newt/…`, shareable/auditable). Proposal: default `home`, opt into `repo`.
- **Branch scoping** — does a PR branch share the project's conversation with
  `main`, or get its own? Proposal: key on repo; add an optional branch
  sub-scope for divergent work.
