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

**D3 — Auth posture: LAN-bind now, WebAuthn later.** v1 binds to the home LAN
behind the firewall/UFW (phone + MacBook testing per the operator's ask);
`agent-bridle-gateway`'s WebAuthn-presence pattern is the W8 hardening path.
Never a public bind.

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
- [ ] **W8** — ops: WebAuthn hardening path, reconnect, README, release call

## Craft

Per the Craft Register (`steward-charter/docs/CRAFT.md`): composition over
implementation (newt-web owns no agent logic), the characterization-net
discipline from #1319 applies to the web surface from W1 (HTML shell golden:
missing-fails, double-render determinism, negative control), one-issue-one-PR,
merge on green.
