# Decision: Newt Markup — one interaction model, many native views

**Status:** Proposed (authored for A0, #1823; **acceptance = human review/merge
of the A0 PR** — the epic names this document as the decision record to
ratify).
**Date:** 2026-08-25
**Related:** epic #1803 (normative source — where this document and the epic
disagree, fix this document), #1823 (A0: ratify + freeze the behavioral
inventory), `docs/decisions/plain_scroller_tui.md` (the amphibious
plain-scroller law this architecture must keep intact),
`docs/decisions/lean_rich_tui_morphologies.md`,
`docs/decisions/operating_modes_and_permission_postures.md` and the Bridle
`Gate` (domain authority stays there), `docs/decisions/newt_web_htmx.md` +
`docs/decisions/newt_web_docking.md` (the web view and its single-writer /
mirror-and-inject rules), `docs/decisions/agent_line_architecture.md`
(contracts survive a rewrite; implementations do not).

---

## TL;DR

Newt gets **one** interaction/design language — **Newt Markup**, a
progressive-enhancement superset of GFM Markdown backed by a renderer-neutral
interaction protocol and controller — instead of separate terminal, RichTUI,
web, and future-mobile interpretations of prompts, forms, dialogs, notices,
progress, tables, and diagrams. Stripping the versioned metadata always
leaves a useful Markdown document; Newt-aware views upgrade the same
definition into native controls. Authority never lives in markup or a
renderer.

## Architecture — a deliberate MVC variation

- **Model:** an immutable, semantic `InteractionDefinition`, plus a
  host-minted, out-of-band `InteractionInstance` carrying all mutable runtime
  state.
- **Interaction controller:** lifecycle, frozen responder policy, validation,
  and exactly-once typed response resolution.
- **Views:** replaceable projections — Lean interactive TTY (canonical
  scrolled Markdown/plain/ANSI fallback), headless/noninteractive (useful
  fallback without waiting or choosing), RichTUI (Ratatui-native controls),
  newt-web (sanitized HTML + native form controls), future mobile — that
  never own authority or durable state.
- **Domain authority:** the existing permission policy and the Bridle
  `Gate`, never markup and never a renderer.

## Governing laws

1. Strip Newt metadata and useful valid GFM remains.
2. Behavior and authorization are typed; labels, Markdown links, hotkeys,
   CSS, DOM values, and terminal rows never confer authority.
3. One semantic model feeds every view; no terminal/web/mobile model forks.
4. Surface features and frozen responder eligibility are separate types;
   domain authority remains in permission policy and Bridle.
5. Unknown content falls back visibly; unknown required behavior fails
   closed.
6. RichTUI stays feature-/TTY-gated and outward of the renderer-neutral
   core.
7. Lean/headless output stays canonical, portable, and protocol-safe.
8. Views are pure projections; durable state and lifecycle live in the
   controller/store.
9. Every control has accessible semantics and a plain/keyboard fallback.
10. Every migration child deletes a predecessor or names a time-bounded
    removal issue. A second permanent path is a regression.
11. Authored/model/tool/remote/browser markup is untrusted and confers zero
    authority. Only a scoped, host-minted instance is actionable.
12. Definitions and transcript bytes are immutable. Instance state,
    progress, expiry, responses, and resolution travel out of band.

## Format decision (v1)

Newt Markup v1 reuses Newt's existing `+++` TOML-front-matter convention
(the role-profile grammar) followed by a GFM body: human-editable,
structurally parseable, cleanly strippable.

- **Front matter (typed):** versioned type, stable definition/control IDs,
  interaction kind, immutable lifecycle semantics, controls, validation,
  typed values.
- **Markdown body:** all readable content and fallback choices. Generated
  documents derive fallback from the typed model; hand-authored documents
  are linted so metadata and fallback cannot drift.
- **Runtime sidecar (deliberately out-of-band):** fresh unguessable
  instance nonce, exact definition/form digest, revision, TTL,
  workspace/audience scope, frozen responder policy, provenance, progress,
  resolution. A response binds type + definition + instance + digest +
  revision + control values + idempotency key + responder provenance.
  **IDs route; they are not credentials.**

### Rejected for v1

- **Hidden HTML comments** — parser/sanitizer variance and
  comment-termination hazards.
- **Custom directives/containers** — potentially useful for future
  composite documents, but unnecessary grammar before
  one-document/one-interaction is proven.
- **Sidecars as the authored definition** — breaks copy/paste of semantic
  intent. Runtime sidecars remain required so mutable state never rewrites
  the source.

## Why now (the boundary is missing, not the pieces)

- `newt_core::Question` is serializable and shared by terminal and web, but
  `terminal_text()` embeds presentation in the semantic type.
- Permission construction branches on `PromptSurface::Terminal/Web`, mixing
  policy with renderer identity.
- Modal readers and `SurfaceRequest::ReadLine` receive formatted strings
  after semantic information has been discarded.
- TUI and web enable different Markdown dialects and soft-break behavior.
- Tables, notices, progress, setup questions, crew forms, and dialog-like
  flows each have multiple format/input paths.
- The web progressively enhances Mermaid today with no shared diagram
  contract or fallback rule.

Adding more surface-specific fixes compounds the problem. The epic
establishes the boundary once, proves it through permissions (B0), then
migrates and deletes old paths one vertical slice at a time — each child
carries a deletion gate (law 10).

## Consequences

- The migration proceeds along #1803's dependency ladder
  (A0 → A1 → A2 → A3 → B0 spine; then C/D/E families per the epic's
  ordering rule); each child issue restates its predecessor's deletion
  gate AND its exact focused test before implementation begins.
- A0 freezes today's behavior first: the behavioral inventory
  (`docs/findings/`), goldens/tests for the current contract — including
  the **deliberately different** terminal/web permission action matrices,
  frozen as-is — and an anti-sprawl ratchet so duplicate counts can only
  go down during migration.
- The protocol layer takes no Ratatui, crossterm, Axum, HTMX, ammonia,
  browser, mobile, filesystem, or application dependency (A2's dependency
  guard); `--no-default-features`/headless/wyvern guards prove rich
  renderer dependencies never leak inward.
- Non-goals stay non-goals (all twelve, per the epic): no replacement for
  Markdown or a custom pixel/cell layout language; no pixel-identical
  rendering across platforms; no authorization encoded in prose, links,
  hotkeys, CSS, DOM, or widget state; no mutable runtime state encoded in
  Markdown/front matter; no Ratatui, browser, Mermaid-JS, or mobile
  dependencies in the core protocol; no treating TUI-internal
  `SurfaceRequest` as the durable/cross-process protocol; no general
  web/mobile co-driving of the single-writer conversation; no generic
  layout tree, generic MVC framework, or second capability system; no
  arbitrary executable markup plugins; not every form control or Mermaid
  grammar production in the first slice; no big-bang TUI rewrite; no
  publishing crates before the seams have two production consumers.
