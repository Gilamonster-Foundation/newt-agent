# The Permissions Facade: Presenting an OCAP Authority Algebra as Familiar Permissions

**Shawn Hartsock** · Gilamonster Foundation · `hartsock@users.noreply.github.com`

*Design doc + paper material. Companion to
[`caveat-lattice-paper.md`](./caveat-lattice-paper.md) and
[`authority-model-design-review.md`](./authority-model-design-review.md).*

> Pinned to `newt-agent` **HEAD `1a0fb88`** (`docs(ocap): authority-model design
> review + caveat-lattice paper`). All `file:line` citations are against that
> commit; where the upstream gather notes cited stale lines, the verified
> lines below supersede them.
>
> **Red-team revision (2026-06-29).** An adversarial review traced every
> load-bearing claim to source. It confirmed the algebra is sound and the model
> cannot self-amplify, but found that three invariants this document leaned on
> were overstated: the #307 preset meet is **opt-in via `/mode`** (off by
> default), the mint-gate re-roots at ⊤ (provenance, not a ceiling), and the
> shipped `--yolo` runs **unconfined**, contradicting §4.3. It also found the
> §2.1 `permissions.rs` citations were ~20 lines stale despite the "verified"
> claim. This revision corrects those citations, tightens §6's invariants to
> say what is actually true by default, fixes §4.3, and adds **§7 Soundness &
> threats** — each finding with the facade's answer or a noted residual. The
> honest one-line summary: *the facade is OCAP-sound in the narrow formal sense
> (meet-only, no model self-amplification, fail-closed) on every path; the
> stronger "a grant can never exceed the operator's **configured** ceiling"
> holds only with an active `/mode` preset — otherwise the boundary is a human
> reading a model-authored prompt, and the hardening in §7 is required before
> this is paper-grade.*

---

## 0. Reading guide

This document has two audiences and one spine.

- **For the repo owner** it is a *build plan*: §2 says what exists, §3–§5 say
  what to build, §6 states the invariants, **§7 stress-tests those invariants
  against the red-team and records the residuals**, and §8 sequences the PRs.
- **For the enterprise-OCAP paper series** it is the *third leg*. The
  caveat-lattice paper gave the **algebra** (a meet-semilattice, attenuation-
  only) and the **enforcement floor** (complete mediation). This document gives
  the **facade** — the human/model-facing surface that makes the algebra
  usable without weakening it. Algebra + enforcement + facade is the whole
  enterprise-OCAP system. §9 develops the paper framing.

The spine is the workspace thesis, applied to authority:

> *"Computer Science has as much to do with Computers as Astronomy does with
> Telescopes."* — Dijkstra

**The algebra is the sky. "Permissions" is the telescope.** OCAP is the sound
structure of authority — it is what is *true* about what an agent may do. But
nobody points at the sky with their bare eyes: capable models (Claude Opus 4.8)
and lay operators reach for the instrument they already know — *allow once /
allow for the session / deny* — not a lattice meet. The facade is that
instrument. It must faithfully image the sky; it must not repaint it.

---

## 1. Problem and thesis

### 1.1 The model-expectation gap (#766)

The caveat-lattice paper proves that authority can only ever *narrow*: every
composition is a meet, the implementation exposes no join, so a fully
compromised agent cannot exceed the down-set of what it was minted with. That
is the correct kernel. But it surfaces to the model as an *algebra*, and the
model does not speak algebra.

Field finding **#766**: capable models expect a **permissions UX** — "may I do
X? [permit once / permit always / deny once / deny always]" — and instead meet
a denial phrased as an authority-boundary fact:

```
exec of "export" is not within the granted authority
```

The model's mental model is *ask for permission*; OCAP's is *operate within
authority*. These are not the same speech act. A model that cannot translate
"you lack this capability" into "ask the human to grant it" simply **halts** —
the `mkdir` dead-end that motivated #721, observed again under the HotSeat
finding (a model `exec` denial under a non-permission-aware MCP surface). This
is a *semantic* mismatch, not a missing feature.

The lay operator has the dual problem: shown `Caveats { exec: Only({git,
cargo}), fs_write: Only({/work}), … }` and asked to edit `[tui.permissions]`,
they cannot reason about whether adding `rm` to an `Only` set is safe. They
want a doorbell, not a lattice.

### 1.2 Two registers, one authority

Following the Steward's Charter discipline of *two registers* (systems names in
code, plain gloss in commentary), the facade is a **translation layer between
two vocabularies for one underlying authority**:

| Algebra register (kernel)                     | Permissions register (facade)        |
|-----------------------------------------------|--------------------------------------|
| `Caveats` element of a meet-semilattice       | "what you're allowed to do"          |
| axis `exec`/`fs_read`/`fs_write`/`net`        | "running commands / reading / writing / network" |
| `meet` (greatest lower bound)                 | (never shown)                        |
| widen grant then re-clamp under preset        | "allow this"                         |
| `valid_for_generation` window                 | "for this session"                   |
| `attest` discharge (liveness)                 | "confirm with your fingerprint/key"  |
| denial = not in the down-set                  | "do you want to allow it? [y/n]"     |

### 1.3 Thesis

> **The facade presents OCAP as familiar permissions without weakening the
> algebra.** Every facade verb compiles down to an operation the algebra
> already sanctions — a single-op widen, a session-scoped caveat, a liveness-
> attested mint, or a plain denial. The model and the operator see only
> *allow X? [once / session / deny]*. They never see `Caveats`, never see
> `meet`, never see a generation window. The kernel's invariants (meet-only,
> complete mediation, fail-closed, mint-gate) are **preconditions of the
> facade, enforced beneath it** — the facade is *sugar over the algebra, never
> a bypass of it* (§6).

This is the precise sense in which permissions is the telescope: it changes
*what the user looks through*, not *what is there to see*.

---

## 2. What exists today

The first piece of the facade already shipped. This section is verified against
HEAD `1a0fb88`.

### 2.1 The permission surface

**`newt-core/src/agentic/permissions.rs`** — the kernel-facing seam. *(Line
numbers re-verified against HEAD `1a0fb88` in the 2026-06-29 red-team; the
24-line module header had shifted the prior citations ~20 lines.)*

- **`DenialKind`** (`permissions.rs:33`) — the capability axis a denial fell
  on: `Exec` | `FsRead` | `FsWrite` | `Net`.
- **`PermissionRequest`** (`permissions.rs:58`) — one denied capability framed
  for a human decision: `{ tool, kind: DenialKind, target, reason }`
  (`permissions.rs:60–68`). `target` is *what an allow would grant* — a command
  name (exec), an absolute path (fs), or a host (net) (`permissions.rs:65`).
  **`reason` is model-authored free text** (`permissions.rs:68`) and is
  rendered verbatim in the prompt; §7-F4 treats it as untrusted.
- **`PermissionDecision`** (`permissions.rs:72`) — the verdict:
  `Allow(Caveats)` ("re-execute under these freshly minted caveats — root ∪
  grant, session key untouched, value does not outlive the call",
  `permissions.rs:76`) or `Deny` ("keep the standard structured denial, bit-
  for-bit", `permissions.rs:78`).
- **`PermissionGate`** (`permissions.rs:97`) — the one human-interface seam,
  carrying two *distinct* interactions that share one operator presence:
  - `ask(&[PermissionRequest]) -> PermissionDecision` (`permissions.rs:101`) —
    the capability **GRANT** path (#263/#721). It can *widen* authority by
    re-minting caveats.
  - `ask_question(&str) -> Option<String>` (`permissions.rs:111`) — the generic
    free-text **Q&A** behind the `request_user_input` tool (#728). It only
    gathers text; it never touches authority. `None` ⇒ no human ⇒ recoverable.
- **`widen_caveats(base, grants)`** (`permissions.rs:120`) — builds the widened
  *policy*: each grant's target is inserted into its axis's `Only` set;
  `Scope::All` axes are untouched; **`max_calls` and `valid_for_generation` are
  never touched** (`permissions.rs:117`). It is a plain policy value — proving
  `base ⊑ root` and minting a signed key is the gate's job, not this function's.
  **Caveat (§7-F2):** the mint the gate performs re-roots from `Caveats::top()`,
  so the `base ⊑ root` it proves is `base ⊑ ⊤` — always true. The mint gives
  provenance and a signature, *not* a ceiling on the interactive grant; the only
  authority ceiling is the §6.1 preset meet (when a `/mode` is active) plus the
  operator at the prompt.
- **`PermissionRecord`** (`permissions.rs:145`) — every prompted decision
  logged as one JSONL line, with a `scope` of `"once"` or `"session"`
  (`permissions.rs:160`). It is a **review artifact, not config**: nothing
  reads it back into authority (`permissions.rs:1`). Promotion to a durable
  grant is a human editing `[tui.permissions]` (#181).

### 2.2 The allow-once/session/deny prompt — the first facade piece

**`newt-tui/src/lib.rs`** implements the interactive gate as exactly the
familiar permissions UX:

- **`permission_prompt_text`** (`lib.rs:1675`) renders the doorbell:
  ```text
  ⊘ {tool} wants to {verb} `{target}` — {axis}.
    ({reason})
    [a]llow once   [s]ession allow   [d]eny (default)   [D]eny always >
  ```
  The model/operator sees *verbs and a target*, never a `Caveats` value.
- **`parse_permission_choice`** (`lib.rs:1665`) maps keystrokes →
  `PromptChoice`: `a`→`AllowOnce`, `s`→`AllowSession`, `D`→`DenyAlways`,
  everything else (incl. empty) → `Deny` (default-deny, `lib.rs:1667`).
- **`PromptPermissionGate::ask`** (`lib.rs:1842`):
  - `[D]eny always` and prior session denials short-circuit with no re-prompt
    (`lib.rs:1849`); `session_denials: BTreeSet` (`lib.rs:1743`).
  - a prior session grant skips the prompt and auto-allows
    (`lib.rs:1860`); `session_grants: BTreeSet` (`lib.rs:1740`).
  - `AllowSession` inserts `(kind, target)` into `session_grants`
    (`lib.rs:1872`); `AllowOnce` pushes to a transient `once_grants` vec
    (`lib.rs:1868`).
  - returns `Allow(self.mint(&once_grants))` (`lib.rs:1891`).
- **`PromptPermissionGate::mint`** (`lib.rs:1816`) is the load-bearing bridge
  to the algebra:
  1. `policy = widen_caveats(base, session_grants ∪ once_grants)` (`lib.rs:1820`).
  2. **#307 FLOOR (conditional)** — `if let Some(clamp) = &self.preset_clamp {
     policy = policy.meet(clamp); }` (`lib.rs:1827–1828`): when a preset clamp is
     present, the widened policy is re-clamped under it, so a grant can never
     raise authority above the preset ceiling. **This is the meet that makes the
     facade sound *when a `/mode` is active* (§6.1) — but `preset_clamp` is
     `Option`, defaults `None` (`lib.rs:2159`), and is sourced *only* from an
     `active_mode` that starts `None` (`lib.rs:4110`) and is set *only* by the
     manual `/mode` command (`apply_mode`, `lib.rs:6554`). At dispatch,
     `preset_clamp = active_mode.as_ref().map(|m| m.clamp.clone())`
     (`lib.rs:5438`).** So **absent `/mode`, this `meet` does not run and there
     is no configured ceiling on grants** — the configured `[tui.permissions]`
     becomes the *widen baseline* (`base`), not a ceiling, and `widen_caveats`
     adds targets on top of it. This is finding §7-F1; see §6.1 for the
     corrected invariant and §8/P0 for the fix (make the clamp a non-optional
     floor derived from `[tui.permissions]`).
  3. re-mint from the per-user key when present
     (`newt_identity::enforced_caveats`, `lib.rs:1835`); degrade to the plain
     policy value when no key is available. **The mint re-roots from
     `session_root(&user) = AgentKey::issue(user, Caveats::top())`
     (`newt-identity:133–134`) and then `attenuate(root, policy)`
     (`lib.rs:1492–1493`), i.e. it proves `policy ⊑ ⊤` — always true. The mint
     supplies provenance and signature, not a ceiling (§7-F2).** The genuine
     `child ⊑ parent` enforcement (`EnvelopeError::Amplification`,
     `newt-identity:242–251`) bounds *downstream delegation* to plugins/mesh,
     not the interactive grant the facade is about.

### 2.3 Recoverable denials + `request_permissions` (#721)

A denial used to be a dead-end addressed to the *human* ("edit
`[tui.permissions]`"), which the *model* cannot do mid-turn — so the loop
stalled. #721 made denials *model-actionable*:

- **`DENIAL_RECOVERY_HINT`** (`tools.rs:1162`) is appended to every capability
  denial the model sees, telling it to call `request_permissions` *or* take a
  different approach. The gate is unchanged — the call is **still denied**;
  only the coaching is added.
- **`request_permissions`** (`execute_request_permissions`, `tools.rs:1327`) is
  the model-facing GRANT verb. It builds a `PermissionRequest` from
  `{capability, target, reason}` and consults the **same** gate a denial would
  (`tools.rs:1370`). `Allow` ⇒ "granted, retry now"; `Deny` ⇒ "declined, take a
  different approach"; **no gate** (headless/eval/ACP) ⇒ "no operator available"
  — a recoverable signal, never a hang (`tools.rs:1383`).
- **`request_user_input`** (#728, `tools.rs:1413`) is the *distinct* free-text
  path via `ask_question`. Kept separate because it cannot mint caveats.

### 2.4 The exec re-exec path

For `run_command`, an out-of-scope command is denied *inside* the confined
shell and surfaced as a structured envelope `{denied: true, denials:[{kind,
target, reason}]}`. The loop does **one consult + one re-execution**
(`tools.rs:1982`): `exec_denial_requests` (`tools.rs:1250`) lifts a pure-exec
envelope into `PermissionRequest`s; on `Allow(widened)` it re-dispatches under
the widened caveats; a *second* denial (a different target reached on the
re-run) falls back to the standard envelope (`tools.rs:1996`), so the model can
retry and prompt afresh.

### 2.5 The garbled denial bug (root cause + fix)

**Symptom (field report):**

```
capability denied: exec does not permit
  'denied: exec of "export" is not within the granted authority
   - add it via [tui.permissions] extra_exec=...'
```

The denial reason is wrapped **twice**: a fully-formatted reason appears *inside
the `'…'` slot* of a `{kind} does not permit '{target}'` formatter, where that
slot is meant to hold a bare target (a command name).

**Verified structural root cause — there is no canonical denial type.** Denial
text is built by *composing string formatters*, and the same logical message is
produced by **multiple duplicated builders that consume each other's output as
opaque text**:

1. **Reason joiners (duplicated).** `envelope_denial_reason` (`tools.rs:1098`)
   joins `denials[].reason`; **`denial_reason` in
   `newt-mcp-server/src/handlers.rs:512` is a byte-for-byte duplicate** of it.
2. **Guidance appender.** `envelope_denial_reason_with_guidance`
   (`tools.rs:1145`) does `format!("{reason} - {hint}")`, where `hint` =
   `extra_exec_hint` (`tools.rs:1127`) → `add it via [tui.permissions]
   extra_exec = […]`.
3. **`{kind} does not permit '{target}'` prefixers (≥3, duplicated).**
   - `denied_fs_result` (`tools.rs:1195`): `capability denied: {kind} does not
     permit '{path}'. {hint}`.
   - `CoderError::CapabilityDenied` (`newt-coder/src/error.rs:50`): `capability
     denied: {kind} does not permit '{target}'`.
   - `display.rs:467`/`:473`: `capability denied: {axis} does not permit
     '{target}'`.

The bridle already emits `denials[].reason` as a *full sentence* (`exec of
"export" is not within the granted authority`; see the in-tree fixtures
`tools.rs:3098`, `tools.rs:4817`). When that sentence — optionally after the
guidance appender adds ` - add it via …` — is handed to *any* `{kind} does not
permit '{target}'` formatter as its `target`, you get the doubled message
exactly. The formatters are individually correct; the defect is that **none is
idempotent and none distinguishes "a bare target" from "an already-formatted
reason," and a reason string crosses an engine/MCP boundary and re-enters as a
target.** That a reason can be a target is the whole bug.

**Honesty note.** Each formatter is correct in isolation, and I could not pin a
single in-tree line on HEAD that feeds a formatted reason back as a `target`
within one process — the doubling manifests across a layer boundary (the
coder/MCP engine's `CapabilityDenied` Display, or `denial_reason` in the MCP
handler, consuming a reason produced upstream). The *structural* cause is
verified regardless, and the fix does not depend on which boundary triggers it.

**Fix (structural, not a patch on the string):** introduce **one canonical
denial value**, formatted **exactly once at the UI edge**:

```rust
/// The single source of truth for a capability denial. Carries structured
/// fields only — NEVER a pre-formatted human string. Formatted exactly once,
/// at the model/operator edge, by `Display`. No other code path may prefix
/// "capability denied:" or "{kind} does not permit".
pub struct Denial {
    pub kind: DenialKind,   // the axis
    pub target: String,     // a BARE item: command / path / host
    pub note: Option<String>, // optional structured detail, never re-wrapped
}
```

Rules enforced by the fix:
- `envelope_denial_reason` and the MCP `denial_reason` collapse into **one**
  shared function (delete the duplicate at `handlers.rs:512`).
- the guidance hint (`extra_exec`) becomes a **field**, attached once at
  render, never concatenated into a `reason` that then flows onward.
- `CoderError::CapabilityDenied`, `denied_fs_result`, `denied_run_command_
  result`, and `display.rs` all render the *same* `Denial` via one `Display`;
  none takes a `reason: String` it might re-wrap.
- a regression test feeds a `Denial` whose `target` is *itself a formatted
  denial sentence* and asserts the rendered output contains `capability denied:`
  and `does not permit` **exactly once** (the bug reproduction).

This is an early PR in the build plan (§8, P1), because every other facade verb
emits denials and they must all speak through one un-doublable type.

---

## 3. The facade design — model-facing verbs and their OCAP mappings

The facade exposes **four verbs and one default**, and *nothing else*. The
model and operator never see `Caveats`, `meet`, `Scope`, or a generation
window. Each verb is defined by the algebra operation it compiles to.

### 3.1 Allow once → single-op caveat widening for the next op

- **Surface:** `[a]llow once` / `request_permissions` answered once.
- **Algebra:** `PromptChoice::AllowOnce` → `once_grants` → `mint(&once_grants)`
  → `widen_caveats(base, …)` then `.meet(preset_clamp)` (`lib.rs:1820–1828`).
  The widened `Caveats` is returned as `PermissionDecision::Allow(_)` and used
  for **exactly one** re-execution (`tools.rs:1990`); it does not outlive the
  call (`permissions.rs:76`) and the live session key is untouched.
- **Status:** shipped (§2). The facade work here is presentation parity — the
  exec path (re-exec) and the fs path (`fs_gate_allows`, `tools.rs:1278`) must
  emit the same *once* framing as the prompt.

### 3.2 Allow session → a session-scoped caveat (the generation window)

- **Surface:** `[s]ession allow`.
- **Today (two unbounded axes — §7-F3).** `(kind, target)` is remembered in
  `session_grants: BTreeSet` (`lib.rs:1872`) and folded into every subsequent
  `mint`; a matching pair auto-allows future calls with **no re-prompt**
  (`lib.rs:1860`). Two real over-grants ship today:
  - **Unbounded in time.** The scope lives **only in TUI process memory** — the
    minted `Caveats` carries no session bound (`valid_for_generation` is always
    `Scope::All` in newt-core), so a session grant projected anywhere outside the
    TUI (mesh, crew, a resumed log) would be *unbounded in time*. The
    generation-window fix below is **P2, unbuilt** (§8).
  - **Unbounded in breadth, with no risk-tiering.** The exec axis matches by
    **command-name set membership** (`Scope::permits → set.contains`,
    `newt-core/src/caveats.rs:41`). Nothing distinguishes `ls` from
    `bash`/`sh`/`python`/`node`/`perl`. A single `[s]ession allow` of an
    **interpreter name** is therefore a standing arbitrary-code grant: once
    `(Exec,"bash")` is in `session_grants`, future `run_command bash …`
    auto-allows, `mint` folds it, and the confined shell runs `bash` whose
    `exec`'d children fork *outside* the per-command interceptor — escaping the
    exec allowlist's per-spawn mediation, bounded only by an L3 boundary that
    (per #522 and the team's Landlock-exec findings) is not soundly applied to
    exec. The facade applies **no danger-tiering**: "allow `bash`" and "allow
    `ls`" are the same gesture. Likewise fs: `request_permissions` lets the
    **model choose the `target` string** (`tools.rs:1334,1357`) and fs
    enforcement is **prefix-based** (`tui_permits_path → starts_with`,
    `tools.rs:1018`), so a model-supplied `target:"/home/user"` or `"/"` that the
    human session-allows becomes a whole-subtree grant. §7-F3 specifies the
    danger-tiering fix (interpreter/shell exec targets and broad fs prefixes
    must force step-up or be un-settable from a plain session-allow).
- **The algebra already has the axis.** `Caveats.valid_for_generation:
  Scope<u64>` (`agent-mesh-protocol/src/caveats.rs:155`) is a causal-counter
  window, top = `Scope::top()` (`caveats.rs:169`), composed by `meet`
  (`caveats.rs:189`), proven non-amplifying by the same property tests as every
  other axis (`caveats.rs:401`). Mesh enforcement already verifies it:
  `caveats_for_peer_at(cert, current_generation)` (`newt-mesh/src/caveats.rs:117`)
  honors a cert only if every link is valid for the supplied generation, and
  `caveats_for_peer` **fail-closes** on any bounded generation it cannot check
  (`newt-mesh/src/caveats.rs:95`). **In newt-core today this axis is always
  `Scope::All`** — the session dimension exists in the algebra but is unused at
  the dispatch layer.
- **The mapping:** "allow for the session" → bind the grant to
  `valid_for_generation: Scope::Only([session_start_generation ..])`, i.e. valid
  from the turn the grant was made onward, for the life of this session's
  generation. Two changes, both small and attenuating:
  1. `mint` learns the current generation (the session start, or the live turn
     counter `SessionState::turn`, `newt-core/src/session.rs`) and, *when there
     are session grants*, narrows `valid_for_generation` from `All` to the
     session window. Because narrowing is a `meet`, this can only **reduce**
     authority — it is sound by construction (§6.1).
  2. dispatch gains a generation-aware check
     (`permits_exec_at(cmd, generation)` etc. on `CaveatsExt`,
     `newt-core/src/caveats.rs:78`) so a session grant stops applying once the
     session's generation window closes.
- **Why this is the right shape:** a session grant minted this way is already
  *wire-compatible* with mesh generation-scoped certs — no adapter is needed to
  project a session capability to a co-drive partner — and it enables
  **pull-based revocation**: bumping the generation at a chosen boundary (turn
  end, explicit revoke) auto-invalidates mid-session grants with no durable
  revoke list. See §5 for the lifecycle.

### 3.3 Deny once / deny always → the recoverable denial

- **Surface:** `[d]eny (default)` / `[D]eny always`.
- **Algebra:** both return `PermissionDecision::Deny` (`lib.rs:1880`,
  `lib.rs:1887`); `DenyAlways` additionally records the pair in
  `session_denials` so it short-circuits future prompts (`lib.rs:1849`). The
  *authority is unchanged* — the down-set is never enlarged. The facade
  contribution is the #721 recovery framing (§2.3): a denied call returns a
  model-actionable message (call `request_permissions`, or change approach),
  not a dead-end. **Deny remains bit-for-bit the structured denial**
  (`permissions.rs:78`) — the facade reframes the *coaching*, never the
  *authority*.

### 3.4 Allow with step-up (fingerprint / YubiKey) → a liveness-attested mint

- **Surface:** a fifth menu item — `[k]ey allow` / "confirm with your
  passkey" — for high-consequence grants.
- **Algebra:** this is the `attest` decision verb from the step-up MVP
  (`agent-bridle`, branch `feat/step-up-decision-mvp`, PR #24; design in
  newt-agent PR #472 `docs/design/human-presence-capabilities.md`). The key
  property, load-bearing for soundness: **`attest` is a *sharpening* of
  Writ-exercise (a liveness constraint), not new authority** —
  `effective = granted.meet(required)` is **unchanged**. A step-up cannot grant
  what a plain allow could not; it can only *demand a gesture* before the same
  grant fires.
- **Shape:** `Gate::evaluate(tool, granted, request, policy) -> Decision` where
  `Decision = Allow(ctx) | NeedsDischarge(AttestRequirement) | Deny(err)`.
  `NeedsDischarge` triggers the ceremony; `authorize_with_discharge` verifies
  the discharge against a recomputed `Challenge::bind(action_id, generation,
  nonce)` and mints the context *after* verification, charging budget only on
  success. The `Presence` lattice `None < Prompt < Passkey` composes by `meet`
  like every other axis — attenuation can only *raise* the required presence,
  never lower it.
- **The keystone (#766 + Charter):** *mutating policy is itself an OCAP
  capability.* Attenuating (deny/tighten) needs only ordinary authority;
  **amplifying (a standing allow) requires the human root via an `attest`** — an
  agent cannot loosen its own leash. This is what makes "session allow with a
  fingerprint captured at session start" safe to offer at all (§5).
- **Bounds (honest, §7-F6).** Step-up is **entirely unbuilt** today — P3,
  cross-repo on agent-bridle #24, marked high-risk (§8). §3.4 and §5 are
  *design*, not shipped behavior; the facade currently has no `[k]ey allow`. And
  even as designed, step-up **binds liveness to a generation window, not to each
  consequential act**: the §5 lifecycle captures the fingerprint *once* at
  session start and covers *all* session grants under it. If the generation
  never bumps (open question §10.2 is undecided), one gesture amortizes across
  unlimited later grants. So "step-up binds" is true only as strongly as the
  (undecided) bump policy — the design's strength is bounded by that knob, which
  must be resolved before §5 is advertised as a liveness guarantee.

### 3.5 The model never sees the algebra

The hard rule: the four verbs above are the *entire* model-facing authority
vocabulary. The model sees `request_permissions{capability, target, reason}`
and a denial coached toward it; it sees grant/deny/no-operator outcomes; it
sees, in the operator-mediated case, "allow X? [once / session / key / deny]".
It **never** sees `Caveats`, a `meet`, a `Scope`, or `valid_for_generation`.
The translation from these verbs to lattice elements happens entirely inside
`mint` and the gate. *This is the facade.*

---

## 4. Hidden tool-call routing (#773)

### 4.1 The problem

Capable models emit tool calls learned from *other* harnesses — `cat`, `find`,
`git`, `bash`, `str_replace_editor` — that newt does not expose by that name.
Today each lands as a wasted round, and worse, a model that reaches for `bash
cat …` can trip an *exec* denial for a read it could have done within authority.
The model should not have to learn OCAP tool syntax; the harness should route
its instinctive calls to the OCAP-safe built-ins.

### 4.2 What exists — the alias resolver (the #773 precursor)

`resolve_tool_alias` (`tools.rs:538`) already does a constrained version of
this, returning `AliasOutcome` (`tools.rs:527`):

- **`Rewrite(canonical)`** — a foreign name whose *arg shape matches* a real
  tool is dispatched transparently. E.g. `bash`/`sh`/`shell`/`execute` →
  `run_command` (`tools.rs:543`); `where_were_we` → `resume_context`
  (`tools.rs:623`).
- **`Correct(msg)`** — a foreign name whose args do *not* match is answered with
  the right tool's signature so the model retries. E.g. `cat`/`open_file`/`view`
  → "call `read_file` with `{path}` … `list_dir` with `{path}`"
  (`tools.rs:579`); `mkdir` → "newt has no mkdir tool — call `write_file`, it
  creates parent dirs" (`tools.rs:570`).
- **`is_hallucination`** (`tools.rs:508`) catches `run_command` *called with a
  tool name as the command* (e.g. `run_command cat …`) and unknown non-MCP
  names.

### 4.3 What #773 adds

The facade goal is to **silently route the common read/search/VCS reaches to
OCAP built-ins**, not merely coach them:

- **`cat <path>` → `read_file{path}`**, **`find … <path>` → `find{path}`**,
  **`ls <path>` → `list_dir{path}`** become `Rewrite` (silent) when the call's
  shape is unambiguous, instead of today's `Correct` (coach). The routed call
  goes through the *same* fs caveat check (`tui_permits_path`, `fs_gate_allows`,
  `tools.rs:1278`), so authority is identical — the model just doesn't pay a
  round to be corrected.
- **`git <subcommand>`** routes by sub-verb: read-only subcommands (`status`,
  `diff`, `log`, `show`) route to the built-in git read path; write subcommands
  (`commit`, `push`) stay gated (note `tools.rs:3671` already denies `git
  commit` without authority). This keeps the exec axis honest while removing the
  read-path friction.
- **`--yolo` — TWO different flags share one name; do not conflate them
  (§7-F5).** This is the sharpest documentation defect the red-team found.
  - **Shipped (#297) `--yolo` = `--disable-ocap` = L3-OFF.** `ocap_disabled()`
    (`tools.rs:888`, true on `NEWT_DISABLE_OCAP=1`) routes `run_command` to
    `host_shell_dispatch` — *"the PLAIN host shell — no leash, no interceptor, no
    sandbox"* (`tools.rs:933`) — gated only by `exec_floor_permits`, which
    **returns `true` unconditionally when no `/mode` floor is active**
    (`tools.rs:916`). So **default session + `--yolo` = arbitrary, fully
    unconfined exec on the host.** The "fenced fs" framing is illusory on this
    path: the fs fence governs only newt's `read_file`/`write_file` tools, so an
    unconfined `cat /etc/shadow` or `echo x > /etc/passwd` via the host shell
    bypasses the fs axis entirely. Even *with* a `/mode` floor, `exec_floor_permits`
    is leading-token + shell-metachar matching only (`tools.rs:914–931`): an
    allowed interpreter (`bash script.sh`, `python prog.py`, no metachars) runs
    unconfined, defeating the floor.
  - **Proposed (#773) routing-`--yolo` = L2-OFF/L3-ON.** What §4.3 *wants* is a
    flag that turns off the *convenience routing* (run the raw command instead of
    a built-in) while the command still goes through the confined shell under the
    session caveats — the L2-convenience/L3-boundary split (ADR 0005): turning
    off the convenience engine never turns off the boundary.
  - **The defect, and the rule:** the original §4.3 described the *proposed*
    semantics under the *shipped* name. As shipped, "turning off the convenience
    engine never turns off the boundary" is **false** — `--yolo` is exactly the
    L3-off unconfine. **P4 (§8) MUST NOT reuse the `--yolo` / `--disable-ocap`
    name for the routing flag.** The unconfine escape and the routing escape are
    different operations with different blast radii; aliasing them would make a
    routing convenience silently grant unconfined host exec. Either rename the
    routing flag (e.g. `--raw-shell`, still L3-on) or make `--disable-ocap`
    require an explicit, separately-named opt-in that the routing flag can never
    imply.

### 4.4 Debug surface + audit (invariant)

Routing must be **transparent and auditable** (#773): every silent `Rewrite` is
logged (the alias resolved, the canonical tool, the resolved target), and a
debug/verbose mode surfaces the rewrite to the operator. The `--yolo` escape is
logged as an explicit, traceable event — *never implicit*. Routing changes
*which built-in serves the call*; it must never change *whether the call is
within authority*. A routed call that is out of scope is denied exactly as the
canonical call would be — and now emits the **single canonical `Denial`** of
§2.5.

---

## 5. Session-allow with step-up captured at session start (#766.4)

### 5.1 The lifecycle

The richest facade verb is "allow for the whole session, captured once with a
fingerprint at session start." Its lifecycle, all in the algebra:

1. **Session start.** If the operator opts into session-scoped grants, the
   facade runs **one** step-up ceremony (`Presence::Passkey`,
   `record = true`). This produces an `Attestation` — a non-repudiable
   provenance record that becomes a **scar** in the causal log — bound to the
   session's start generation via `Challenge::bind(action_id, generation,
   nonce)`.
2. **Grant.** Each `[s]ession allow` thereafter mints with
   `valid_for_generation: Scope::Only([session_start ..])` (§3.2) and is
   *covered by* the captured attestation — no per-grant gesture, because the
   liveness was established for the session window.
3. **Enforcement.** Dispatch checks both the axis (`permits_exec` etc.) **and**
   the generation window (`permits_*_at(…, current_generation)`), and — for
   grants requiring it — the attestation's freshness
   (`freshness_generations`). All three are `meet`-composed; any one failing
   denies.
4. **Close.** The session ends, or the generation is bumped at a chosen
   boundary; the window closes and every session grant minted under it stops
   applying — **pull-based revocation with no durable revoke list** (§3.2).
   Process exit already evaporates the in-memory `session_grants`; the
   generation window makes that revocation *expressible in the minted caveat
   itself*, so it holds even if the grant was projected outside the TUI.

### 5.2 Why this is sound, not a blanket permit

The invariant from §3.4: "session allow" is **amplification**, and amplification
requires the human root via an `attest`. The fingerprint at session start *is*
that root gesture. It is captured **once**, but it is captured — the operator is
not bypassed, they are consulted at the strongest point (session start, full
liveness) and the result is *scoped* (the generation window) and *recorded* (the
attestation scar). The result is strictly narrower than "allow always": it is
"allow, within these axes, for this generation window, having proven liveness
once." A blanket `[tui.permissions]` config edit, by contrast, is durable and
unscoped — which is exactly why it stays a deliberate human action (#181), not a
facade verb.

---

## 6. Invariants (must not break)

The facade is **sugar over the algebra, never a bypass**. Each invariant below
is stated, then tied to the kernel mechanism that enforces it. If a facade verb
ever violated one, it would be a bug in the facade, not a property of the system.

### 6.1 Meet-only — and the precise sense in which "never widens" is true

This invariant has a strong part that holds unconditionally and a part that is
**conditional on an active `/mode` preset**. The red-team showed the original
phrasing overstated the conditional part; here is the corrected statement.

**Holds always (formal soundness).** The facade exposes **no join**. Every
composition in the facade is a `meet`: `widen_caveats` only populates `Only`
sets and is then folded; the generation-window narrowing (§3.2) and the step-up
presence narrowing (§3.4) are `meet`s. The algebra's `meet_never_amplifies`
property (`agent-mesh-protocol/src/caveats.rs:401`) therefore extends to the
facade: **no facade verb, and no sequence of facade verbs, can produce more
authority than the principal it was minted under, and the model cannot
self-amplify** (a gate-less `request_permissions` returns "no operator
available," never a self-grant, `tools.rs:1383`). This is the OCAP soundness
floor and it is real on every path.

**Conditional (the configured ceiling).** The stronger claim — *"a grant can
never exceed the operator's **configured** ceiling"* — holds **only when a
`/mode` preset is active.** `mint` re-clamps with `policy.meet(clamp)` *only*
`if let Some(clamp) = &self.preset_clamp` (`lib.rs:1827`), and `preset_clamp` is
`None` until the operator runs `/mode` (§2.2). **Absent `/mode`, there is no
configured ceiling on grants**: `[tui.permissions]` is the *widen baseline*, not
a cap, and a session-allow adds targets on top of it. Worse, the mint that §6.4
credits re-roots at `Caveats::top()`, so it is not a ceiling either (§7-F2). By
default, then, the real boundary on a grant is **the human reading a
(model-authored) prompt** (§7-F4). The honest invariant:

> **Sound *given an active `/mode` preset*: no grant exceeds the preset meet.
> Otherwise bounded only by the operator at the prompt, with no system-computed
> ceiling and no risk-tiering.** The §8/P0 hardening (derive a non-optional
> clamp from `[tui.permissions]` so the meet runs even without `/mode`) is what
> would make the configured ceiling unconditional.

### 6.2 Complete mediation — every effect is still checked (with two residuals)

The facade adds *prompts*, never *fast paths*. The re-executed call after an
allow is re-dispatched through the confined shell (`tools.rs:1990`) and
re-checked; `fs_gate_allows` re-checks the widened caveats against the path
rather than assuming the grant (`tools.rs:1292`); the enforcement helpers go
through `Caveats::permits_*` even when the caveat is `top` — *no fast-path
bypass, by design* (`newt-coder/src/coder.rs:442`). For the in-process tool path
this invariant holds. This is the enforcement-floor obligation of the
caveat-lattice paper §5 (and the §3.0-first sequencing of the authority-model
design review, #749): **the facade is sound only on top of a total enforcement
floor, and it must not perforate it.**

**Two residuals the facade inherits and does not yet close (§7-F7):**

1. **The fs floor is not canonically containing.** `tui_permits_path` is
   string-lexical and does **not** resolve symlinks; the in-tree test
   `tui_permits_path_symlink_escape_is_the_known_residual` (`tools.rs:3227`)
   pins the **OPEN** `#522` deviation — a symlink inside the workspace pointing
   out is permitted. Complete mediation is "every effect is checked," but the
   check it runs is not canonical containment. This is a floor deviation the
   facade sits on, not one the facade introduces; it must be tracked, not
   advertised away.
2. **`--yolo` (shipped #297) *is* a mediation fast-path.** On the
   `host_shell_dispatch` branch the #263 gate is never consulted (`tools.rs:1946`)
   and nothing is checked (§7-F5). This is the one path where "the facade adds
   prompts, never fast paths" is currently false — because the shipped
   `--disable-ocap` flag predates and sits beside the facade. §8/P4 must keep the
   routing escape strictly L3-on so it never becomes a second such hole.

### 6.3 Fail-closed

Default is deny: `parse_permission_choice` maps every unrecognized input
(including empty) to `Deny` (`lib.rs:1665`); a stdin read error is `Deny`
(`lib.rs:1704`); a bounded generation that cannot be checked is refused
(`newt-mesh/src/caveats.rs:95`); a headless `request_permissions` returns "no
operator available" rather than self-granting (`tools.rs:1383`). The facade
never converts absence-of-a-human into permission.

### 6.4 Mint-token gate — provenance and signature, not a ceiling

A grant becomes authority only by being **minted**. `mint` re-roots from the
per-user key (`newt_identity::enforced_caveats`, `lib.rs:1835`); the gate doc
requires that even auto-allowed session grants return *freshly minted* caveats
(`permissions.rs:96`); the widened value does not outlive the call
(`permissions.rs:76`); the live session key is never widened in place
(`lib.rs:1812`). What minting buys is **provenance and a signature** over the
granted caveats, and a freshly-issued (rather than mutated-in-place) value.

**Correction (§7-F2): minting is *not* the ceiling.** The original §6.4 said
"minting is where `base ⊑ root` is enforced." Traced to source, the mint
re-roots from `session_root(&user) = AgentKey::issue(user, Caveats::top())`
(`newt-identity:133–134`) and then `attenuate(root, policy)`
(`lib.rs:1492–1493`) — i.e. it enforces `policy ⊑ ⊤`, which is **always true**.
The interactive grant mint therefore *cannot fail* its attenuation check; it
provides no ceiling on the grant. The real `child ⊑ parent` enforcement
(`EnvelopeError::Amplification`, `newt-identity:242–251`) is genuine and
load-bearing for **downstream delegation** to plugins/mesh, but it does **not**
bound the interactive grant the facade is about. The ceiling on a facade grant
is the §6.1 preset meet (when a `/mode` is active) plus the operator at the
prompt — not the mint.

### 6.5 Denial completeness — the facade reframes UX, not authority

A request for a denied action stays denied. #721 added *coaching* to denials;
it did not change what is granted (`tools.rs:1158`). `Deny` is returned bit-for-
bit (`permissions.rs:78`). Hidden routing (#773) changes *which built-in serves
a call*, never *whether it is in scope* (§4.4). MCP write tools stay blocked
until explicitly allow-listed (#762) even when the harness enumerates them. The
facade is a lens; the sky is unchanged. **Exception (§7-F5):** the shipped
`--yolo`/`--disable-ocap` path is *not* a lens — it removes the boundary
entirely. It is a pre-existing escape hatch, not a facade verb, and §8/P4 must
keep it from being aliased by the routing escape.

---

## 7. Soundness & threats (red-team, 2026-06-29)

An adversarial review traced every load-bearing claim to source on HEAD
`1a0fb88`. This section records its findings verbatim-in-substance, each with the
facade's **answer** or a **noted residual** (and where the fix lands in §8). The
discipline: a design doc that overstates its own soundness floor is worse than
one that names its residuals, because the overstatement is what pressures
operators toward `--yolo`.

### 7.0 What genuinely holds (the steelman)

- **The algebra is sound and mechanically corroborated.** `meet` never amplifies
  (`agent-mesh-protocol/src/caveats.rs:401`); no join is exposed anywhere.
- **The model cannot self-amplify.** A gate-less `request_permissions` returns
  "no operator available" (`tools.rs:1383`), never a self-grant.
- **Fail-closed defaults are real.** `parse_permission_choice` maps anything
  unrecognized — including EOF — to `Deny` (`lib.rs:1665`); a stdin error is
  `Deny` (`lib.rs:1704`); an uncheckable bounded generation is refused
  (`newt-mesh/src/caveats.rs:95`).
- **Baseline `[tui.permissions]` confinement is enforced at normal dispatch**
  even without `/mode` (`effective_caveats` = base, `lib.rs:5434`). It is
  specifically the *grant path* and `--yolo` that escape it.
- **The §2.5 garbled-denial root cause is accurate** (a UX/plumbing bug, not an
  authority-soundness issue; the doc honestly flags that it could not pin the
  exact in-process line that re-feeds a reason as a target).

So in the **narrow, formal OCAP sense — a compromised agent cannot exceed its
down-set — the facade holds.** What follows are the places where the document
claimed *more* than that, and where the default-path boundary is weaker than
"the operator's configured ceiling."

### 7-F1 The #307 preset meet is OFF BY DEFAULT

**Finding.** §6.1/§2.2 presented `policy.meet(preset_clamp)` as an always-on
ceiling. It is `Option`, default `None` (`lib.rs:2159`), sourced only from an
`active_mode` set only by `/mode` (`apply_mode`, `lib.rs:6554`); at dispatch
`preset_clamp = active_mode.as_ref().map(|m| m.clamp.clone())` (`lib.rs:5438`)
and the meet runs only `if let Some(clamp)` (`lib.rs:1827`). Absent `/mode`,
`[tui.permissions]` is the widen *baseline*, not a ceiling.

**Answer + residual.** §2.2 and §6.1 are corrected to state the meet is
conditional. **Residual until §8/P0:** with no `/mode`, grants have no
system-computed ceiling. **Fix (P0, new):** derive a **non-optional** clamp from
`[tui.permissions]` so the meet runs on every grant even without `/mode`; `/mode`
then only *tightens* it further. This makes "a grant can never exceed the
configured ceiling" unconditionally true.

### 7-F2 The mint-gate is vacuous as a ceiling (re-roots at ⊤)

**Finding.** §6.4 credited minting with enforcing `base ⊑ root`. The mint
re-roots from `session_root = AgentKey::issue(user, Caveats::top())`
(`newt-identity:133–134`) then `attenuate(root, policy)` (`lib.rs:1492–1493`) =
`policy ⊑ ⊤`, always true. The real `child ⊑ parent` check
(`EnvelopeError::Amplification`, `newt-identity:242–251`) bounds downstream
delegation, not the interactive grant.

**Answer.** §6.4 corrected: minting buys **provenance + signature + a freshly
issued (not mutated-in-place) value**, *not* a ceiling. The ceiling is F1's
preset meet plus the operator. No residual beyond F1 — this is a documentation
correction, the code is doing exactly what it should for delegation.

### 7-F3 "Session allow" is a real over-grant on time and breadth

**Finding (the user's suspicion, confirmed).**
- *Too long:* `valid_for_generation` is always `Scope::All` in newt-core; session
  grants live in process memory (`session_grants`, `lib.rs:1740`) with no time
  bound in the minted caveat. The fix is P2, **unbuilt**.
- *Too broad (the sharp one):* exec matches by command-name set membership
  (`Scope::permits → set.contains`, `newt-core/src/caveats.rs:41`). One
  `[s]ession allow` of an interpreter (`bash`/`python`/…) is a standing
  arbitrary-code grant that auto-allows with no re-prompt (`lib.rs:1860`); the
  interpreter's `exec`'d children fork outside the per-spawn interceptor. The
  facade applies **no risk-tiering** — "allow `bash`" == "allow `ls`".
- *fs breadth:* the model chooses the `target` string (`tools.rs:1334,1357`) and
  enforcement is prefix-based (`tui_permits_path → starts_with`, `tools.rs:1018`),
  so a session-allowed `target:"/"` is a whole-tree grant.

**Answer + residual.** §3.2 is rewritten to state both axes honestly.
**Residuals:** time bound is P2 (unbuilt); breadth/risk-tiering is unaddressed
today. **Fix:** (a) P2 lands the generation bound *before* "session allow" is
advertised as scoped; (b) **new danger-tiering** (P3-adjacent): interpreter/shell
exec targets and broad fs prefixes (e.g. `/`, `$HOME`, the workspace root) are
**not settable from a plain `[s]ession allow`** — they require `[k]ey allow`
(step-up) or are refused outright. "Session allow" stays for narrow,
single-command / single-path grants.

### 7-F4 The model CAN trick the facade — through the operator

**Finding.** Absent `/mode`, the only boundary is the human, and the model
controls the prompt text: `request_permissions{capability, target, reason}` has a
**model-authored `reason`** (`tools.rs:1354–1362`) rendered verbatim
(`permission_prompt_text`, `lib.rs:1675`). `request_permissions{capability:"exec",
target:"bash", reason:"list the files in this directory"}` shows a benign
justification for a catastrophic grant. The prompt shows the verb, the bare
target, and the model's framing — **no danger annotation**, no "this grants
arbitrary execution" warning. This is a confused-deputy-through-the-operator.

**Answer + residual.** **Residual today:** the prompt trusts model text.
**Fix (new, pairs with F3):** treat `reason` as **untrusted model text**
(label it as such in the UI) and render a **system-computed blast-radius line**
the model cannot author — e.g. "⚠ `bash` is an interpreter: this grants
arbitrary command execution" or "⚠ `/` is the filesystem root: this grants
read/write to everything." The operator's decision must be informed by a fact
the model cannot forge, because §1.1 itself argues lay operators cannot reason
about blast radius unaided.

### 7-F5 `--yolo` already blows a hole — §4.3's claim was false

**Finding.** §4.3 said a `--yolo` shell call is "still dispatched through the
confined shell." The shipped flag of that name (`#297`, `ocap_disabled()`,
`tools.rs:888`) does the opposite: it routes to `host_shell_dispatch` — "no
leash, no interceptor, no sandbox" (`tools.rs:933`) — gated by
`exec_floor_permits`, which **returns `true` unconditionally when no `/mode` is
active** (`tools.rs:916`). **Default session + `--yolo` = arbitrary unconfined
host exec**, also bypassing the fs axis (an unconfined `cat /etc/shadow` never
touches `tui_permits_path`). Even with a `/mode` floor, the check is leading-token
+ metachar only (`tools.rs:914–931`), so `bash script.sh` runs unconfined. §4.3
had conflated the **unbuilt #773 routing-`--yolo` (L2-off/L3-on)** with the
**shipped #297 unconfine-`--yolo` (L3-off)**.

**Answer + residual.** §4.3 is rewritten to separate the two flags explicitly.
**Hard rule for §8/P4:** the routing escape MUST NOT reuse the `--yolo` /
`--disable-ocap` name; aliasing them would let a routing convenience silently
grant unconfined host exec. **Residual:** the shipped `--disable-ocap` is a
genuine L3-off escape and stays one — but it must be a separately-named,
explicitly-opted-in flag that the routing flag can never imply, and it should
carry a danger banner of its own.

### 7-F6 Step-up is unbuilt and binds to a window, not to each act

**Finding.** §3.4/§5 are aspiration: step-up is P3, cross-repo on agent-bridle
#24. The design property (`attest` is a sharpening, `effective =
granted.meet(required)`, `Presence None<Prompt<Passkey` composes by meet) is
sound on paper. But the §5 lifecycle captures the fingerprint **once** at session
start to cover **all** session grants; if the generation never bumps (§10.2
undecided), one gesture amortizes across unlimited later grants.

**Answer + residual.** §3.4 now states the bounds explicitly. **Residuals:**
(a) entirely unbuilt; (b) liveness binds to a *window* whose size is an
undecided knob. **Fix:** resolve §10.2 (bump policy) before §5 is advertised as a
per-act liveness guarantee; for high-consequence grants (F3's interpreter/broad
targets) require a **fresh** step-up per act rather than the session-amortized
one, so the window's looseness cannot cover the catastrophic cases.

### 7-F7 Complete mediation inherits an fs hole, and `--yolo` is a fast-path

**Finding.** `tui_permits_path` is string-lexical and does not resolve symlinks;
`tui_permits_path_symlink_escape_is_the_known_residual` (`tools.rs:3227`) pins
the OPEN `#522` deviation. And `--yolo`'s `host_shell_dispatch` is a mediation
fast-path: the #263 gate "is never consulted" (`tools.rs:1946`).

**Answer + residual.** §6.2 now records both. **Residuals:** #522 (the fs floor
is not canonically containing — a floor deviation the facade sits on, tracked not
hidden); `--yolo` fast-path (F5). The facade does not *introduce* either; it must
not advertise them away, and P0/P4 must not widen them.

### 7-F8 §2.1 line citations were stale despite the "verified" claim

**Finding.** The `lib.rs`, `tools.rs`, and `agent-mesh-protocol` citations were
exact, but every `permissions.rs` citation in §2.1 was ~20 lines stale (the
24-line module header was unaccounted for): `DenialKind` `:13`→`:33`,
`PermissionRequest` `:38`→`:58`, `PermissionDecision` `:52`→`:72`,
`PermissionGate` `:78`→`:97`, `ask` `:82`→`:101`, `widen_caveats` `:101`→`:120`,
`PermissionRecord` `:125`→`:145`, scope `:140`→`:160`. Substance was correct; the
"verified" claim was not, for that file.

**Answer.** Fixed in this revision — every §2.1 (and §3/§6) `permissions.rs`
citation re-verified against HEAD `1a0fb88` and corrected. No residual.

### 7.9 Bottom line

The facade **preserves OCAP soundness in the narrow formal sense** (meet-only
algebra, no model self-amplification, fail-closed) on every path. It does **not**
preserve the stronger property the original document implied — *"a grant can
never exceed the operator's configured ceiling"* — because that ceiling (the
#307 preset meet) is opt-in via `/mode` (F1), the mint-gate that §6.4 credited is
vacuous (F2), and the real default boundary is a human reading a model-authored
prompt with no risk-tiering (F3, F4). Concretely, the user's three suspicions are
all confirmed: (a) a session-allow of an interpreter or a broad path is a
standing over-grant with no time bound today; (b) the model can craft
`request_permissions` to present a dangerous grant under a benign reason; (c) the
shipped `--yolo` already runs fully unconfined exec by default. The §8 hardening
(P0 non-optional clamp; danger-tiering; `--yolo` rename; untrusted-`reason` +
blast-radius line; land P2's generation bound first) is what closes the gap
between the narrow and the strong claim. **Until P0 + danger-tiering land,
§6.1/§6.4 read as "sound *given an active `/mode` preset*; otherwise bounded by
the operator at the prompt."**

---

## 8. Build plan (phased PRs)

Layered on the enforcement-floor stack (#749 / authority-model design review
§3.0 — *the floor lands first*). Each PR is one issue, merged on green, cleanup
between (the ratchet discipline). Risk noted per the CLAUDE.md autonomy table.

| PR | Scope | Depends on | Risk |
|----|-------|-----------|------|
| **P0 — non-optional clamp floor (§7-F1)** | Make `preset_clamp` non-`Option`: derive a baseline clamp from `[tui.permissions]` so `mint`'s `policy.meet(clamp)` runs on **every** grant even without `/mode`; `/mode` then only tightens it. Property test: with no `/mode`, a session-allow cannot exceed the configured `[tui.permissions]` ceiling. Makes §6.1's strong claim unconditional. | enforcement floor (#749) | **high** (changes the default grant ceiling; needs the meet-never-amplifies property over the no-`/mode` path) |
| **P1 — canonical `Denial`** | One `Denial{kind,target,note}` value, formatted once at the edge; collapse the duplicate `denial_reason` (`handlers.rs:512`) into the shared joiner; make `CoderError::CapabilityDenied`, `denied_fs_result`, `denied_run_command_result`, `display.rs` all render it; regression test: a `Denial` whose `target` is a formatted sentence renders `capability denied:` / `does not permit` exactly once (§2.5). | enforcement floor (#749) | **low** (bug fix + regression test) |
| **P1b — danger-tiering + untrusted `reason` (§7-F3/F4)** | Classify grant targets: interpreter/shell exec names (`bash`/`sh`/`python`/`node`/`perl`/…) and broad fs prefixes (`/`, `$HOME`, workspace root) are **not settable from a plain `[s]ession allow`** — they require `[k]ey allow` (P3) or are refused. Mark prompt `reason` as **untrusted model text** and render a **system-computed blast-radius line** (e.g. "⚠ interpreter → arbitrary execution"). The tier table is *data*, not hardcoded `match` arms (three-Cs, §10.7). | P0, P1 | **high** (changes what a session-allow can grant; needs operator-facing UX + tier-table tests) |
| **P2 — session-allow caveat** | `mint` learns the session generation and narrows `valid_for_generation` to `Scope::Only([start..])` when session grants exist; add `permits_*_at` generation-aware checks to `CaveatsExt`; thread the current generation into dispatch (§3.2). **Land before "session allow" is advertised as time-scoped.** | P1 | **high** (touches the authority mint + dispatch; needs property tests that the narrowing is a `meet` and never amplifies) |
| **P3 — step-up verb** | wire the `attest` decision verb (`Gate::evaluate` / `authorize_with_discharge`) as the `[k]ey allow` menu item and the session-start ceremony; `Presence` lattice; `effective = granted.meet(required)` unchanged (§3.4, §5). For F3's high-consequence targets require a **fresh** step-up per act, not the session-amortized one. Depends on the agent-bridle step-up MVP (PR #24) being consumable. | P1b, P2, agent-bridle #24 | **high** (cross-repo; new authority gesture) |
| **P4 — hidden-tool routing** | promote `cat`/`find`/`ls` and read-only `git` from `Correct` to silent `Rewrite` in `resolve_tool_alias`; route through the same fs/exec checks; log every rewrite (§4). **The routing escape MUST be a separately-named flag (e.g. `--raw-shell`, L3-on) that NEVER aliases the shipped `--disable-ocap`/`--yolo` (L3-off) (§7-F5).** | P1 | **high** (changes default tool behavior; needs audit-log + escape-hatch tests + a test that the routing flag never disables L3) |
| **P5 — MCP enumeration + write allow-list (#762)** | enumerate all MCP tools up front; deny-by-default write tools behind an explicit operator allow-list; surfaced through the same facade verbs. | P1, P4 | **high** (write-path gating) |

**Sequencing rationale.** **P0 first** alongside P1: the non-optional clamp is
what makes the headline soundness claim (§6.1) true by default rather than only
under `/mode`, and every grant verb below relies on it as the ceiling. P1 because
every later verb emits denials and they must all speak one un-doublable type. P1b
(danger-tiering) gates the catastrophic session-allow cases and must precede any
broadening of routing or write-paths. P2 before P3/P5 because the session
generation window is the substrate both step-up-session and write-allow-lists
scope themselves to. P4 is independent of P2/P3 and can land in parallel after
P1, **provided it does not reuse the `--yolo` name.** None of P0–P5 may land
before the enforcement floor (#749) — they are sugar on a floor that must already
be total, and (per §7-F7) that floor still carries the OPEN `#522` symlink
deviation, which P0/P4 must not widen.

---

## 9. Paper framing

The enterprise-OCAP system is **three layers**, and the paper series should make
all three explicit:

1. **The algebra** (caveat-lattice paper, §3–§4): a bounded meet-semilattice
   over six axes, attenuation-only, mechanically corroborated. *What is true
   about authority.*
2. **The enforcement floor** (caveat-lattice paper, §5; authority-model design
   review, §3.0; #749): complete mediation — every effect checked before it
   happens. *What makes the truth bind to the running system.*
3. **The facade** (this document): the human/model-facing completion. *What
   makes the bound truth usable by a capable model and a lay operator without
   weakening it.*

The facade is not a softening of the thesis; it is its **operational
completion**. The caveat-lattice paper's sharpest lesson is that *a clean
lattice is a telescope, not the sky* — it secures the system only to the extent
the running system actually looks through it. The facade is the **eyepiece**:
the part a human or model actually puts their eye to. A telescope with a
beautiful mirror and no eyepiece images nothing for anyone. The contribution
this layer adds to the canon:

- **The model-expectation gap as a first-class security concern.** Prior ocap
  work assumes a principal who reasons in capabilities. An LLM principal
  reasons in *permissions* and *halts* when handed an algebra (#766, the
  HotSeat finding). A sound algebra that the principal cannot operate is, in
  practice, an availability failure that pressures operators toward `--yolo`.
  The facade closes the gap *without* the usual move of clawing authority back
  after the fact — it preserves attenuation-only and complete mediation while
  presenting permissions.
- **A constructive mapping, verb-by-verb, from a permissions UX to lattice
  operations** (§3): allow-once = single-op widen-then-meet; allow-session =
  generation-window narrowing; deny = unchanged down-set + recovery coaching;
  step-up = liveness-attested mint with `effective = granted.meet(required)`
  unchanged. Each verb is proven to be a `meet` (or a no-op on authority), so
  the facade inherits `meet_never_amplifies` directly (§6.1).
- **Amplification requires the human root.** The keystone — *mutating policy is
  itself a capability; attenuation needs ordinary authority, amplification needs
  an `attest`* — is what lets the facade offer a standing "session allow" that
  is sound rather than a blanket permit (§5). This connects the facade to the
  "Age of the Confused Deputy" position paper's step-up argument (§7.5).
- **The garbled-denial bug as a methodological miniature.** §2.5 is the
  enforcement-floor lesson in small: the *algebra* of denial was right, but the
  *plumbing* (multiple duplicated formatters, no canonical type, reasons used as
  targets) produced a user-facing failure. "Audit the effect surface, not just
  the call-sites" applies to the *message* surface too.

This document should be cited by the paper series as the facade layer, and its
§3 mapping table reused as the paper's "permissions-to-algebra" figure.

---

## 10. Open questions for the owner

1. **Generation source for session scope (§3.2).** Should "session start" bind
   to the `SessionState::turn` counter, a dedicated session-generation, or the
   mesh generation when meshed? The turn counter is cheapest but conflates
   "session" with "turn"; a dedicated counter is cleaner but is a new piece of
   session state.

2. **Generation bump policy (§5.1).** Do we bump the generation at every turn
   boundary (aggressive auto-revocation — session grants last one turn unless
   re-confirmed), only at explicit revoke, or only at session end? This is the
   knob between "session allow = sticky" and "session allow = per-turn liveness."

3. **Step-up availability degradation (§3.4).** When no authenticator is
   present (no YubiKey, no platform passkey), does `[k]ey allow` fall back to
   `Presence::Prompt` (advisory affirmation), or is it hidden entirely? Falling
   back risks training operators that "key allow" sometimes means "just press
   enter."

4. **Hidden-routing default for write-ish verbs (§4.3).** `git status` is a
   clear silent `Rewrite`; `git commit` is a clear gate. Where do `git add`,
   `git stash`, `git checkout` land — silent route, or gated as exec? This is a
   per-subcommand policy table that wants an owner decision (and is itself a
   three-Cs "knowledge as data" candidate, not a hardcoded match arm).

5. **The two `--yolo`s — naming and granularity (§4.3, §7-F5).** The shipped
   `--disable-ocap`/`--yolo` is L3-off (unconfined host shell); the proposed
   routing escape is L2-off/L3-on. **They must not share a name** (settled
   direction in P4); the open part is *what* to call the routing flag
   (`--raw-shell`?) and whether the shipped unconfine flag should additionally
   be per-axis (`--disable-ocap=exec` keeps fs/net governed) or carry a
   mandatory danger banner / require a step-up to arm. Per-axis is safer but more
   surface.

6. **MCP write allow-list shape (#762, §8 P5).** Per-server, per-tool, or
   per-(server,tool)? And does an allow-list entry mint a standing caveat
   (durable, like `[tui.permissions]`) or a session-scoped one (§3.2)?

7. **Should the facade verbs themselves be data (three-Cs)?** The menu, the
   verb→axis mapping, and the routing table are all candidates for the
   language-pack/lexicon pattern (composition/configuration/convention) rather
   than hardcoded `match` arms. Worth doing now, or after the verbs stabilize?
