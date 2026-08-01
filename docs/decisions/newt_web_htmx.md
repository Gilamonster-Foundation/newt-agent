# Decision: newt-web — an HTMX web cockpit over newt's published seams

**Status:** Accepted (plan approved by Shawn Hartsock, 2026-07-22; ladder +
decision tags marked up on #1331).
**Date:** 2026-07-22

## The thing being decided

A set of newt-agent instances on one shared Linux box, surfaced in a browser:
each agent a tab, attach/detach independently, `/attach` issuable from inside a
running conversation, testable from a MacBook Air and a phone on the home
network (#1331).

## Decisions

**D1 — Placement: `newt-web/` is an excluded crate in this repo** (the
`newt-mesh` pattern: own `Cargo.lock`, `[workspace] exclude`, path deps). The
plain-scroller charter stands untouched: newt's chat surface stays a scroller;
newt-web is a **separate product binary composing published seams**
(`TurnDriver`, `ConversationStore`, agent-mesh presence) — the first true
second product of the kernel-first decomposition, and deliberately *not* a
member so its web dependency tree (axum et al.) never enters the agent's
workspace graph.

**D2 — Attach semantics: mirror + inject; co-drive never.** The store's
single-writer conversation claim is inviolate. A web tab attached to a running
session *mirrors* the transcript (store-follow, read-only by construction) and
*injects* prompts through the session's own input seam — the running newt
remains the sole writer, exactly as if the operator had typed. No second claim,
no co-driving, no claim theft. Detach closes the inbox; the session never
noticed anything but input.

**D3 — Auth posture (amended 2026-07-22, twice).** First: the browser path is
HTTPS + SSO at the ingress — the cluster's Authentik-backed oauth2-proxy
forward-auth chain gates every request; newt-web is ClusterIP-only (the
unauthenticated NodePort was closed the day it existed). Never a public bind.

Second (operator ruling): **auth self-sufficiency, fail-closed.** newt-web
must not hard-depend on a resident IdP. The tiers:

1. **Ingress SSO present** (Authentik/oauth2-proxy, the deployed state) —
   newt-web trusts the forward-auth boundary; the ingress is the gate.
2. **No IdP available** — newt-web stands up its OWN gate: WebAuthn passkey
   enrollment on first run (the `agent-bridle-gateway` presence pattern —
   enroll once, assert per session), with a printed one-time enrollment token
   as the bootstrap (the Jupyter pattern).
3. **No gate at all** — web chat is DISABLED. An absent IdP is a named CHORE
   ("enable auth to open the cockpit"), never an open door. The cockpit
   renders the chore, not the tabs.

Tier detection is explicit config, not sniffing (`NEWT_WEB_AUTH =
ingress | webauthn | disabled-until-configured`); the default is tier 3 —
fail closed. Implementation is the W8 rung, promoted from "hardening" to a
release blocker for any deployment outside the SSO'd cluster.

**D4 — Release: out of the 0.8.0 gate.** newt-web versions independently until
it earns a release story (revisit at W8).

**D5 — The environment lives in this repo: `deploy/newt-web-dev/`.** The
repurposed drake-interactive image (Containerfile + manifests) is versioned
here so "tear down and rebuild" is one command. Identity is escrowed
cluster-side: the registered SSH host keys live in the
`drake-interactive-hostkeys` Secret (extracted 2026-07-22 from the live pod's
`/home/drake/.ssh-host-keys/` — the set sshd actually serves via `-h`; the
image-baked `/etc/ssh` keys were decoys), alongside `drake-interactive-keys`
(authorized_keys) and `drake-nats-nkey`. Manifests mount identity from
Secrets; nothing identity-bearing is baked into the image ever again. The
NodePort 30122 (SSH) is preserved; newt-web adds its own NodePort.

## Architecture

axum + HTMX + SSE. Server-rendered fragments; tabs = agents; per-agent SSE
stream of transcript deltas; a prompt box POSTs to the agent's drive seam.
HTMX means no JS toolchain — the whole front end is the server's templates.

**Canonical presentation contract (operator ruling, 2026-07-31).** Harness,
store, and transport output is GFM Markdown by default. A surface projects that
same source into its native medium: `newt-core` renders ANSI for a capable TTY,
while newt-web renders sanitized HTML and progressively enhances recognized
fenced blocks. Mermaid is the first enrichment (<code>```mermaid</code>); the browser
runtime is pinned and served locally, with strict security mode. Genuine Rich
TUI interactions may remain native widgets, but they need a Markdown
representation/fallback so the same workflow stays usable through headless,
web, and mobile adapters. HTMX controls are an input adapter, not a second
conversation format.

The browser acceptance seam follows the same split: deterministic headless BAT
runs on pull requests/main; phone-sized, user-driven UAT gates `release/**` and
can be dispatched manually. Both run against a local simulated backend. Live
model BAT/UAT remains the separate `eval-live.yml` tier.

Two agent sources, one tab model:
- **Owned agents** — spawned by newt-web, driven in-process via `TurnDriver`.
- **Followed sessions** — any conversation in the shared `ConversationStore`
  (read-only until W6's attach seam lands; then mirror + inject per D2).

## The ladder (epic checklist — one concern per PR, goldens byte-identical throughout)

- [x] **W-ENV.1** — host-key escrow to Secret (done 2026-07-22, non-destructive,
  wire fingerprint verified unchanged)
- [ ] **W0** — this decision record
- [ ] **W1** — crate scaffold + CI lane + shell BAT/golden (+ `deploy/newt-web-dev/`)
- [ ] **W2** — spawn-and-drive one agent (v0 core: POST /agents, prompt, SSE, delete)
- [ ] **W3** — tabs: N agents, independent lifecycles, view attach/detach
- [ ] **W4** — store-follow: read-only attach to any session on the box
- [ ] **W5** — presence over local mesh (signed announce; tab list = live agents)
- [ ] **W6** — the attach seam: `/attach` opens a provenance-tagged local inbox
  the REPL muxes with stdin; `/detach` closes it
- [ ] **W7** — v1: web tab ↔ running session (mirror + inject end-to-end)
- [ ] **W8** — auth self-sufficiency (tiered gate per amended D3: ingress SSO /
  own WebAuthn / fail-closed chore) + ops: reconnect, README, release call

## Craft

Per the Craft Register (`steward-charter/docs/CRAFT.md`): composition over
implementation (newt-web owns no agent logic), the characterization-net
discipline from #1319 applies to the web surface from W1 (HTML shell golden:
missing-fails, double-render determinism, negative control), one-issue-one-PR,
merge on green.
