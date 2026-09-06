# RFC: A Coaching-Persona Capability for newt-agent

**Status:** Draft — plan only, no implementation in this PR
**Type:** Design / planning RFC
**Audience:** newt-agent maintainers and reviewers

---

## 1. Summary

This RFC proposes turning newt-agent into a faithful, generic host for a **read-only "judgment-training" coaching persona**: an operator working an on-call rotation pastes an alert or incident, and the agent **coaches rather than acts** — it presents decisions, trade-offs, prior-incident reasoning, and Socratic questions, and it derives **specific, copy-pasteable runtime commands and diagnostics** from **generalized documentation** (an enterprise wiki treated as the source of truth). The path from doc to command works by matching the alert against a **versioned registry** that maps it to a runbook/skill/doc-URL plus `{host}`-parameterized command templates. The persona reaches enterprise MCP servers directly (knowledge search, wiki, ticketing, paging, monitoring) to pull live state, but it **never executes host mutations**: an object-capability grant plus a grant-independent hard deny-list is enforced by filtering tools before they are advertised and re-checking every call's name **and** argument values at execution time. newt-agent already supplies most of the substrate; the coaching use-case mainly exposes a consistent theme — governance metadata is **parsed but not enforced**, the doc→command path is a **closed 6-phase enum** rather than a general registry, and the default "soul" is aggressively doer-oriented and fights any coach overlay. Closing those gaps is the subject of this RFC.

---

## 2. Motivation / Use-case

Consider a generic **enterprise on-call coaching assistant**. A newer operator picks up a shift on a rotation covering a fleet of build/version-control/monitoring infrastructure. An alert fires. The operator does not yet have the tribal knowledge held by a handful of senior engineers, and the authoritative runbooks are written **generally** ("check the replication lag on the affected instance") rather than as ready-to-run commands.

The coaching assistant's job is **judgment training**, not automation:

- **Coach, not driver.** It presents the decision the operator faces, the trade-offs, how similar incidents were reasoned about before, and asks one focused question at a time to build the operator's mental model. It does **not** edit files, restart services, or resolve incidents.
- **Generalized doc → specific command.** It reads the general runbook from the wiki (e.g. Confluence) or a knowledge-search tool (e.g. Glean), matches the alert to a playbook entry, and interpolates concrete host parameters so the operator gets an exact, copy-pasteable diagnostic command plus the doc link it came from.
- **Live read-only state.** It pulls current state from read-only enterprise MCP servers — ticketing (e.g. Jira), paging (e.g. PagerDuty), monitoring (e.g. Prometheus/Nagios) — to ground its coaching in what is actually happening.
- **Provably safe.** Regardless of what any connected server advertises, the assistant can never `ssh`, write/delete a file, bounce a service, or create/resolve an incident. That guarantee must be enforced by the host, not merely requested in a prompt.

All vendor names above are examples of a **category** (knowledge search / wiki / ticketing / paging / monitoring), named with "e.g." No specific employer, product, team, host, or ticket is assumed. The design targets any organization whose runbooks live in a wiki and whose live state lives behind MCP-reachable read APIs.

---

## 3. Current state of newt-agent

newt-agent already provides most of the building blocks this use-case needs. The relevant substrate:

- **Persona / role-profile system** — `newt-core/src/role_profile.rs` parses a persona's `tools` allow-list, a `[caveats]` `CaveatProfile`, and a `model`/`tier`; `/persona show` displays them. Personas are markdown + TOML front-matter overlaid onto the system prompt (`newt-tui/src/lib.rs`, `build_system_prompt_with_persona`, `PersonaStore`).
- **Object-capability caveat lattice** — a clamp-only caveat/OCAP gate attenuates fs/exec/net axes for **built-in** tools, with named permission-preset clamps enforced via `[modes]`.
- **Agentic tool-call loop** — `newt-core/src/agentic/tools.rs` (`merged_tool_definitions`, `execute_tool`) drives the model's tool calls; `request_user_input` (with `clarify`/`ask_question` aliases) is always advertised and degrades honestly headless.
- **External MCP consumption** — `newt-core/src/mcp.rs`, `newt-mcp-client`, `newt-tui/src/mcp.rs`: consumes arbitrary **stdio + streamable-HTTP** MCP servers declared in config or reused from an existing agent config, discovers tools, namespaces them `server__tool`, and injects OAuth 2.1 Bearer tokens under a secure-by-default transport policy.
- **Tier router** — a FAST/STANDARD/COMPLEX/REVIEW classifier selects a backend; `[loadouts]` bind provider/model/role.
- **Skills with progressive disclosure** — `newt-skills/src/lib.rs` exposes a static name/description/`when_to_use` index block; `tool_search`/`use_skill` provide lazy disclosure.
- **Lifecycle/tooling packs** — `newt-core/src/tooling.rs` maps a **fixed** 6-phase vocabulary (setup/format/lint/test/check/clean) to a repo's resolved command as pure data via the `lifecycle` tool — the right data-over-logic shape, but a closed enum.
- **Local-first inference** — local/air-gapped model backends alongside an OpenAI-compatible endpoint.
- **Python bindings** — `newt-agent-py` exposes primitives (inference backends, coder prompt build, file tools, MCP dispatch shell). The async agent loop (`Coder::run`) is intentionally **not** bound.

The recurring limitation: this governance metadata (persona `tools`/caveats, skill caveats, remote-tool leash) is **parsed and displayed but not enforced** at dispatch, and there is no general alert→playbook registry.

---

## 4. Gap analysis

| Capability | newt-agent today | Needed for the coach | Gap |
|---|---|---|---|
| Read-only "coach, not driver" altitude | `DEFAULT_SOUL` (`newt-core/src/memory.rs`) is strongly doer-oriented ("never describe a change, make it") and discourages narration | A first-class advise/guide altitude that hands over the exact command + reasoning instead of executing | No soul variant or altitude flag; coaching is prose that fights the built-in soul |
| Persona-scoped capability enforcement | `RoleProfile` parses `tools` + `[caveats]` but `merged_tool_definitions` does not filter and caveats are not meet-ed into the session ("enforcement is a follow-up"); only `[modes]` clamp is enforced | A persona's read-only grant genuinely restricts what the model sees and can call | Governance metadata is advisory only |
| OCAP leash on **remote** MCP tools | Built-ins run under the gate; remote tools (`newt-tui/src/mcp.rs` `call()`, dispatched in `agentic/tools.rs` `execute_tool`) carry **no** caveats leash | A ticketing/paging/monitoring server advertising create/resolve/write must be vetoed for a read-only coach | No per-remote-tool classification, approval, or scoping |
| Grant-independent absolute deny-list (name **and** args) | Caveats attenuate axes for built-ins; no fixed deny-list inspects argument **values**, and it does not span remote tools | Absolute veto of ssh/exec/shell, write/delete, service bounce, incident create/resolve — regardless of grant, on name and args | No argument-scanning deny-list; a generic `run_command` could smuggle a payload |
| Persona ↔ skill binding | `RoleProfile` has no `skills` field; `[modes]` has skill-no-role, `[loadouts]` has role-no-skill | Activating the coach preloads its coaching `SKILL.md` bodies | No way for a persona to declare required skills; wired manually per session |
| Enterprise MCP consumption | **Strong**: stdio + streamable-HTTP, tool discovery, namespacing, OAuth Bearer injection | Fan-out to several read-only servers with token brokering | Mostly present; servers connect once at start, every tool floods the catalog, SSE-only servers skipped, OAuth needs pre-existing client registration |
| Doc→command: alert → runbook/skill/doc-URL + `{host}` template | `tooling.rs` maps a **fixed** 6-phase enum to a resolved command as data | A general, versioned registry mapping arbitrary alert keys to playbook + template, with `{placeholder}` interpolation, path sandboxing, URL allowlisting | No open alert→playbook registry; the enum cannot express it |
| Doc fetch + source-of-truth vs. distillation | Skills are static markdown; the model can pull a page via MCP but nothing caches it or records provenance | Fetch + cache a wiki page as markdown, tag a skill with its source URL, validate/regenerate against fresh source | No doc-cache and no skill→source provenance link |
| Semantic retrieval over a large corpus | Static name/description/`when_to_use` index; `tool_search` keyword-ranks the catalog | Rank many docs/skills by semantic similarity to stay in budget | No semantic/RAG layer; keyword only |
| Complexity-tier routing per persona | Router exists; loadouts bind provider/model/role | Persona's declared `model`/`tier` drives backend selection | `RoleProfile.model`/`tier` parsed but not fed to the router |
| Structured one-question-at-a-time interview | `request_user_input` exists, always advertised | Socratic multiple-choice, one-at-a-time interview | Single-shot string only; `options` hint deferred |
| Durable shift-length state across compaction | Conversation-context architecture designed but largely roadmap / in-progress | Shift log + per-item tracker + handoff summary surviving 7+ hrs and `/compress` | Persistent session-long store not shipped |
| Governed persona on headless surfaces | `newt mcp` / `newt worker` expose dev tools; they do not consume `RoleProfile` | Headless doc→command flow minting the coach's tool-set + authority | Persona selection and its clamp unavailable headless |
| Observability of the gate decision per turn | Loop runs tools; no structured trace of tier + endpoint + gate decisions | Auditable trace proving the read-only posture held | No per-turn governance trace |
| Trust-tiering of external content + URL allowlist | MCP results flattened to text and fed to the model; `web_fetch` exists; transport allowlists hosts for Bearer but content is not marked untrusted | Treat bug/wiki/chat text as data-not-instructions; allowlist domains before a browser open | No untrusted-content wrapping and no browser-open allowlist |
| Turnkey Python "run a governed agent conversation" API | `newt-agent-py` exposes primitives only; agent loop not bound; pip CLI planned | A python+markdown host driving a multi-turn coach without reassembling the loop | No bound high-level agent-conversation entrypoint |

---

## 5. Proposed feature requests

Sizes use a Fibonacci scale (1/2/3/5/7/13). All interface sketches are **proposed** and illustrative only.

### FR-1 — Enforce `RoleProfile` tools allow-list and caveats at tool dispatch
**Rationale:** A coaching persona's read-only intent is currently advisory prose. The parsed `tools` allow-list and `[caveats]` must actually restrict what the model is shown and can execute.
**Target crates:** `newt-core` (`role_profile` + `agentic/tools`).
**Proposed sketch:**
```rust
// proposed — newt-core/src/agentic/tools.rs
fn merged_tool_definitions(session: &Session) -> Vec<ToolDef> {
    let all = discover_all_tools(session);
    match session.active_persona() {
        Some(p) => all.into_iter()
            .filter(|t| p.tools_allow.matches(&t.name))
            .collect(),
        None => all,
    }
}
// execute_tool: additionally meet() the persona's to_caveats() into the
// session caveats and refuse any call outside the allow-list.
```
**Acceptance:** With a persona declaring `tools=[...]` and read-only `[caveats]`, `merged_tool_definitions` advertises only allowed tools and `execute_tool` refuses any tool outside the allow-list or violating the meet-ed session caveats; unit + integration tests cover an allowed read and a denied write.
**Size:** 5

### FR-2 — Extend the object-capability leash to remote MCP tools
**Rationale:** Remote enterprise tools run today with no caveats leash, so a read-only coach cannot be prevented from calling a create/resolve/write tool a server advertises.
**Target crates:** `newt-tui` (`mcp.call`) + `newt-core` (`agentic/tools` `execute_tool`).
**Acceptance:** A connected server advertising a mutating tool (e.g. `incident.create`) has that tool withheld from a read-only persona's catalog and, if the model still emits the call, dispatch is vetoed with a gate reason; a `metrics.read` tool from the same server is allowed.
**Size:** 5

### FR-3 — Grant-independent absolute deny-list scanning tool name **and** argument values
**Rationale:** Defense-in-depth requires a fixed veto that no capability unlocks and that inspects argument values, so a generic tool cannot smuggle a host command.
**Target crates:** `newt-core` (new `agentic/deny.rs` used by `agentic/tools.rs`).
**Proposed sketch:**
```rust
// proposed — newt-core/src/agentic/deny.rs
// Grant-independent. No capability unlocks these.
const DENY_NAME: &[&str] = &["ssh","scp","sftp","rsync","exec","shell","bash","subprocess"];
const DENY_ARG_PATTERNS: &[&str] = &[
    r"\bssh\b", r"\brm\s+-", r"\bsystemctl\s+(restart|stop|start)\b",
    r"\bservice\s+\S+\s+(restart|stop)\b", r">\s*/", // write redirection
];
fn deny_check(name: &str, args_json: &str) -> Result<(), DenyReason> { /* … */ }
```
**Acceptance:** Any built-in or remote call whose name **or** any argument value matches a deny pattern is refused regardless of persona grant; tests cover a generic `run_command`/remote tool carrying an ssh/rm/service-restart payload in its args.
**Size:** 3

### FR-4 — Add a `skills` field to `RoleProfile` and preload declared skills on activation
**Rationale:** A coach persona should bundle its coaching procedures; role and skill currently live in disjoint config surfaces.
**Target crates:** `newt-core` (`role_profile`) + `newt-tui` (`PersonaStore`) + `newt-skills`.
**Acceptance:** A persona whose front-matter lists `skills=[...]` causes those `SKILL.md` bodies to resolve from `skill_search_dirs` and become available (index + `use_skill`) as soon as the persona is set; `/persona show` lists the bound skills.
**Size:** 3

### FR-5 — Coaching/advise soul altitude selectable by persona
**Rationale:** `DEFAULT_SOUL` is aggressively doer-oriented and fights any coach overlay; the coach must present commands + reasoning and defer the call to the human.
**Target crates:** `newt-core` (`memory` `DEFAULT_SOUL`) + `newt-tui` (`build_system_prompt_with_persona`).
**Proposed sketch:**
```rust
// proposed — newt-core/src/memory.rs
pub const COACH_SOUL: &str = "\
You are a coach, not a driver. Present the exact command and the reasoning \
behind it; explain the trade-offs; ask one focused question at a time. \
Never edit files, run mutating commands, or resolve incidents — hand the \
decision to the operator.";
```
**Acceptance:** A persona (or `--altitude` flag) selecting the coach altitude replaces the doer framing so a scripted incident turn yields a presented command + explanation and does **not** auto-edit or auto-run; a golden-transcript test asserts no mutating tool call is emitted.
**Size:** 2

### FR-6 — Feed `RoleProfile` model/tier into the tier router
**Rationale:** Persona-declared model/tier is parsed but not routed, so a coach cannot pin cheap-vs-reasoning tiers per turn.
**Target crates:** `newt-core` (router + loadout/role wiring).
**Acceptance:** Setting a persona with model/tier routes subsequent completions to that backend/tier; a test asserts the selected endpoint matches the persona and that FAST vs COMPLEX classification maps to the persona's `tier_map`.
**Size:** 3

### FR-7 — Per-persona MCP tool scoping and lazy disclosure
**Rationale:** Wiring several enterprise servers floods the model catalog and context window; a persona must be scoped to a subset of servers/tools, disclosed lazily.
**Target crates:** `newt-tui` (`mcp.tool_defs`) + `newt-core` (`agentic/tools` `merged_tool_definitions`, `tool_search`).
**Acceptance:** With 3 servers connected but a persona scoped to 1 (or a named tool subset), only that subset is advertised; remaining tools are discoverable via `tool_search` but not eagerly injected; a test asserts catalog size shrinks to the scoped set.
**Size:** 5

### FR-8 — Data-driven alert→playbook registry with safe placeholder substitution
**Rationale:** The core doc→command step: map an alert name/keyword to a runbook/skill/doc-URL and a `{host}`-parameterized command template, interpolated into concrete copy-pasteable diagnostics — generalizing the closed lifecycle enum.
**Target crates:** `newt-core` (new registry module modeled on `tooling.rs`) + a `playbook` tool in `agentic/tools`.
**Proposed sketch:**
```toml
# proposed — playbooks.toml (versioned, exact-key + regex fallback)
version = 1
domain_allowlist = ["wiki.example.com", "runbooks.example.com"]

[[playbook]]
key   = "DiskUsageHigh"                 # exact match
regex = "^disk[_-]?usage"               # fallback
skill = "skills/disk-pressure/SKILL.md" # must resolve inside repo root
doc   = "https://wiki.example.com/runbooks/disk-pressure"
command = "df -h {host}:/data && du -xh {host}:/data | sort -rh | head"
```
```rust
// proposed — resolve returns interpolated command + doc pointer as data;
// rejects skill/runbook paths outside repo root and URLs off the allowlist.
fn resolve_playbook(alert: &str, params: &Params) -> Result<PlaybookHit, RegistryError>;
```
**Acceptance:** Given a TOML/JSON registry entry (exact key + regex fallback) and a host param, newt resolves the entry and emits the interpolated command string and doc pointer; entries whose runbook/skill path resolves outside the repo root are rejected (path-traversal sandbox) and command/doc URLs outside the configured domain allowlist are refused; tests cover exact-match, regex fallback, traversal rejection, and URL-allowlist rejection.
**Size:** 8

### FR-9 — Doc-fetch-and-decode with source-of-truth provenance
**Rationale:** Skills are distillations, not the source; the coach must fetch the authoritative page via MCP, cache it as markdown, and record provenance so a skill can be validated/regenerated against its source.
**Target crates:** `newt-core` (new `doc_cache.rs`) + `newt-skills` (frontmatter `source`) + MCP client.
**Acceptance:** newt fetches a wiki/knowledge page through a configured MCP tool, caches it as markdown keyed by source URL, and a skill carrying a `source` URL can be diffed against a fresh fetch to report drift; test uses a mock MCP server returning a page body.
**Size:** 5

### FR-10 — Multiple-choice / one-at-a-time clarifying-question primitive
**Rationale:** Socratic coaching needs structured, one-question-at-a-time interviews; `request_user_input` is single-shot with no options hint today.
**Target crates:** `newt-core` (`agentic/tools` `request_user_input`).
**Acceptance:** `request_user_input` accepts an optional `options` list and renders a numbered choice interactively while degrading to a plain prompt headless; a test asserts the options round-trip and the selected value is returned to the model.
**Size:** 2

### FR-11 — Runtime MCP add/reconnect and per-connection resilience
**Rationale:** Servers connect only at session start; a coach cannot reach a new server mid-shift, and a dead server stays dead, breaking a 7+ hour session.
**Target crates:** `newt-tui` (`mcp` connection pool) + `newt-mcp-client`.
**Acceptance:** A slash command adds a server mid-session and its tools appear without restart; a server killed mid-session is transparently reconnected (or cleanly skipped with a warning) on next call; the per-server request timeout is configurable rather than fixed.
**Size:** 5

### FR-12 — Consume `RoleProfile` in the headless MCP and ACP worker entry points
**Rationale:** An MCP/ACP-driven doc→command flow must mint the coach persona's tool-set and read-only authority headlessly; those surfaces ignore `RoleProfile` today.
**Target crates:** `newt-mcp-server` (handlers) + `newt-acp-worker` + `newt-cli` (`Command` enum).
**Acceptance:** `newt mcp --persona <coach>` and `newt worker --persona <coach>` apply the persona's tool allow-list, caveats, and deny-list to every dispatched tool; a headless test asserts a mutating tool is vetoed.
**Size:** 3

### FR-13 — Per-turn governance trace (classification + endpoint + per-tool gate decision)
**Rationale:** The read-only posture must be auditable: which tier was chosen, which endpoint served it, and for each tool call whether the gate allowed or denied it and why.
**Target crates:** `newt-core` (agentic-loop instrumentation).
**Acceptance:** Each turn emits a structured trace record containing the classification tier, resolved endpoint, and an entry per tool call with allow/deny + reason; a test asserts a denied write appears in the trace with its deny reason.
**Size:** 3

### FR-14 — Untrusted-content wrapping and browser-open URL-domain allowlist
**Rationale:** External MCP-fetched content (bug bodies, wiki text, chat) must be treated as data not instructions, and any browser-open must be domain-allowlisted (prompt-injection + navigation safety).
**Target crates:** `newt-core` (agentic prompt assembly + `web_fetch`/`open`) + `newt-tui`.
**Acceptance:** MCP tool results and fetched pages are inserted into context wrapped with an untrusted-data delimiter and an injection-guard note; an attempt to open a non-allowlisted domain is refused; tests cover an injected "ignore previous instructions" payload surfaced as data and a blocked non-allowlisted open.
**Size:** 3

### FR-15 — Ship the file-backed `ConversationStore` for durable shift-length state
**Rationale:** A multi-hour coaching shift needs an append-only shift log + per-item status tracker that survives `/compress` and process restart; the store is designed but not shipped.
**Target crates:** `newt-core` (conversation context / `ConversationStore` file backend).
**Acceptance:** Within a folder-derived conversation, an append-only log and a per-item status map persist across `/compress` and a process restart in the same folder; a test writes items, compacts, restarts, and reads them back.
**Size:** 8

### FR-16 — Seed a shipped coaching persona into the per-user personas dir on first run
**Rationale:** Persona templates ship in-repo but there is no runtime seeding, so a fresh install has no coach available; the coach should be present like the default `coder`.
**Target crates:** `newt-tui` (`PersonaStore`) + `personas/`.
**Acceptance:** A fresh install exposes a read-only coach persona via `/persona list` without hand-installing a file; the seeded file carries a read-only `[caveats]` block and coach-altitude framing.
**Size:** 2

### FR-17 — Bind a turnkey "run a governed agent conversation" Python entrypoint
**Rationale:** A python+markdown coaching host currently must reassemble the loop from primitives; binding the agent loop lets a Python app drive a governed multi-turn coach against local inference.
**Target crates:** `newt-agent-py` + `newt-core` (agent loop) / `newt-coder`.
**Acceptance:** From Python, a single call drives a multi-turn conversation that honors a selected persona's caveats/deny-list, executes local-inference tool calls, and returns the transcript; an example script coaches through a mock incident end-to-end.
**Size:** 8

---

## 6. Suggested changes (per crate)

- **Persona enforcement.** Add `skills: Vec<String>` to `RoleProfile`; have `is_role_bound()`/`parse()` carry it; have `build_system_prompt_with_persona` + the tool catalog honor the persona's `tools` allow-list and meet its `to_caveats()` into the session instead of only displaying them. *(`newt-core/src/role_profile.rs`, `newt-tui/src/lib.rs`, `newt-core/src/agentic/tools.rs`)*
- **Remote-tool governance.** Route every `server__tool` call through the caveats + absolute deny-list gate before `mcp.call()`, classifying by tool name and argument values; withhold ungranted remote tools from `tool_defs()` output. *(`newt-tui/src/mcp.rs`, `newt-core/src/agentic/tools.rs` `execute_tool`)*
- **Absolute deny-list.** Add a fixed, grant-independent deny module (ssh/scp/sftp/rsync/exec/shell/bash/subprocess, file write/delete, service bounce/restart, incident create/resolve/delete) that scans tool name and serialized args; call it from both built-in and remote dispatch paths. *(new `newt-core/src/agentic/deny.rs`, `newt-core/src/agentic/tools.rs`)*
- **Coach altitude.** Add a `COACH_SOUL` constant (advise/guide framing) selectable via persona front-matter or `--altitude`, replacing `DEFAULT_SOUL` in the assembled prompt. *(`newt-core/src/memory.rs`, `newt-tui/src/lib.rs`)*
- **Config surface unification.** Let a binding carry **both** role and skill: add a `role` to `[modes.<name>]` (`ModeConfig`) or a `skills` to `[loadouts.<name>]` (`Loadout`) so a single `/mode` or `--loadout` activates persona + skill + read-only preset atomically. *(`ModeConfig`: `newt-core/src/config/permissions.rs`; `Loadout`: `newt-core/src/config/loadout.rs`)*
- **Doc→command registry.** Generalize the fixed `Phase` enum into a data-driven named-command/alert registry (exact-key + regex fallback) with `{placeholder}` interpolation, repo-root path-traversal sandboxing, and URL-domain allowlisting; expose a `playbook` tool alongside `lifecycle`. *(`newt-core/src/tooling.rs`, `newt-core/src/agentic/tools.rs`)*
- **Doc provenance.** Add an optional `source` (authoritative doc URL) to Skill `Frontmatter` and a small markdown doc-cache keyed by source URL; add a validate/regenerate path that diffs a distilled skill against a fresh MCP fetch. *(`newt-skills/src/lib.rs`, new `newt-core/src/doc_cache.rs`)*
- **Clarifying-question loop.** Add an optional `options` field to the `request_user_input` tool schema; render a numbered choice interactively, degrading to a plain prompt headless. *(`newt-core/src/agentic/tools.rs`)*
- **Router wiring.** Feed `RoleProfile.model`/`tier` into the tier router so persona selection actually chooses the backend/tier per classified turn. *(`newt-core/src/config.rs`, router module, `newt-tui/src/lib.rs`)*
- **Headless persona.** Accept `--persona` in `newt mcp` and `newt worker` and apply the persona's tool-set/caveats/deny-list to every handler dispatch. *(`newt-cli/src/lib.rs`, `newt-mcp-server/src/handlers.rs`, `newt-acp-worker`)*
- **MCP resilience.** Add runtime add/reconnect + per-server health-check to the connection pool and make the per-request timeout configurable. *(`newt-tui/src/mcp.rs`, `newt-mcp-client/src/lib.rs`)*
- **Persona seeding.** Ship a read-only coach persona template and seed it into the per-user personas dir on first run alongside the default `coder`. *(`personas/coach.md`, `newt-tui/src/lib.rs` `PersonaStore`)*

---

## 7. Non-goals / out of scope

- **No implementation in this PR.** This is a plan. The only code here is small, clearly-marked **proposed** interface sketches (Rust trait/function signatures and config YAML/TOML); none of it is wired.
- **Semantic retrieval / RAG over the doc corpus is deferred.** Keyword `tool_search` remains for now; whether semantic ranking lives inside newt or is delegated to an external knowledge-search MCP server is an open question (see §9).
- **Legacy SSE-only MCP transport is not committed to** here; stdio + streamable-HTTP remain the supported transports (open question in §9).
- **Non-interactive / service-account OAuth** (`client_credentials`) is not designed here; the existing PKCE browser flow is assumed sufficient for a single operator's interactive shift (open question in §9).
- **No new inference backend or model.** The coach reuses existing local-first and OpenAI-compatible endpoints.
- **No enterprise-specific runbook content ships** in newt-agent; the registry and skills are authored by the deploying organization against its own wiki.

---

## 8. Sanitization checklist (pre-publish verification)

This document is destined for a public repository. Before publishing, verify:

- [ ] Scrub company/team identity: no employer name, team name, on-call product name, team slug, or squad names; use generic phrasing ("an on-call rotation", "the SCM team", "a squad").
- [ ] Remove all internal hostnames and host-naming conventions (build/master/gateway/proxy hosts, instance IDs, CNAMEs, datacenter-prefix→city maps); use `<host>`, `<build-host>`, `<instance>` placeholders.
- [ ] Remove version-control instance/port specifics and internal path patterns; genericize to `<instance:port>` and `<path>`.
- [ ] Remove all internal URLs/domains (wiki base + space keys + page IDs, dashboards + UIDs, monitoring host:port, bug-tracker/help-desk hosts, inference API base URL, local model ports, install/pages URLs); replace with `example.com` placeholders.
- [ ] Remove all ticket/record identifiers (internal bug numbers, ticket project-key issues, incident numbers, page IDs, project/snippet IDs).
- [ ] Remove paging service IDs, schedule IDs, and inlined service-name constants — no real IDs in examples.
- [ ] Remove all employee/personal identifiers (SME names, usernames, account IDs, `@company.com` emails, escalation tables, author/stakeholder blocks, skill YAML author fields, cron MAILTO); use `<sme>`, `<on-call-lead>`.
- [ ] Remove internal chat channel names, distribution-list names, wiki space keys, onboarding links.
- [ ] Genericize in-house tooling/product names; describe by role only ("an enterprise personal-assistant CLI", "an internal sync tool").
- [ ] Genericize inference identifiers; describe as "an internal OpenAI-compatible inference endpoint" and "an optional local/air-gapped model backend".
- [ ] Remove datacenter/site references and migration/decommission status remarks.
- [ ] Remove on-prem operational specifics quoted from registry notes (absolute log/cron paths, admin script names, NFS mount behavior, replica ports, named recovery procedures).
- [ ] Remove auth-file layout hints for internal systems; keep only a generic "env → OS keyring → file(0600) → secrets-manager" resolution order.
- [ ] Remove the repo's own internal clone URLs, project IDs, and snippet IDs from help text/examples.
- [ ] Keep (safe, no scrubbing): the four-layer decision/reasoning/outcome/principle concept, the progressive-disclosure cost hierarchy, the object-capability grant + hard deny-list design, the "coach not driver" posture, and the generic MCP server categories named only by role with vendor examples clearly marked "e.g.".
- [ ] Final pass: grep the PR text for internal markers (employer name, bug-tracker prefix, `confluence.`, dashboard/monitoring hostnames, `.internal`, datacenter host prefixes, version-control port suffixes, `@`, and any bare 4–7 digit numbers) before publishing.

---

## 9. Open questions

1. **Grant model.** Should the coach's read-only posture be a shipped named permission preset (clamp-only, reusable via `/mode`) or a **new grant vocabulary** (`metrics.read`/`forensics.read`/`query`/`ack`)? The four named capabilities do not map 1:1 onto newt's current fs/exec/net axes.
2. **Deny-list location.** Should the absolute deny-list be a hardcoded `newt-core` constant (guaranteed, non-configurable) or data-driven config (flexible but wideable)? The requirement is explicitly "no capability unlocks it."
3. **Registry seam.** Is the alert→playbook registry better as a first-class `newt-core` subsystem (FR-8) or authored entirely as a bundled `SKILL.md` with scripts, given the repo's data-over-logic preference? Where is the right seam between core and skill assets?
4. **Enterprise auth.** Does newt need `client_credentials`/service-account (non-interactive) OAuth for headless multi-tenant use, or is the existing PKCE browser flow + refresh-token store sufficient for a single operator's interactive shift?
5. **SSE transport.** Should legacy SSE-only MCP transport be implemented (some enterprise deployments are SSE-only), or is it acceptable to require stdio/streamable-HTTP and skip SSE with a warning?
6. **RAG ownership.** Does the semantic-retrieval layer belong inside newt, or should it be delegated to an external knowledge-search MCP server so newt stays a thin retrieval-over-MCP client?
7. **Config unification.** How should persona ↔ skill ↔ mode ↔ loadout be unified without four overlapping surfaces — is the cleanest fix a single "stance" object composing role + skills + preset + framing?
8. **Shift-state schema.** Should durable shift-state (FR-15) reuse the in-progress `ConversationStore` design as-is, or does coaching need a distinct append-only shift-log schema that ships sooner?
9. **Python scope.** Is binding the full agent loop (FR-17) in scope for `newt-agent-py`, or is the intended integration to shell out to the Rust `newt` binary / drive ACP JSON-RPC?