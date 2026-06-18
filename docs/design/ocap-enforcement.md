# OCAP enforcement — the runtime side of the deviation ratchet

**Status:** Design + first scaffold (2026-06-18). The runtime checker
(`newt-core/src/ocap.rs`) and the architecture for closing the three critical
deviations. **No real isolation is built yet** — this lands the *interface* and
the *fail-closed gates*, so the dangerous paths are structurally unreachable and
`just ocap-check` gains real teeth.
**Builds on:** `docs/security/ocap-deviations.md` (the register + ratchet),
`docs/design/captured-shell-ocap.md` + `captured-shell-cross-platform.md` (the
threat model + per-OS sandbox), the threat-model verdict (`unsound-needs-rework`:
authority algebra sound, enforcement unbuilt; **redaction is NOT the boundary** —
egress-proxy + keep-the-token-out-of-the-box is).

## 1. The model

> effective authority = meet( the human's grant , what the currently-verified
> invariants can actually enforce ).

A dangerous capability is available **iff** all its required OCAP invariants
*verify*; else it is **fail-closed OFF** with honest evidence. Enforcement is
two-sided and both sides are now wired:

- **Runtime** (`newt-core/src/ocap.rs`): a checker returns
  `Verification::{Verified{evidence} | Absent{deviation, reason}}`; a dangerous
  capability calls `require(verify_X())?` before doing anything. Today every
  verifier returns `Absent`, so every dangerous path refuses.
- **CI** (`scripts/ocap_check.py`, `just ocap-check`): statically asserts every
  `OCAP-DANGER:<id>` site carries its `OCAP-GATE:<id>` within 20 lines while the
  deviation is open. The gates **cannot be removed** without turning the build red.

The scaffold ships **3 `OCAP-DANGER` sites** — `seed_live_credential` (gated on
`b1-os-isolation` *and* `disclosure-gate-live-path`) and `admit_untrusted_remote`
(gated on `b1-os-isolation`) — all fail-closed.

## 2. Closing `b1-os-isolation` (the critical one)

**Invariant:** uid-namespace + Landlock fs + seccomp + a **default-deny netns**
whose *only* egress is a broker proxy (DNS included). `verify_b1()` flips to
`Verified` only when the full stack is present and fail-closed **at session
seed** — re-checked per session (no COW-cloned-pod skip).

Per-OS floor (`captured-shell-cross-platform.md`):

| OS | Mechanism |
|---|---|
| Linux | Landlock (fs + net, kernel ≥ 6.7) + seccomp + user/net namespaces + egress proxy |
| macOS | Seatbelt (`sandbox-exec`) profile / App Sandbox + egress proxy |
| Windows | AppContainer + Job Object + WFP filters + egress proxy |

The **egress proxy is the boundary**, not redaction: the worker's netns has no
route to the internet except the proxy, which holds the credential and presents
it to *allowed* destinations. A monitor bypass inside the box still cannot reach
the network.

## 3. Closing `disclosure-gate-live-path`

**Invariant:** *every* tool result passes a **single disclosure filter** before
it is pushed into `messages` — one chokepoint, on the live path (not just the
next-turn observation, and not shape-only regex). `verify_disclosure_gate()`
flips to `Verified` when (a) all disclosure paths share one chokepoint and (b) a
**canary** value seeded at session start never appears in any `{"role":"tool"}`
message or summary. The robust filter is **by known value** (the broker knows the
exact secret), not by shape — re-encoding defeats shape-matching.

## 4. The credential broker (`seed_live_credential` / the `pa login` use case)

The operator runs `pa login`, which mints short-lived scoped tokens. Instead of
writing the token into the agent's environment (ambient authority the model can
read and exfiltrate), the broker design:

1. keeps the **token value out of the box** — the worker/model never sees it;
2. the broker (in the trusted plane, behind the egress proxy) **presents the
   token to outbound requests** to allowed hosts only;
3. `seed_live_credential` carries only a non-secret `label` for the ledger.

It refuses (`FailClosed`) until **both** `verify_b1` and `verify_disclosure_gate`
pass — so a live credential is never seeded into an unsandboxed, un-gated box.
This is the *practical caveat* in the register made enforceable: function is kept
by keeping the dangerous part disabled, bounded by construction.

## 5. Separation of duties (`sod-proposer-not-worker`)

A future verifier `verify_sod()` gates **auto-apply of any proposed policy**: the
proposer must be a cryptographically distinct, more-trusted identity than the
confined worker (`proposer_fp != worker_fp`), and observe-then-propose must be
taint-aware (down-weight worker-controlled transcript). Until then, every policy
promotion needs a human approval bound to the lowered-`Caveats` hash.

## 6. The ratchet: how a deviation closes

1. Build the invariant's enforcement (e.g. the Linux Landlock+netns+proxy stack).
2. Flip its `verify_*()` from `Absent` to `Verified{evidence}` (evidence = what
   was confirmed at session seed).
3. The fail-closed gate now passes → the capability **unlocks** — the ceiling
   rises *toward, never above* the human's grant.
4. Update the register entry's `Status: OPEN → CLOSED`; `ocap-check`'s code guard
   stops requiring the marker (closed deviations are exempt), and a regressed
   verifier is caught by the runtime refusal + the canary/ratchet tests.

Zero open deviations = full OCAP = full function.

## 7. Scope

**In this scaffold:** the `Verification` interface, fail-closed `verify_b1` /
`verify_disclosure_gate`, the `seed_live_credential` / `admit_untrusted_remote`
gates with `OCAP-DANGER`/`OCAP-GATE` markers, unit tests, and the `ocap-check`
teeth.

**Out of scope (follow-ups):** the real per-OS sandbox + egress proxy (B1); the
single disclosure chokepoint + canary harness; the broker transport; `verify_sod`
+ the proposer key; wiring `just ocap-check` into the pre-push hook / CI as a
blocking gate (deferred to avoid colliding with concurrent CI changes — do it
once the credential path begins to land); the policy-authoring assistant (the
multi-path caveat helper).
