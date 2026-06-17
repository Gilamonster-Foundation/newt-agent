# Captured-shell OCAP + policy-authoring assistant

**Status:** Design + adversarial threat-model (2026-06-16). **Verdict:
`unsound-needs-rework`** — the authority *algebra* is sound; the *enforcement* is unbuilt.
Read §4 before building anything that holds a live credential.

> **Amended by [`captured-shell-cross-platform.md`](./captured-shell-cross-platform.md).**
> This doc left two things implicit that that companion makes binding: (1) it
> treats `brush` as the interception layer, but brush is **not wired** yet
> (agent-bridle `stub-shell`); (2) its B1 ("Landlock + seccomp + netns + egress
> proxy") is **Linux-only**, and newt is tri-platform. The companion grounds the
> long-term **brush-as-portable-interpreter-gate** vision, specifies B1 **per OS**
> (Linux / macOS Seatbelt / Windows AppContainer), and adds a deliberate native
> `bash`/`zsh`/`powershell` carve-out for compatibility. Where this doc says
> "B1," read "the **host platform's** B1" per that matrix.

## 1. The problem

Real workflows need real short-lived scoped credentials. The motivating case: a corporate
`pa login` drops **short-lived scoped token files** at fixed filesystem paths, and the
agent's subsequent commands need them — *while* running under object-capability (OCAP)
confinement.

Two facts shape the answer:
- **Ambient env can't work, by design.** newt's confined shell already runs
  `do_not_inherit_env(true)` (`newt-core/src/agentic/tools.rs:207`) — it does **not**
  inherit the host's ambient environment, which is why `venv_cmd_prefix` *injects*
  `export VIRTUAL_ENV=…` explicitly. So `pa login` exporting a token into a parent shell
  is invisible to confined commands. That block is **OCAP working**, not a bug: an env-var
  token is ambient authority readable by every command — the Confused Deputy you're
  defending against.
- **The tokens are already files** → already capability-by-reference → the OCAP-clean
  primitive. The credential should be a *named reference*, presented to authorized
  commands, never ambient.

## 2. The captured/interrogated shell

A shift in confinement *model*: from per-command, env-clean, stateless confinement to a
**persistent, OS-isolated, interpreter-mediated shell session** that *retains* state (env,
cwd, sourced rc, the token files) — a **reference-monitor sandbox**. The persistence is the
feature: it solves the `pa login` workflow without ambient *host* authority, because the
box has its own deliberately-seeded env.

newt's confined shell is **`brush`** — a Rust shell newt controls — so "interrogation" can
be **interpreter-level** (each `exec`/file-`open`/network-`connect` is a *structured event*
checked **before** it commits), not fragile byte-stream parsing. That is the difference
between **prevention** and **detection**. Two gates:
- **Admission gate** — may this effect happen, given what's seeded?
- **Disclosure gate** — what of the output reaches the model, redacted?

### Three isolation invariants (REQUIRED for soundness)
1. **OS-level isolation** — namespaces / seccomp / landlock + a network egress proxy, so
   the monitor is *not the only barrier* between a token and the internet.
2. **The monitor can BLOCK inline** (admission *and* egress), not merely log.
3. **Least-privilege seeding** — the box holds *only* this task's scoped short-lived
   tokens, nothing else.

## 3. The policy-authoring assistant

Hand-authoring multi-path OCAP carve-outs is beyond a normal human (the SELinux
`audit2allow` / AppArmor `aa-logprof` problem). The assistance is the **loadout pattern
applied to the authority axis** — three authoring modes + an audit surface:
1. **Observe-then-propose** — read the interrogation transcript from a watched run, propose
   the *minimal* carve-out that would have allowed exactly those effects. The wiretap *is*
   the training data.
2. **Deny → ask → promote** — extend `--prompt-for-permissions` (#263) so each decision can
   be *promoted* to a durable carve-out. The human authors policy by answering questions.
3. **Compose-from-named-packs** — `NamedPermissionPreset` made composable + credential-aware
   (`pa-credentialed`, `python-dev`, `read-only-audit`), selected like a loadout kit.
4. **Explain/verify surface** — plain-language grant summary, policy diff, over-broad-grant
   flag, "would this allow Z?" simulation. The audit companion to `/config` / `/loadout`.

### The propose-not-grant invariant (the spine)
The assistant **proposes; it never self-grants.** A human — or a separate trusted authority
key — approves. Every proposal `meet`s to ≤ the human-authorized ceiling; the default-deny
base is harness-minted ("the harness stamps, the model never asserts"). Explain + diff +
adversarial-review of the proposal are **security controls**. **Separation of duties:** the
proposing surface must be a *different, more-trusted* model/role than the confined worker.

This is the Centaur model applied to security: the human moves from *author* to
**approver/judge** of machine-proposed, machine-explained, least-privilege policy.

## 4. Adversarial threat-model verdict — `unsound-needs-rework`

A 6-lens adversarial panel (75 agents, 68 findings, 67 survived independent triage,
`wf_ebb2efe5-b41`) went **into the newt code** and verified its claims.

**The one genuinely sound built piece:** the authority **algebra** — meet-only signed
unforgeable `AgentKey` delegation (`agent-mesh/…/caveats.rs`, `agent_key.rs`) + `plan.rs`
deny-default + "harness stamps, model never asserts." **Everything that turns it into
enforcement is missing or wrong.**

### Verified holes
| # | Finding | Sev |
|---|---|---|
| 1 | **B1 (OS isolation + egress proxy) is entirely UNBUILT** — zero landlock/seccomp/namespace/egress code; the only working exec path is an unconfined host `bash -c` with `sandbox_kind none`. The in-process monitor is the *only* barrier. | critical |
| 2 | **The disclosure gate doesn't exist on the live tool-result path.** `redact_secrets` (`compress.rs:1069`, a 7-regex shape matcher) runs only on the *next-turn* observation/summary; `mod.rs:1312`/`2025` push the raw tool result to the model. Shape-matching can't catch an opaque corporate token, and `cat $TOK \| base64` defeats it anyway. | critical |
| 3 | **Separation of duties is nominal** — `role` is a string; observe-then-propose ingests the *worker's own transcript* as ground truth, so the worker authors its own ceiling. | high |
| 4 | fs fence is `starts_with` (cross-voice theft via `..`/symlink); MCP has no caveat leash; exec is name- not behavior-bound; the headless path skips the prompt gate. | high |

### The load-bearing reframe (binding)
**Redaction is NOT the boundary.** No redactor catches a secret obfuscated beyond
enumerated encodings inside an interpreter (md5sum oracle, custom cipher). The real
boundary is **(a) the egress proxy + (b) keeping the token OUT of the box entirely** — a
broker/credential-helper presents it to outbound requests so the value **never transits the
model context.** Don't scrub the token; don't let the model see it.

### The three MUSTs (path to sound)
1. **Gate credential-seeding on a fail-closed, runtime-verified B1** — uid-namespace +
   Landlock fs + seccomp + default-deny netns + an egress proxy that is the *only* egress
   (DNS included). **No real token without it.**
2. **Close the in-process monitor holes regardless of B1** — value-provenance taint
   redaction on the *live* tool-result path; canonicalize-then-contain fs fence via
   `openat2`; behavior-tiered exec bound to resolved paths; MCP under the Caveats leash;
   fail-closed config/ocap defaults.
3. **Make the human spine real** — cryptographic proposer≠worker separation of duties; a
   taint-aware observe-then-propose; an explainer derived *from the enforcer* with approval
   bound to the lowered-Caveats hash; a tamper-evident per-voice ledger + token burn seam.

### Residual risks to accept (irreducible)
- Disclosure-gate leakage under arbitrary lossy/keyed transforms — so the egress proxy +
  keeping the token out of the box is the boundary, not redaction.
- Application-layer exfil to the one host the credential legitimately needs (allowlist can't
  distinguish sanctioned from exfil traffic without payload inspection or a broker).
- Granting any interpreter is arbitrary local code execution in the box — accept it as an
  OS-sandbox-bounded decision, not pretend name-granularity bounds behavior.
- Approval fatigue (bounded by risk-weighted per-line approval + the meet-only ceiling).
- `brush` enters the TCB once integrated (parser-differential between interceptor view and
  executor effects) — pin an audited revision; B1 is the backstop.
- Short-lived-token TTL window — a token exfiltrated early is usable until expiry unless the
  issuer supports sender-binding (mTLS/DPoP/audience pinning); out of newt control.

## 5. Build sequencing
1. **Do not seed a live `pa` token** until B1 is a fail-closed precondition.
2. PTY-wiretap MVP (observe-only) → interpreter-mediated admission gate → egress-gated.
3. The disclosure gate becomes a single chokepoint on the live tool-result path, with the
   seeded token redacted by **known value** (B3 knows the exact path), not by shape.
4. The policy assistant ships advisory-only, with proposer≠worker enforced cryptographically
   before any observe-then-propose runs against a credentialed transcript.

The design is salvageable and the spine is correct — but the credential-seeding feature is
**not** safe to ship until the three MUSTs land, or the invariants are silently off in
exactly the headless foreign-model swarm this exists for.
