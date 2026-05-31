# Decision: Agentic Object-Capability Security

**Status:** Proposed
**Date:** 2026-05-30
**Tracking issue:** Gilamonster-Foundation/newt-agent (feature request — see issue link in PR)
**Supersedes nothing.** Extends `docs/decisions/mesh_integration.md` (the
`UserKey` → `AgentKey` trust root is the substrate this builds on).

---

## TL;DR

The agent harness is a **confused deputy**. It runs with the full identity and
full authority of the human user, then takes instructions from untrusted input
(model output, tool results, fetched web pages, repo contents). Any of those
inputs can steer the deputy into misusing authority the user never intended to
lend for this task.

Today we fight this with regex allow/deny lists bolted onto each tool call.
That approach does not compose, and it collapses entirely against external
systems (GitHub, etc.) that have no vocabulary for "a smaller version of me."

The fix is **object-capability security (ocap)**: the agent holds *attenuated
capabilities*, not the user's ambient authority. Authority forms a
**meet-semilattice** under intersection; delegation is **attenuation-only** —
you can hand a child a subset of what you hold, the child can subset further,
but **nothing in the chain can ever amplify**. Safety stops depending on the
model behaving and becomes *structural*: a fully compromised agent still cannot
exceed the down-set of capabilities it was minted with.

We already have the seed of this in `newt-mesh`: `UserKey` (ed25519 root)
issues `AgentKey`s. We promote `AgentKey` from a *discovery tag* into an
*attenuated capability* and enforce it with kernel primitives that are
themselves trees of uid/namespace mappings.

---

## 1. The problem, named precisely

### 1.1 Identity vs. authority

Unix conflates two things that must be separated:

- **Identity** — *who* a process claims to be (UID; an ed25519 key; a GitHub
  login).
- **Authority** — *what* it is permitted to do (its effective permission set).

The agent harness today runs with the user's identity **and** the user's full
authority, fused. The regex allow/deny lists are an attempt to claw authority
back *after* the fusion, at the boundary of each tool call, in each tool's own
vocabulary. That is why they are brittle: they do not compose, and they have
nothing to say to a system that cannot represent sub-identities.

### 1.2 The confused deputy

Hardy's confused deputy (1988): a privileged program is tricked by a less
privileged party into misusing its authority. The classic example is a
compiler with billing-file write permission that a caller tricks into
overwriting the billing file by naming it as the "output."

An LLM agent harness is the confused deputy in its purest, highest-leverage
form:

- It is **maximally privileged** — it runs as the user, with the user's
  tokens, keys, filesystem, and shell.
- It takes instruction from **untrusted channels** — the model's own
  continuation, tool outputs, fetched pages, file contents, issue text. Prompt
  injection is literally "confuse the deputy."
- It acts at **machine speed and scale** — one confused step can push to a
  forge, exfiltrate a secret, or `rm -rf` before a human notices.

Ambient authority is the root cause. Object-capability security is the
literature's answer, and it has *not* yet been applied to agent harnesses. That
gap is the opportunity.

---

## 2. The mathematics

The intuition "this is shaped like Unix root → groups → subusers" is right in
spirit, but the precise shapes matter because they tell you what is free and
what is hard.

- **Principals form a tree (or DAG).** You, issuing sub-principals, each issuing
  their own. This is the "Unix tree" the intuition reaches for. `newt-mesh`
  already has it: `AgentKey::issue(&user_key, AgentMetadata { .. })`.

- **Authority forms a bounded meet-semilattice** `(L, ⊑, ⊓, ⊤)`, *not* a tree.
  Capability sets are partially ordered by ⊆. The top element ⊤ is the user's
  full authority.

- **Delegation is a monotone-decreasing map (attenuation).** Minting a child =
  choosing any `A' ⊑ A(parent)`. There is no upward operation in the algebra.

- **Chains compose by meet.** A request flowing p₁ → p₂ → p₃ carries effective
  authority `A(p₁) ⊓ A(p₂) ⊓ A(p₃) ⊓ caveats`. This is associative, commutative,
  with identity ⊤ — i.e. a **commutative monoid under ⊓**.

The "usable by an LLM" property falls straight out of attenuation-only: because
the algebra has **no join/amplify operation reachable by a child**, a confused
or compromised agent *cannot* escalate. Correctness of the worker model stops
being a safety dependency.

This algebra is not novel to us — it is the formal core of:

- **Object-capability security** (unforgeable references that *are* the
  authority to use the referenced object).
- **SPKI/SDSI** — principals are *keys, not names*. This is exactly how we dodge
  "GitHub doesn't know my sub-users": authority is anchored in keys we control,
  not in identities a remote must recognize.
- **Macaroons** — bearer tokens with append-only *caveats* (attenuation by
  stapling restrictions you can add but never peel).
- **Biscuit** (`biscuit-auth`, Rust-native, Datalog caveats, offline
  attenuation + signature verification) — the most direct off-the-shelf fit for
  this workspace.

---

## 3. The external-systems crux — and why it dissolves

The hard part: external systems (GitHub) will not let one identity decompose
into many limited ones. The move is to **stop trying to make them.**

There are only two kinds of external system:

**(A) Systems that offer a native attenuation primitive.** Project the local
sub-principal onto whatever the system already speaks:
- GitHub: fine-grained PATs (repo + permission scoped), **GitHub App
  installation tokens** (short-lived, per-repo, per-permission), deploy keys.
- NATS: accounts → users → signed JWTs with subject-scoped permits — a
  delegation hierarchy *in the protocol itself*.
- Vault: policies are a lattice.

This is exactly the kyln "projection" move: a one-way transform from an
authority into a derived, scoped view that carries provenance.

**(B) Systems that offer nothing.** Never let the sub-principal touch the system.
The master credential stays put in a **broker** (Vault on the NUC; the existing
`drake-keysmith` pattern). The sub-principal holds only a *handle* — "I may
invoke operation X through the broker" — and the broker re-identifies as the
full user at the boundary. The remote sees one identity; **attribution is
reconstructed from the broker's content-addressed log, not from the remote's
identity field.**

This is already the house style:

- **Secrets never move** → the sub-principal never holds the GitHub token; the
  broker does. The child gets a capability, not a secret.
- **Swarm-internal storage = bare git repo, not a forge** → sub-principals push
  to a local bare repo (fully local ⇒ fully attenuable with OS primitives).
  **Exactly one** narrow, audited **RTB / promote-to-forge** gate runs as the
  full identity and projects approved work outward. GitHub never has to
  understand sub-users *because sub-users never reach GitHub* — only the bridge
  does.
- **NATS not SSH** → inter-machine, the lattice is *natively expressible* as
  NATS account/user JWTs. No interposition needed.
- **Generation counters, not wall-clock** → caveats key on commit SHA /
  generation counter / idempotency token ("valid for flight N, repo X, paths
  `src/**`, ≤ K calls"), never on time.

**Unifying principle:**

> Attenuate **once, locally**, in a single algebra. Enforce it with OS
> primitives that are already trees of uid/namespace mappings. Reach the outside
> world only by **(A) projecting onto a native scoped token**, or **(B)
> brokering through a bridge that holds the real secret and re-identifies as
> you**. The decomposition is yours and stays home; the world sees either a
> token you minted or the one bridge acting as you.

---

## 4. Primitives on hand, in two tiers

### Local enforcement tier (OS-enforced, strongest, free on Linux)

| Primitive | Gives you | Algebra it realizes |
|---|---|---|
| Real subusers (`useradd`, `sudo -u`) | fs/process separation, group lattice | the principal tree, literally |
| **User namespaces / rootless Podman** | subdivide uids *without root* — a kernel-native tree of uid mappings | the structure the intuition reaches for, already in the kernel |
| **Landlock LSM** | an unprivileged process *irreversibly drops* its own fs authority to a subset | attenuation-by-construction in one syscall |
| seccomp / capabilities(7) | attenuate one process's syscall/cap set | meet on the syscall axis |
| OverlayFS / ZFS clone | give the child a *view* (down-set) of the tree | ties to "lived workspace snapshot/fork" |

Headline: **Linux user namespaces + Landlock already *are* the "Unix tree
usable by a process."** We do not invent the enforcement; we bind the principal
model to it.

### Credential / external tier (brokered or projected)

| Primitive | Role |
|---|---|
| Vault policies / `drake-keysmith` | the broker; mints scoped, short-lived tokens per sub-principal |
| GitHub fine-grained PAT / App installation token | the (A)-projection target for GitHub; minted on demand, narrow, short-lived |
| NATS account/user JWTs | native lattice for inter-machine authority — no broker needed |
| bare-repo-on-NFS + single RTB gate | the (B) pattern: sub-principals act locally, one bridge exports |
| biscuit / macaroon token | portable attenuable capability where we control both ends |

---

## 5. Concrete shape for newt — one struct away

Newt today has **no** attenuation: `newt-tools` are bare functions run as the
full user; the only authority knob is `ProviderConfig::env_pass`. But
`newt-mesh` already ships the principal tree:

```rust
AgentKey::issue(&user_key, AgentMetadata {
    role: "newt-worker".into(),
    capabilities: vec!["newt-inference".into()],  // discovery only, today
    expires_at: None,                              // empty caveat slot
});
```

The design is: **promote `AgentKey` from a discovery tag into an attenuated
capability.** Three moves, each a roadmap-step-sized PR:

1. **Make `AgentMetadata` carry authority, not just advertise it.** Add a
   `Caveats` field — a meet-semilattice element, e.g.
   `{ fs_read: PathSet, fs_write: PathSet, exec: CmdSet, net: HostSet, max_calls,
   valid_for_generation }`. `issue()` enforces `child.caveats ⊑ parent.caveats`
   at signing time — the monoid, type-checked. `biscuit-auth` provides this off
   the shelf, signatures included.

2. **Enforce it twice — belt and suspenders, both derived from the same signed
   token.** (a) `newt-tools` consults the caveat before acting; (b) `newt worker`
   calls **Landlock + a uid-mapped namespace** on startup from its own
   `AgentKey`, so a tool that ignores the check still *cannot* escape.

3. **Keep the worker off all forges (already mandated).** The worker's
   `AgentKey` grants *bare-repo push only*. Promotion to a forge is a separate
   privileged principal holding the broker handle, running the RTB gate. Sortie
   → bare repo (local, attenuated) → arbiter grades the patch → RTB bridge
   projects to the forge as the full user. The mandatory `model_id` on the reply
   already supplies the attribution leg.

Payoff in flight-ops terms: **every sortie gets a freshly-minted sub-principal**
— a content-addressed scorecard row, a provenance anchor, an authority strictly
below the user's that *cannot* be amplified however the worker misbehaves. The
soul file authors *who* the worker is; the `AgentKey` caveat set authors *what
it may touch*. Same idea, two orthogonal axes.

---

## 6. Why this matters beyond newt

This is a general property of agent harnesses, not a newt detail: **ambient
authority + untrusted instruction = confused deputy.** The entire industry runs
agents this way today. An ocap substrate — keys not names, attenuation-only
delegation, local enforcement, projection/brokerage at the edge — is the
structural fix. Newt is the natural first proving ground because it already has
the `UserKey` → `AgentKey` root and a deliberately minimal tool set to wrap.

---

## 7. Open questions

- Caveat language: hand-rolled lattice types vs. adopting `biscuit-auth`'s
  Datalog wholesale. Bias toward biscuit for the signature + offline-attenuation
  machinery; revisit if the Datalog surface is heavier than the tool set needs.
- Landlock kernel-version floor (≥ 5.13 for v1; ≥ 6.7 for the network rules) vs.
  rootless-Podman fallback on older kernels.
- Where the broker lives for the (B) path: extend `drake-keysmith`, or a
  newt-local minimal broker for the worker's bare-repo-push capability.
- How caveats serialize into the mesh wire types without breaking the
  "no broker, no JWT plumbing" simplicity the mesh decision deliberately chose.
