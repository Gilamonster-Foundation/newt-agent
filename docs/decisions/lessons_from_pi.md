# Decision: lessons drawn from the `pi` agent harness (inbound learnings)

**Status:** Accepted (recorded by Shawn Hartsock, 2026-06-14)
**Date:** 2026-06-14
**Related:** `docs/decisions/plain_scroller_tui.md` (newt's surface),
`docs/decisions/mesh_integration.md` (newt as a mesh worker),
`docs/decisions/conversation_context_architecture.md` (§6 causal store),
`docs/decisions/agentic_object_capability_security.md` (the ocap leash);
agent-bridle `integrations/pi-bridle/` (the outbound experiment this doc
authorizes).

---

## TL;DR

`pi` (`github.com/earendil-works/pi`, Mario Zechner / earendil-works) is a
public, MIT-licensed **TypeScript** coding-agent harness with a deliberately
minimal core and an extension model. It is a near mirror-image of newt: same
problem, opposite language, opposite philosophy on several axes. That makes it
an unusually clean reference. This doc records **what newt takes from pi** (the
inbound half) and authorizes **one** outbound experiment (a pi extension that
runs pi's tools through newt's agent-bridle leash) — but **only inside our own
repos**, to prove utility for ourselves before any upstream contribution.

We are **not** adopting pi's architecture wholesale. newt's load-bearing
choices — causal ordering over wall-clock, the concrete (non-trait) loop, the
plain-scroller surface, local-first/zero-C-dep — stand. The items below are
discrete primitives worth lifting, each justified on its own.

## Context — what pi is, and why it is a useful telescope

| Axis | pi | newt |
|------|----|------|
| Language | TypeScript / Node ≥22 | Rust |
| Core philosophy | minimal core + extensions | lean concrete loop, no framework |
| Provider model | runtime **registry** (`registerApiProvider`, `sourceId`) | compiled `InferenceBackend` + router tier |
| Session store | JSONL, **branching tree** (parent pointers), UUIDv7-ordered | SQLite, **linear per-writer merkle log**, Lamport-tick-ordered |
| TUI | differential renderer (Kitty images, sync-output) | plain scroller (deliberate) |
| Permissions | **none** — "containerize it yourself" | agent-bridle ocap leash, in-process + Landlock |
| Compaction | summarize-first (`compact`/`shouldCompact`) | **structural prune → summarize** |
| Tool execution | concurrent, ordered events | sequential rounds |

The mirror is the value: where pi is strong, newt is often deliberately
absent, and vice-versa. pi's gaps (no permission system, timestamp ordering)
are exactly newt's strengths; pi's strengths (registry providers, branching
sessions, parallel tools, differential rendering) map cleanly onto things newt
either lacks or has intentionally deferred to `gilamonster-agent`.

Reference reading (pi source, as of clone 2026-06-14):
`packages/ai/src/api-registry.ts`, `packages/agent/src/agent-loop.ts`,
`packages/agent/src/harness/session/jsonl-repo.ts`,
`packages/tui/src/tui.ts`, `packages/coding-agent/examples/extensions/`.

## Decision — inbound learnings, ranked by fit

### Adopt (clear fit with newt's design)

1. **The UI-message vs. wire-message boundary (`convertToLlm`).**
   pi keeps rich custom message types in the session record (`bashExecution`,
   `branchSummary`, `compactionSummary`) and converts them to valid LLM roles
   *only at send time* (`agent-loop.ts:275-308`). newt's `ConversationTurn` /
   `ToolEvent` already separate stored from sent, but the conversion is not a
   single named choke point. **Action:** formalize a `to_wire()` boundary so
   the store can hold display-only entries (e.g. compression markers, notes)
   that are provably never emitted as invalid roles. Roadmap candidate, not
   urgent.

2. **Structural prune-before-summarize is validated by an independent design.**
   newt already does this (Phase 18: BLAKE3-dedup tool results, per-tool
   one-liners, JSON-aware arg shrinking, *then* an LLM summary). pi summarizes
   first. This doc records that newt's order is the differentiator and should
   **not** regress toward pi's. No action beyond "keep it."

### Consider (good idea, needs design work)

3. **Parallel tool execution with deterministic event ordering.**
   pi runs independent tool calls concurrently, emits `tool_execution_end` in
   *completion* order but `toolResult` messages in *source* order
   (`agent-loop.ts:373-449`). newt runs sequential rounds. newt's `TurnDriver`
   already isolates the non-`Send` loop on a dedicated thread, so the
   concurrency primitive exists. **Action:** spike a per-round parallel
   executor for independent calls, preserving source-order results. Latency
   win for read-heavy turns. Gate behind a config knob; sequential stays the
   default for reproducibility.

4. **Conversation branching.**
   pi's JSONL tree forks without copying (parent pointers + `firstKeptEntryId`
   compaction markers). newt's store is intentionally a *linear* per-writer
   merkle log — better for causal integrity, but it cannot fork a conversation.
   **Action:** evaluate whether a branch point can be expressed *causally*
   (a turn whose `prev_hash` references a non-tip ancestor) rather than by
   timestamp, so branching does not betray §6. Design spike only; do not build
   until a concrete need (e.g. "what-if" replays) appears.

### Note but do not adopt in newt (belongs downstream)

5. **Differential TUI rendering** (`tui.ts:1208-1350`) — line-diff, Kitty
   image tracking, synchronized-output protocol. Genuinely good, and a strong
   reference for **`gilamonster-agent` / `monitor-agent`**. It is explicitly
   *out of scope for newt* per `plain_scroller_tui.md`; recording it here so the
   reference is not lost.

6. **Runtime provider registry.** pi lets extensions add whole providers at
   runtime. newt's compiled-backend + subprocess-plugin model is the correct
   posture for a local-first, zero-C-dep binary. Note for `gilamonster-agent`
   if it ever wants community-contributed backends without recompilation.

### Engineering practices to mirror (cheap, high value)

7. **Release smoke-test in an isolated install.** pi's `release:local` builds,
   packs, and installs each package *outside the repo* before tagging
   (`README.md` "Supply-chain hardening"). **Action:** add an equivalent
   `just release-smoke` that builds the binary and runs it from a temp dir with
   a clean `$HOME`, catching "works in-tree, breaks installed" regressions.

8. **Dependency-lifecycle-script allowlist.** pi fails CI when a new dep
   introduces an install lifecycle script not on an explicit allowlist.
   newt's pure-Rust posture makes this mostly moot, but the *principle*
   (new ambient-execution surface must be reviewed, not silently accepted)
   is worth a `cargo-deny`/`cargo-vet` note in the dependency-discipline rules.

## Outbound — what this doc authorizes (and forbids)

pi's most conspicuous gap is stated plainly in its own README: **pi has no
permission system; containerize it yourself.** That is precisely the hole
agent-bridle fills. pi's extension model (see
`packages/coding-agent/examples/extensions/gondolin/`) already demonstrates
*replacing a built-in tool's operations* — gondolin routes pi's `bash` into a
micro-VM. The same seam can route pi's `bash`/`user_bash` through agent-bridle's
Caveats-confined shell, closing the confused-deputy gap structurally instead of
by sandboxing the whole process.

**Authorized:** build that extension **in our own repos** (it lives at
agent-bridle `integrations/pi-bridle/`), to prove the leash is useful against a
second, independent harness. This dogfoods agent-bridle's MCP frontend
(`agent-bridle-mcp`) as a universal bus and exercises the leash outside the
Rust agent line.

**Forbidden (for now):** any contribution into the `pi` repo itself — no
issues, no PRs, no upstream extension submission. pi's contribution gate is
strict (new-contributor issues/PRs auto-closed; `lgtm` required; core-bloat
rejected). We cross that boundary only after the extension has proven its worth
to us, and only as a deliberate, separately-decided step. See the
`memory_*`/MEMORY note and the user's standing instruction: *"don't cross into
pi development until we prove utility for ourselves."*

## Consequences

- This doc is the standing reference for "should newt adopt X from pi?" —
  if X is not on the Adopt/Consider lists above, the default answer is no, and
  changing that requires amending this doc.
- The `pi-bridle` experiment is scoped to our repos. A future decision doc (in
  agent-bridle) governs any actual upstream contribution.
- Revisit trigger: pi is under active development; if a future pi release lands
  a primitive that materially changes this analysis (e.g. pi adds its own
  capability system, or a Rust core), re-read and amend rather than assuming
  these notes still hold.
