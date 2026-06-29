# Attenuation Is Not Enough: A Meet-Lattice Authority Algebra for LLM Agents and a Complete-Mediation Audit of Its Enforcement Floor

**Shawn Hartsock**
Gilamonster Foundation
`hartsock@users.noreply.github.com`

*Draft — workshop submission.*[^venues]

---

## Abstract

LLM agent harnesses run the operator's full ambient authority while taking
instruction from untrusted channels — model output, tool results, fetched
pages. This is Hardy's confused deputy at machine speed. We present *Caveats*,
an object-capability authority algebra in which an agent's authority is an
element of a bounded **meet-semilattice** $(L, \sqsubseteq, \sqcap, \top, \bot)$
over six independent axes (filesystem read/write, exec, network, a call-count
bound, and a causal generation window). Delegation is *attenuation-only*: every
composition is a meet, the implementation exposes no join, and so authority can
only narrow. We give the formal structure, hand-prove the greatest-lower-bound
and no-amplification properties, and corroborate them with property-based tests
(proof-sketch-plus-mechanized-corroboration — not a machine-checked theorem).
Our central result is methodological and, we argue, the more important one:
**algebraic soundness is necessary but not sufficient**. A sound lattice secures
a system only if authority is checked before every effect — *complete mediation*
(Saltzer & Schroeder 1975) specialized to capability axes, a condition we call a
**total enforcement floor**. We substantiate this with an audit of a deployed
agent runtime (`newt-agent`) that exposes two distinct execution engines. The
single-agent *coder* loop gates all four effect-bearing axes and is individually
near-total. The *crew/team* dispatch engine forwards the session grant to
sub-agents **without applying the meet** and consults only `fs_write` at the
member level — a genuine incomplete-mediation gap. Crucially, when we audited the
*effect surface* rather than merely the caveat call-sites, we found the reachable
harm is far narrower than a first reading of the call-graph suggests: crew
members run single-shot inference with no ambient read/exec/network tools, reads
are structurally confined to the worktree, runs are bounded by fixed retry/
subtask caps, and a human attestation gate sits ahead of dispatch. The accurate
finding is a *latent escalation vector* and a *sub-worktree read-scope
confidentiality gap*, not a demonstrated live privilege escalation. We report
the correction itself as the paper's sharpest lesson — our first audit examined
the telescope (the algebra) and not the sky (the reachable syscalls), the exact
error the thesis warns against — and position the whole as an experience report
against the complete-mediation canon.

---

## 1. Introduction

The dominant architecture for LLM agent harnesses fuses **identity** (who the
process is) with **authority** (what it may do). The harness executes as the
operator, with the operator's ambient credentials — shell access, filesystem,
network, cloud tokens — and then accepts its next action from channels that are,
by construction, untrusted: the model's own generated tokens, the textual output
of tools, the contents of fetched web pages. Any of these can be adversarial
(prompt injection, poisoned tool output, a malicious dependency's README). A
harness that holds full authority and takes orders from an untrusted channel is
the textbook **confused deputy** [Hardy 1988]: a trusted intermediary induced
to misuse authority it legitimately holds on behalf of an attacker who holds
none. The novelty here is only the speed and autonomy; the structure is
fifty years old.

The prevailing mitigations are *symptomatic*. Regex allow/deny lists, prompt-
injection classifiers, and "alignment" training all attempt to claw authority
back *after* identity and authority have already been fused, and all are
defeated by inputs their designers did not foresee. The structural fix is the
object-capability (ocap) discipline [Miller 2006]: separate identity from
authority, model authority as a partially ordered set, and make delegation
*attenuation-only* so that no operation — no sequence of operations — can ever
produce more authority than was held. Safety then follows from the *shape of the
algebra*, not from the model behaving.

This paper makes that program concrete for LLM agents and then complicates it.

**Thesis.** Agent authority should be an element of a bounded meet-semilattice
whose only composition operator is the meet (greatest lower bound). Because the
meet can never amplify, a fully compromised agent cannot exceed the down-set of
the capabilities it was minted with — *provided* the algebra is actually
consulted before every effect. That proviso is the whole game.

**Contributions.** We are explicit that the positive contributions (C1–C3) are
an *engineering synthesis* of well-established ideas, and that the methodological
contribution (C4) is an *experience report* restating a fifty-year-old principle
for a new substrate. We do not claim new theory.

- **C1 (Multi-axis caveat lattice).** A formal authority algebra, *Caveats*,
  as a product of six independent axes over two value domains — set-valued
  scopes and a numeric count bound — each a meet-semilattice with top and
  bottom, lifted coordinatewise to a bounded meet-semilattice on the product
  (§3).

- **C2 (The attenuation invariant as a structural confused-deputy bound).**
  Every authority boundary that *applies it* — minting a delegated key,
  composing a role preset, deriving a subtask's grant — is realized as a meet.
  Meet is monotone and non-amplifying, so authority narrows monotonically along
  every delegation chain *on which the invariant is applied* (§3.5).

- **C3 (Mechanically corroborated algebraic laws).** The partial-order axioms,
  the GLB laws, the monoid laws (commutative, associative, idempotent,
  $\top$-identity), and an explicit *never-amplifies* law are validated as
  property-based tests over randomized lattice elements. We report exactly what
  these guarantee — *proof-sketch-plus-mechanized-corroboration*, not a
  machine-checked theorem — and, just as importantly, what they do **not** (§4).

- **C4 (The enforcement-floor experience report — the key contribution).**
  Algebraic soundness is *necessary but not sufficient* for system soundness. A
  sound lattice secures the system only if authority is checked before every
  effect — **complete mediation** [Saltzer & Schroeder 1975], realized by an
  always-invoked **reference monitor** [Anderson 1972], here specialized to per-
  axis capability checks (a *total enforcement floor*). We audit a deployed
  runtime with two engines: a near-total single-agent loop and a crew dispatch
  engine that omits the attenuating meet and consults only one of the effect-
  bearing axes at the member level. We then audit the *effect surface* and find
  the reachable harm is much milder than the call-graph alone implies — and we
  report *that correction* as the central lesson (§5).

The honest punchline is C4, and it has two halves. First: a clean lattice is a
*telescope*, not the sky — it lets you reason about authority only to the extent
the running system actually looks through it; the sky is the set of real `read`,
`write`, `exec`, `connect` effects an agent can reach. Second, and more humbling:
*we ourselves first audited the telescope*. An early draft of this paper reasoned
from "`permits_net` is never called on the crew path" directly to "the sub-agent
opens network connections" — without checking whether a crew member has any
network effect surface at all. It does not. Correcting that overclaim (§5.5) is
the paper's most useful demonstration of its own thesis.

---

## 2. Background and Threat Model

### 2.1 Object capabilities, POLA, and the confused deputy

A **capability** is an unforgeable, transferable token that simultaneously
*designates* a resource and *authorizes* a specific set of operations on it
[Dennis & Van Horn 1966]. Authority is *transferred, not checked*: there is no
ambient table a deputy can consult to act "as" a more privileged principal,
because authority is carried in the references one holds. This is the structural
cure for Hardy's **confused deputy** [Hardy 1988]: a compiler that writes a
billing record to an attacker-named path because it holds the authority and was
merely *told* the path. With capabilities the compiler can only write where it
was *given* a write capability; being told a path conveys no authority.

Miller's object-capability model [Miller 2006] adds the **Principle of Least
Authority** (POLA — grant only what is needed), **robust composition** (systems
are safe to compose when they share only capabilities and no ambient authority),
and the **membrane** (attenuating proxy that filters or restricts an existing
capability). Denning's lattice model of information flow [Denning 1976]
established the complementary idea that security levels form a partial order with
meets and joins, enabling compositional, monotone reasoning. Macaroons
[Birgisson et al. 2014] make attenuation cryptographic: bearer tokens carry
*caveats* — restrictions baked into the token — that a holder may only add, never
remove, so possession of a token is possession of a point *at or below* the
authority of its issuer. Our axis name, *caveat*, is a deliberate homage; our
contribution is to give the caveat space an explicit lattice algebra and to
study its enforcement end-to-end.

### 2.2 Enforcement: complete mediation and the reference monitor

The algebra is only half of any access-control story; the other half is older
than capabilities. Anderson's **reference monitor** [Anderson 1972] is the
abstract component that mediates every access to every object and must be
*always invoked*, *tamper-proof*, and *small enough to verify*. Saltzer &
Schroeder's **complete mediation** [Saltzer & Schroeder 1975] is the design
principle that *every access to every object must be checked for authority* —
no caching of a once-granted decision past the point where the granting
conditions may have changed, and no path to an effect that skips the check. Our
"total enforcement floor" (§5) is precisely complete mediation specialized to
the six capability axes: every effect on axis $i$ must be preceded by a check of
the consuming authority's $i$-component.

This lineage continued into **decentralized information-flow control** (DIFC),
which pairs a security lattice (Denning) with *enforced* consumption points.
Jif/JFlow [Myers & Liskov 1999] enforces label flows at the language level;
Asbestos [Efstathopoulos et al. 2005], HiStar [Zeldovich et al. 2006], and
Flume [Krohn et al. 2007] enforce them at OS-abstraction boundaries. DIFC is the
closest prior art to "lattice + enforced floor," and our crew-path gap is, in
DIFC terms, a missing enforcement point on an otherwise-sound lattice. Our
novelty is not the principle but the *substrate* (an LLM-agent runtime with
recursive sub-agent dispatch) and the *experience* of finding — and
mis-diagnosing, then correctly diagnosing — such a gap in a live system.

### 2.3 The LLM-agent threat model

We assume the following adversary, which is stronger than the usual
prompt-injection framing.

- **Untrusted instruction channels.** The model's output, any tool's output,
  and any fetched content may be attacker-controlled. We do **not** assume the
  model is aligned, honest, or uncompromised. We treat the agent as *fully
  adversarial after minting*: the worst case is a compromised agent actively
  trying to escalate.

- **Untrusted plans.** Agents author and execute multi-step *plans*; a plan is
  model-generated data and is untrusted. A subtask's requested authority is a
  *request*, never a grant.

- **Dispatched crews.** An overseer agent may spawn sub-agents ("crews"/"teams")
  to do work in isolated worktrees. Recursion is the sharp case: a spawned
  sub-agent must not be a laundering route for authority the overseer does not
  hold. This is the confused deputy applied to delegation itself.

- **Mesh peers.** Agents may delegate across a signed peer mesh; a delegated
  key travels with a certificate chain that any peer can verify.

What is *out of scope* for the algebra (and we are explicit about this, because
conflating the two is how the enforcement gap in §5 hid): the algebra governs
*designation and ordering of authority*, not the *interpretation of a single
designator*. Whether the path `/repo` also authorizes `/repo/src` (prefix
semantics) is an **enforcement** question for the OS-level layer (e.g.
Landlock), not a property of the lattice; the lattice treats axis members as
exact tokens (`caveats.rs:29–36`).

**Security goal.** No sequence of agent actions — minting children, composing
presets, spawning crews, authoring plans — may cause any axis of *effective
reachable authority* to *rise above* the authority the agent was minted with. We
want this to hold *structurally*, independent of model behavior. Note the phrase
"effective reachable": the bound is over effects an agent can actually cause, not
over which caveat methods happen to be called — a distinction §5 shows is
load-bearing in both directions.

---

## 3. The Caveats Lattice

We develop the algebra bottom-up: the two axis domains, then the product, then
the invariant. All claims cite the canonical implementation in
`agent-mesh-protocol/src/caveats.rs`, re-exported by `agent-bridle-core`.

### 3.1 The Caveats type and its axes

An authority value is a six-tuple (`caveats.rs:142–156`):

$$
c \;=\; \langle\, c.\mathit{fsR},\; c.\mathit{fsW},\; c.\mathit{exec},\;
c.\mathit{net},\; c.\mathit{calls},\; c.\mathit{gen} \,\rangle
$$

Five axes ($\mathit{fsR}, \mathit{fsW}, \mathit{exec}, \mathit{net},
\mathit{gen}$) are *set-valued* scopes; one ($\mathit{calls}$) is a *numeric
bound*. The $\mathit{gen}$ axis is a **causal** generation window — it keys on
"flight $N$", never on wall-clock time (`caveats.rs:153–155`), so the algebra
carries no temporal nondeterminism.

### 3.2 The Scope axis (set-valued)

A scope over a totally ordered token type $T$ is (`caveats.rs:50–55`):

$$
\mathrm{Scope}\langle T\rangle \;::=\; \mathsf{All} \;\mid\; \mathsf{Only}(S),
\quad S \subseteq_{\text{fin}} T .
$$

$\mathsf{All}$ is the axis top $\top$ (`caveats.rs:60–62`); the empty bounded set
$\mathsf{Only}(\varnothing)$ is the axis bottom $\bot$ (authorizes nothing,
`caveats.rs:66–68`). The partial order is (`caveats.rs:77–86`):

$$
x \sqsubseteq y \;\iff\;
\begin{cases}
\text{true} & y = \mathsf{All}\\
\text{false} & x = \mathsf{All},\; y = \mathsf{Only}(\cdot)\\
a \subseteq b & x = \mathsf{Only}(a),\; y = \mathsf{Only}(b).
\end{cases}
$$

The meet is (`caveats.rs:91–96`):

$$
x \sqcap y \;=\;
\begin{cases}
y & x = \mathsf{All}\\
x & y = \mathsf{All}\\
\mathsf{Only}(a \cap b) & x = \mathsf{Only}(a),\; y = \mathsf{Only}(b).
\end{cases}
$$

**Lemma 1 (Scope is a bounded meet-semilattice).** $(\mathrm{Scope}\langle
T\rangle, \sqsubseteq, \sqcap, \mathsf{All}, \mathsf{Only}(\varnothing))$
satisfies: $\sqsubseteq$ is a partial order; $x \sqcap y$ is the greatest lower
bound of $x, y$; $\mathsf{All}$ is the identity ($x \sqcap \mathsf{All} = x$) and
top ($x \sqsubseteq \mathsf{All}$); $\mathsf{Only}(\varnothing)$ is the bottom
($\mathsf{Only}(\varnothing) \sqsubseteq x$ for all $x$). (Order-theoretically a
full lattice — joins exist — but the implementation withholds $\sqcup$; see §3.4.)

*Proof.* By cases. $\sqsubseteq$ reflexive/antisymmetric/transitive reduces to
the same properties of $\subseteq$ on finite sets plus the top clause.
$x \sqcap y$ is a lower bound: $\mathsf{Only}(a\cap b) \sqsubseteq
\mathsf{Only}(a)$ since $a \cap b \subseteq a$, and the $\mathsf{All}$ cases are
the identity; it is *greatest* because any $z \sqsubseteq x, y$ is either
$\mathsf{All}$ (only when $x = y = \mathsf{All}$, where the bound is
$\mathsf{All}$) or $\mathsf{Only}(s)$ with $s \subseteq a$ and $s \subseteq b$,
hence $s \subseteq a \cap b$, i.e. $z \sqsubseteq x \sqcap y$. Bottom is
immediate: $\varnothing \subseteq b$ for every finite $b$, and
$\mathsf{Only}(\varnothing) \sqsubseteq \mathsf{All}$. $\square$

### 3.3 The CountBound axis (numeric)

The call-count bound is (`caveats.rs:105–110`):

$$
\mathrm{CountBound} \;::=\; \mathsf{Unlimited} \;\mid\; \mathsf{AtMost}(n),
\quad n \in \mathbb{N}.
$$

with top $\mathsf{Unlimited}$ (`caveats.rs:115–117`), bottom $\mathsf{AtMost}(0)$,
order (`caveats.rs:121–127`)

$$
x \sqsubseteq y \;\iff\;
\begin{cases}
\text{true} & y = \mathsf{Unlimited}\\
\text{false} & x = \mathsf{Unlimited},\; y = \mathsf{AtMost}(\cdot)\\
a \le b & x = \mathsf{AtMost}(a),\; y = \mathsf{AtMost}(b),
\end{cases}
$$

and meet the tighter bound (`caveats.rs:131–136`):
$\mathsf{AtMost}(a) \sqcap \mathsf{AtMost}(b) = \mathsf{AtMost}(\min(a,b))$,
with $\mathsf{Unlimited}$ the identity.

**Lemma 2 (CountBound is a bounded meet-semilattice, and is a chain).** Unlike a
general $\mathrm{Scope}\langle T\rangle$ — which is *not* totally ordered — every
two elements of $\mathrm{CountBound}$ are comparable. It is therefore a **chain**,
order-isomorphic to $\mathbb{N} \cup \{\infty\}$ under $\le$ (with $\infty =
\mathsf{Unlimited}$ as top and $0 = \mathsf{AtMost}(0)$ as bottom), and the meet
$\min$ is the GLB of that chain. *Proof:* immediate from $(\mathbb{N},\le)$ being
a total order with $\min$ as GLB, plus the top/identity clause
(`caveats.rs:99–102`). $\square$ (We correct an earlier statement that claimed
$\mathrm{CountBound}$ is order-isomorphic to "a Scope axis"; it is not — a Scope
over $|T|>1$ contains incomparable elements, so the two cannot be isomorphic.
The shared abstraction is "bounded meet-semilattice," not "same poset.")

### 3.4 The product lattice

`Caveats` is the coordinatewise product. The order is per-axis conjunction
(`caveats.rs:177–184`):

$$
a \sqsubseteq b \;\iff\;
\bigwedge_{i \in \{\mathit{fsR},\mathit{fsW},\mathit{exec},\mathit{net},
\mathit{calls},\mathit{gen}\}} a.i \sqsubseteq_i b.i .
$$

The meet is per-axis meet (`caveats.rs:189–198`):

$$
(a \sqcap b).i \;=\; a.i \sqcap_i b.i \quad\text{for each axis } i,
$$

the top is the per-axis top (`caveats.rs:162–171`), i.e. unrestricted on every
axis — equivalently "no caveats", the user's full authority; this is also the
`Default` (`caveats.rs:201–207`), chosen for backward compatibility so that
metadata declaring no caveats means $\top$. The bottom is the per-axis bottom,
$\bot = \langle \mathsf{Only}(\varnothing),\,\mathsf{Only}(\varnothing),\,
\mathsf{Only}(\varnothing),\,\mathsf{Only}(\varnothing),\,\mathsf{AtMost}(0),\,
\mathsf{Only}(\varnothing)\rangle$ — authorizes nothing on any axis. Both $\top$
and $\bot$ are constructible values, which is why we call the structure
*bounded*.

**Theorem 1 (Caveats is a bounded meet-semilattice).** Let each axis $i$ be a
bounded meet-semilattice $(L_i, \sqsubseteq_i, \sqcap_i, \top_i, \bot_i)$ (Lemmas
1–2). Then $L = \prod_i L_i$ under the coordinatewise order and meet, with top
$\top = \langle \top_i \rangle_i$ and bottom $\bot = \langle \bot_i \rangle_i$, is
a bounded meet-semilattice.

*Proof.* Coordinatewise products of partial orders are partial orders. For the
GLB: $a \sqcap b$ is a lower bound because on each axis $(a\sqcap b).i =
a.i \sqcap_i b.i \sqsubseteq_i a.i$ (and $\sqsubseteq_i b.i$), and the product
order is the conjunction of axis orders. It is *greatest* because any common
lower bound $c$ satisfies $c.i \sqsubseteq_i a.i$ and $c.i \sqsubseteq_i b.i$ on
every axis, so by the axis GLB property $c.i \sqsubseteq_i a.i \sqcap_i b.i =
(a\sqcap b).i$, and conjoining over axes gives $c \sqsubseteq a \sqcap b$.
Identity/top: $(a \sqcap \top).i = a.i \sqcap_i \top_i = a.i$ and
$a.i \sqsubseteq_i \top_i$ on each axis; dually $\bot \sqsubseteq a$. $\square$

**Theorem 2 (Greatest lower bound).** For all $a, b \in L$:

1. **Lower bound:** $a \sqcap b \sqsubseteq a$ and $a \sqcap b \sqsubseteq b$.
2. **Greatest:** $\forall c \in L.\; (c \sqsubseteq a \wedge c \sqsubseteq b)
   \Rightarrow c \sqsubseteq a \sqcap b$.

Parts (1)–(2) are restated from Theorem 1 as the two obligations the security
argument leans on. We *no longer* bill "no amplification" as a separate theorem:
the statement "$a \sqcap b \sqsubseteq a$, with equality iff $a \sqsubseteq
a\sqcap b$" is just part (1) together with antisymmetry of $\sqsubseteq$, not an
independent result. We retain *the phrase* "no amplification" only as a named
convenience for §3.5, and its mechanized analogue is `meet_never_amplifies`
(§4).

**Crucial structural fact — there is no join.** Order-theoretically each axis
(and hence the product) is a full lattice: joins exist. But the *implementation*
exposes no $\sqcup$ operator — no method, anywhere, takes two authorities and
returns one *above* either (`caveats.rs` defines only `top`, `none`, `leq`,
`meet` per axis). Authority can be *named* at any level only by minting (and
$\top$ only by the root user, who is the root of every chain) and thereafter
only *narrowed* via meet. This withheld-join asymmetry — not the GLB proof — is
what does the safety work, and it is the genuinely load-bearing observation of
the formal section.

### 3.5 The attenuation invariant

> **Attenuation invariant.** *Where a boundary applies it*, an effective
> authority $e$ is computed from a granted authority $g$ and a requested/role/
> preset authority $r$ as
> $$ e \;=\; g \sqcap r, \qquad\text{hence}\qquad e \sqsubseteq g . $$

Because $\sqcap$ is monotone and a lower bound (Theorem 2(1)), composing such
boundaries can only descend the order. Three boundaries instantiate it; §5 shows
a fourth (crew dispatch) that does *not*, which is the entire point.

- **Delegated minting (mesh).** A child key is mintable iff
  $\mathit{child} \sqsubseteq \mathit{parent}$; otherwise minting is *refused*
  (`agent_key.rs:62–64`, returning `CaveatAmplification`). Verification re-checks
  $\mathit{child} \sqsubseteq \mathit{parent}$ at *every link* of the certificate
  chain (`agent_key.rs:228–231`), so a forged or tampered chain that amplifies
  authority is rejected even when each individual signature is valid.

- **Role/preset composition.** A named permission preset contributes a *ceiling*
  `clamp()`; the session's effective authority is
  $\mathit{base} \sqcap \mathit{role} \sqcap \mathit{preset}$, and the clamp "can
  only attenuate, never widen" (`role_profile.rs:241–247`).

- **Plan leaf derivation.** A plan *requests*, the parent *grants*, the meet
  *enforces* $\sqsubseteq$: a leaf's caveats are
  $\mathit{parent} \sqcap \mathit{policy}$ (`plan.rs:266–274`).

**Proposition 1 (Monotone narrowing along chains).** If every boundary on a
delegation path applies the attenuation invariant, then the effective authority
$e_k$ after $k$ boundaries satisfies $e_k \sqsubseteq e_{k-1} \sqsubseteq \cdots
\sqsubseteq e_0 \sqsubseteq \top$. *Proof:* each step is a meet, so
$e_j = e_{j-1} \sqcap r_j \sqsubseteq e_{j-1}$ by Theorem 2(1); transitivity
chains them. $\square$

**Corollary (Structural confused-deputy bound).** *Provided the attenuation
invariant holds at every boundary on a chain*, no agent on that chain can be
granted authority exceeding $e_0$, its minted grant — regardless of model
behavior. This is the algebra-level neutralization of the confused deputy
promised in §1.

The corollary's hypothesis — *every* boundary applies the meet — is load-bearing,
and it is only half of what system safety needs. The other half is that every
*effect* consults the resulting authority (complete mediation, §2.2). §5 is the
story of a boundary that skips the meet, and of what that does — and does not —
imply once the effect surface is also audited.

---

## 4. Mechanized Laws

The algebra ships with property-based tests (`proptest`) that validate the
lattice laws over randomly generated elements drawn from a generator covering
$\mathsf{All}$, small bounded scopes over a 4-symbol alphabet, $\mathsf{Unlimited}$,
and small numeric bounds on every axis (`caveats.rs:304–336`). Each law is a
$\forall$-quantified statement checked against hundreds of randomized cases per
run. We call this *mechanized corroboration*, not machine-checking: `proptest` is
randomized testing over a bounded generator, not an exhaustive or symbolic proof.

| Law | Statement | Test (`caveats.rs`) |
|---|---|---|
| Reflexivity | $a \sqsubseteq a$ | `leq_reflexive` (340–343) |
| Antisymmetry | $a \sqsubseteq b \wedge b \sqsubseteq a \Rightarrow a = b$ | `leq_antisymmetric` (345–350) |
| Transitivity | $a \sqsubseteq b \wedge b \sqsubseteq c \Rightarrow a \sqsubseteq c$ | `leq_transitive` (352–357) |
| Lower bound | $a\sqcap b \sqsubseteq a$ and $\sqsubseteq b$ | `meet_is_lower_bound` (360–365) |
| Greatest LB | $c\sqsubseteq a \wedge c\sqsubseteq b \Rightarrow c \sqsubseteq a\sqcap b$ | `meet_is_greatest_lower_bound` (367–373) |
| Commutativity | $a\sqcap b = b\sqcap a$ | `meet_commutative` (376–379) |
| Associativity | $(a\sqcap b)\sqcap c = a\sqcap(b\sqcap c)$ | `meet_associative` (381–384) |
| Idempotence | $a\sqcap a = a$ | `meet_idempotent` (386–389) |
| $\top$-identity | $a\sqcap\top = a$ and $a\sqsubseteq\top$ | `top_is_meet_identity` (391–395) |
| No amplification | $a\sqcap b\sqsubseteq a,b$; and $a\sqsubseteq a\sqcap b \Rightarrow a = a\sqcap b$ | `meet_never_amplifies` (400–408) |

Together these corroborate, to the assurance level of randomized property
testing, that $(L, \sqsubseteq, \sqcap, \top, \bot)$ is a bounded, commutative,
idempotent meet-monoid in which the meet is the GLB and never amplifies — the
mechanized counterpart of Theorems 1–2.

**What these guarantee.** That the *data type* behaves as an attenuation-only
authority lattice: composition is order-theoretically safe, deterministic,
order-independent (commutative/associative), and stable (idempotent). The
"headline safety property" the source itself names — *"meet can never amplify"*
(`caveats.rs:397–399`) — holds.

**What these do *not* guarantee — and this is the crux of §5.**

1. **Coverage is not proof.** Property tests over a 4-symbol alphabet and small
   bounds give strong evidence, not a theorem. The hand proofs in §3 are the
   theorem; the tests are corroboration. We label any property *only* checked by
   `proptest`, and not hand-proved here, as **proof-sketch-level** assurance.

2. **The tests exercise the algebra, not the system.** Every test calls `meet`
   and `leq` *directly*. None establishes that the running agent *invokes* `meet`
   at a boundary, or that a `permits_*` check precedes a real effect. The laws
   are about $L$; system safety is about the *call graph that consumes $L$* —
   and, as §5.5 stresses, about the *effect surface* reachable past that call
   graph. A green test suite is consistent with both a wide-open enforcement
   hole *and* with a hole that no reachable effect can exercise; the suite
   distinguishes neither.

In Dijkstra's terms: the property suite is a very good telescope for the
algebra. It tells you nothing about whether the running system is pointed at it.

---

## 5. From Algebra to System: The Enforcement Floor

### 5.1 The principle (complete mediation, restated for capability axes)

A capability algebra confers a system-level guarantee only through its
*consumption sites* — the points where code converts "authority $c$" into a
concrete effect (`read`, `write`, `exec`, `connect`) and must therefore consult
$c$ first. This is exactly Anderson's reference monitor [1972] and Saltzer &
Schroeder's complete mediation [1975], narrowed to per-axis checks. Define:

> **Enforcement floor.** The set of all consumption sites, each annotated with
> the axes it checks before acting. The floor is **total** iff *every* site
> checks *every* axis relevant to an effect it can actually produce (and applies
> the attenuation invariant when it derives a child authority).

> **Enforcement-floor principle (complete mediation for capability axes).** Let
> the authority algebra be sound (Theorems 1–2) and the attenuation invariant
> hold at all *minting* boundaries. The system upholds the security goal of §2.3
> *iff* the enforcement floor is total. Soundness of the algebra is **necessary
> but not sufficient**; a single site that can produce an effect on axis $i$
> without checking $a.i$ admits an effect on axis $i$ unbounded by the agent's
> grant, defeating the structural bound for that axis.

This is not a new lemma — it is complete mediation, 1975, specialized. We state
it explicitly because the field's enthusiasm for clean capability models risks
forgetting it. The argument is the standard one.

*Argument.* ($\Leftarrow$) If the floor is total, every effect is preceded by a
check against an authority that, by Proposition 1, is $\sqsubseteq$ the minted
grant; so no effect exceeds the grant on any axis. ($\Rightarrow$)
Contrapositive: suppose some reachable site can emit an effect on axis $i$
without checking $a.i$. Then an adversarial agent routes its action through that
site; the effect occurs regardless of $a.i$, so authority on axis $i$ is
effectively $\top_i$ at that site — the bound is broken for $i$. $\square$

The principle's value is that it redirects attention from the part that is easy
and satisfying to verify (the algebra) to the part that is tedious and where the
real bugs live (the floor). Two refinements matter for the case study, and the
second is the one we initially missed:

- **"Axis consulted" ≠ "effect bounded."** A site that never calls `permits_net`
  fails to *check* the net axis. Whether that *matters* depends on whether the
  site can produce a network effect at all. The principle's contrapositive
  requires a *reachable* effect; absent one, the unchecked axis is a
  latent gap, not an open door.

- **A latent gap is still a finding.** An unchecked axis with no current effect
  surface becomes a live hole the instant that surface is added (e.g. a future
  tool). It is a defense-in-depth and maintainability defect even when not
  presently exploitable.

We now show the floor is *not* total in a deployed runtime — and then, in §5.5,
audit the effect surface to size the gap honestly.

### 5.2 Two execution engines, not one floor

The `newt-agent` runtime has **two architecturally distinct execution engines**
that consume `Caveats`. They are not one floor with holes; they are two engines
with different, individually-assessable enforcement strategies. Conflating them
was a source of error in our first audit.

**Engine A — the single-agent coder loop (`newt-coder`).** A tool-driven agent
loop with real read/write/network effect surfaces. It gates all four effect-
bearing axes before the corresponding effect:

- `fs_read`: every file injected into the prompt is checked (`coder.rs:485–495`,
  the check at `:488`).
- `fs_write`: every target path is checked before any write, all-or-nothing so a
  late denial cannot leave a half-write (`coder.rs:393–399`, check at `:394`).
- `net`: the backend's endpoint host is checked before the inference call
  (`coder.rs:465–478`, check at `:471`); mock/in-process backends with no
  endpoint skip vacuously.
- `max_calls`: the retry budget is checked before an additional model call
  (`coder.rs:270`, `caveats.max_calls.permits_one_more(calls_used)`).

This engine is **architecturally disjoint from crews**: `newt-scheduler` does
*not* depend on `newt-coder` (verified — no `newt-coder` entry in
`newt-scheduler/Cargo.toml`). Its only unchecked axis is `exec` (the coder loop
has no general shell-exec effect of its own; verification commands run elsewhere).
For its actual effect surface, Engine A is a near-total floor.

**Engine B — the crew/team scheduler (`newt-scheduler` + `newt-cli`).** A fixed
pipeline — `navigate → curate → plan → apply → verify → triage → revise`
(`crew.rs:243–412` for `run_crew`; `run_team` at `:169`) — in which each role is
a **single-shot chat completion**: `LocalDispatcher::dispatch` calls
`backend.complete(req)` with *no tool loop, no MCP, no shell, no read/net tool*
(`dispatch.rs:46–66`). A crew member can only (a) *name* files for the harness's
curate step to read and (b) *emit* full-file edits as text. Its enforcement is:

- **At the dispatch entrypoint (`crew_runner.rs`):** a step-up/attestation gate
  (`crew_authz`, `:246–256`) holds dispatch unless a human presence was
  established; then a workspace-level `fs_write` gate (`:259`) fails closed for a
  read-only session; then, *only* for a caller-supplied `verify` string, an
  `exec` check (`:276`). It then calls `run_crew`/`run_team` passing the session
  `caveats` **directly, with no `meet`** (`crew_runner.rs:324, 329`). The code
  comment is candid: *"per-crew-member caveat enforcement is a follow-up
  (run_crew does not yet thread caveats to members)"* (`:233–234`).
- **At the member leaf (`crew.rs`):** a crew member's edits are partitioned by
  `permits_fs_write` per file (`crew.rs:348`) — so `fs_write` *is* enforced at
  the leaf — and, independently, every edit/read target is structurally confined
  to the worktree by `is_safe_worktree_path` (`newt-cli/src/crew.rs:42–47`,
  applied in `read` at `:187–192` and `apply` at `:198–207`). No `fs_read`,
  `net`, `exec`, or `max_calls` *caveat* is consulted on the member path.

So Engine B applies no `meet` at its entrypoint and consults only the `fs_write`
*caveat* at the member level. By the enforcement-floor principle it is *not*
total. This contradicts the seam's own documentation, which claims the runner
*"runs every spawned crew under `meet`-attenuated caveats (never the session's
full grant), so the overseer cannot escalate by dispatching a crew"*
(`crew_tool.rs:13–16`). The docstring describes the invariant the principle
requires; the implementation does not apply it.

**The plan path is *not* a third, total engine.** Plan execution leaves dispatch
through the **same Engine B**: `plan_exec.rs:123` calls
`runner.dispatch("crew", &args, &task.caveats)`. The plan path's distinctive
discipline is real but narrow: it *applies the meet* at leaf derivation
(`plan.rs:266–274`) and it *exec-gates the model-authored verify command*
before forwarding it (`plan_exec.rs:119`, `task.caveats.permits_exec(v)`,
fail-closed). But `plan_exec.rs` itself calls *only* `permits_exec` — there is no
`permits_fs_read`, `permits_net`, or call-count check anywhere in it, and the
crew members it spawns get the identical member-level enforcement (`fs_write`
plus the worktree boundary). The accurate contrast is therefore **boundary
discipline, not totality**: *plan attenuates via meet and exec-gates the verify
command; crew does neither at its entrypoint.* Neither path is a total floor.

### 5.3 The audit matrices

We audited each consumption site for two distinct questions the principle keeps
separate (§5.1): **(A) which axes does the site *consult*?** and **(B) which
effects on each axis can actually be *reached* past the site?** Conflating these
is precisely the error we corrected (§5.5).

**Table A — axes consulted ($\checkmark$ checked, $\times$ not checked,
$\circ$ partial/provenance-restricted, n/a not applicable). "`meet`" = does the
site apply the attenuation invariant.** Engine A (`coder.rs`) is listed last and
ruled off, because it is a *separate engine*, not a crew consumer.

| Axis | `crew.rs` (B, member) | `crew_runner.rs` (B, entry) | `team.rs` (B) | `plan_exec.rs` (→B) | ‖ `coder.rs` (A) |
|---|:---:|:---:|:---:|:---:|:---:|
| `fs_read` | $\times$ | $\times$ | $\times$ | $\times$ | ‖ $\checkmark$ |
| `fs_write` | $\checkmark$ | $\checkmark$ | $\times$ | $\times$ | ‖ $\checkmark$ |
| `exec` | $\times$ | $\circ$ | $\times$ | $\checkmark$ | ‖ $\times$ |
| `net` | $\times$ | $\times$ | $\times$ | $\times$ | ‖ $\checkmark$ |
| `max_calls` | $\times$ | $\times$ | $\times$ | $\times$ | ‖ $\checkmark$ |
| `meet` applied | $\times$ | $\times$ | $\times$ | $\checkmark$ | ‖ n/a |

Two corrections to an earlier version of this matrix: `plan_exec.rs` does **not**
check `fs_read` (it calls only `permits_exec`); and `coder.rs` does check
`max_calls` (`coder.rs:270`) — so `max_calls` is *not* "enforced nowhere." The
earlier "`max_calls` is decorative" claim was simply wrong.

**Table B — effect surface reachable on the Engine-B member path.** This is the
table the first draft omitted, and it changes the severity assessment.

| Axis | Caveat consulted at member? | Effect surface a crew member can reach |
|---|:---:|---|
| `fs_read` | $\times$ | Worktree files only: the curate step reads files the navigator *names*, but `is_safe_worktree_path` refuses absolute/`..` paths, so host files (`/etc/shadow`, …) are unreachable. Residual: a read scope *narrower than the worktree* (e.g. `Only({/repo/src})`) is not honored — in-worktree-but-out-of-sub-scope file contents can be curated into the prompt sent to the configured LLM endpoint. |
| `fs_write` | $\checkmark$ (`crew.rs:348`) | Worktree files only, and only those `permits_fs_write` allows; doubly bounded by `is_safe_worktree_path`. Not an arbitrary-write surface. |
| `exec` | $\times$ (member); $\circ$ (entry) | **None for a member's own actions** — single-shot inference has no shell tool. The only exec is the `verify` command, which is provenance- and exec-gated at the entrypoint (`crew_runner.rs:276`). |
| `net` | $\times$ | **None for a member** — there is no `net` tool in the pipeline. The only egress is the harness's own inference call to the operator-configured backend. |
| `max_calls` | $\times$ | Structurally bounded by `MAX_ATTEMPTS = 3` and `MAX_SUBTASKS = 4` (`crew_runner.rs:28–29`), not by the caveat. Not "unbounded." |

### 5.4 The accurate finding (downgraded from "live escalation")

Putting Tables A and B together, the defensible finding is narrower than an
algebra-only reading suggests, and we state it as such:

> **Finding.** The Engine-B crew entrypoint forwards the session grant to
> sub-agents **without applying the meet** (`crew_runner.rs:324, 329`), and the
> member path consults only the `fs_write` caveat (`crew.rs:348`). This is a
> genuine **incomplete-mediation gap**. Its *present* consequence is (i) a
> **sub-worktree `fs_read` confidentiality gap** — in-worktree files outside a
> narrower granted read scope can be fed to the operator-configured LLM
> endpoint via the curate step — and (ii) a **latent escalation vector**: if
> crew members ever gain ambient read/net/exec tools, the missing meet and the
> unchecked axes become directly exploitable. It is **not** a demonstrated live
> privilege escalation: there is no reachable host-file read, no crew-member
> network egress, no arbitrary write (the worktree boundary holds), no unbounded
> call count, and a human attestation gate sits ahead of dispatch.

This also corrects the threat narrative. The earlier framing — *"the escalation
requires only choosing the dispatch verb"* — is wrong twice over: choosing the
crew verb additionally requires clearing the `crew_authz` step-up/attestation
gate (`crew_runner.rs:246–256`), and even once cleared it does not open the
network/exec/host-read effects the earlier draft claimed. We present **no PoC**,
because the milder finding does not support one; we present the gap as a
defense-in-depth and confidentiality defect that should be closed before any
ambient-tool crew member ships.

### 5.5 A reflexive caveat: we first audited the telescope, not the sky

The most useful thing this case study does is fail in the exact way the paper's
thesis predicts, and then recover. An early draft reasoned *purely from the
caveat call-sites*: "`permits_net` is never called on the crew path" → "the
sub-agent opens network connections the grant forbids"; "`max_calls` is never
checked" → "runs an unbounded number of tool calls"; "`fs_read` is never checked"
→ "reads outside the granted read scope." Each inference moved from *the axis is
unchecked* (a fact about the algebra's consumption sites — the telescope) to *the
effect occurs* (a claim about reachable syscalls — the sky) **without auditing
whether the effect surface existed.** It does not: a crew member runs single-shot
inference with no ambient tools (`dispatch.rs:46`), reads are worktree-confined
(`is_safe_worktree_path`), runs are capped (`MAX_ATTEMPTS`/`MAX_SUBTASKS`), and a
human gate precedes dispatch. The overclaim was a textbook instance of mistaking
the algebra for the secured system — the precise error C4 names.

We keep this section rather than quietly fixing the draft because the lesson
generalizes: an enforcement audit must enumerate *effect surfaces*, not just
*authority checks*. A missing check on an axis with no reachable effect is a
latent gap; a missing check on an axis with a live effect is an open door; and
the two demand different urgency. An audit that cannot tell them apart will
either cry wolf (our first draft) or miss real holes.

### 5.6 What the case study proves

- **Algebraic soundness travels; mediation does not.** The *same* `Caveats`
  value flows through both engines. Its lattice laws are identical. The
  difference is entirely in whether the consuming code applied the meet and
  checked the axes *that gate a reachable effect*. Soundness is a property of
  $L$; safety is a property of the call graph *and its effect surfaces*. C4.

- **Documentation is not enforcement.** A docstring asserting the invariant
  (`crew_tool.rs:13–16`) is worth nothing if the path it describes omits the
  `meet` (`crew_runner.rs:233–234, 324, 329`). Worse, it misleads auditors into
  assuming the floor is total. We recommend any claim of attenuation be
  *co-located with the `meet` call that realizes it*, with absence a CI-failing
  lint, not a prose "follow-up."

- **The fix is mechanical; total mediation is not.** Threading
  `caveats.meet(&member_policy)` into `run_crew`/`run_team` and adding `fs_read`/
  `net`/`max_calls` checks at the member sites closes this gap — but closing one
  hole is not the same as a total floor. Totality must be established by
  enumeration of *all* sites *and their effect surfaces*, ideally enforced by a
  type-level obligation (an effect that cannot be produced without a witness that
  the relevant axis was checked) rather than by audit. We sketch this as future
  work (§8); we do **not** claim to have built it.

---

## 6. Evaluation

We evaluate along two axes: coverage of the algebraic laws, and totality of the
enforcement floor — keeping "axis consulted" and "effect reachable" distinct
throughout (§5.1).

**Law coverage.** All ten laws in §4 are covered by property tests
(`caveats.rs:338–409`), exercising both axis domains and the product. We claim
*proof-sketch-plus-mechanized-corroboration* assurance: §3 gives hand proofs of
Theorems 1–2; §4 corroborates over randomized inputs. We do **not** claim a
machine-checked theorem (e.g. in Coq/Lean); that is future work and we flag the
distinction rather than overstate it.

**Enforcement audit.** Tables A and B in §5.3 are the evaluation. The runtime has
two engines:

- **Engine A (single-agent coder).** Consults `fs_read` (`coder.rs:488`),
  `fs_write` (`:394`), `net` (`:471`), and `max_calls` (`:270`) before the
  matching effect; `exec` is the one unchecked axis but the loop exposes no
  general exec effect. For its effect surface this engine is **near-total**.

- **Engine B (crew/team + plan-into-crew).** Applies the meet only on the plan
  path (`plan.rs:274`); exec-gates only the model-authored verify command (plan
  `plan_exec.rs:119`, entry `crew_runner.rs:276`); consults the `fs_write` caveat
  at entry (`:259`) and per-edit at the member (`crew.rs:348`); and consults no
  `fs_read`/`net`/`max_calls` caveat anywhere on the member path. It is **not
  total**. *But* its reachable effect surface is small (Table B): worktree-
  confined reads/writes, no member-level net or exec, structurally capped runs.

**Corrections to a prior version of this evaluation.** (1) `max_calls` is **not**
"0/5, spent nowhere" — it is enforced on Engine A at `coder.rs:270`. The correct
statement is "enforced on the single-agent path, not on the crew/plan dispatch
path." (2) The plan path is **not** "total"; it shares Engine B and checks only
`exec` itself. (3) The matrix now separates "axis consulted" (Table A) from
"effect surface reachable" (Table B), so a `×` on `net` for the crew path reads
correctly as "no network effect is reachable there," not "network is wide open."

**Result.** The end-to-end deployed guarantee is the *intersection* of what every
reachable path enforces, with the adversary choosing the weakest. Engine A
bounds all four effect-bearing axes for its surface. Engine B bounds `fs_write`
(caveat + worktree) and, via structure rather than the algebra, bounds the
otherwise-unchecked axes (no member net/exec surface; capped calls; worktree-
confined reads). The one substantive residual is the **sub-worktree `fs_read`
confidentiality gap** plus the **latent escalation vector** the missing meet
creates for any future ambient-tool crew member. We report this as a finding and
a fix-list, not a fixed result, and not a live privilege escalation.

---

## 7. Related Work

**Enforcement canon.** The principle our case study turns on is not new: it is
Anderson's **reference monitor** [1972] (mediate every access; always invoked)
and Saltzer & Schroeder's **complete mediation** [1975] (every access to every
object checked for authority), specialized to per-axis capability checks. We
position the enforcement-floor "principle" as exactly this restatement, and C4 as
an *experience report* of finding — and first mis-diagnosing — an incomplete-
mediation gap in an LLM-agent runtime, not as a fresh theorem.

**DIFC: lattice + enforced floor.** The closest prior art pairs a Denning
security lattice with enforced consumption points. Jif/JFlow [Myers & Liskov
1999] enforces information-flow labels at the language level; Asbestos
[Efstathopoulos et al. 2005], HiStar [Zeldovich et al. 2006], and Flume
[Krohn et al. 2007] enforce them at OS-abstraction boundaries. Our crew-path gap
is, in DIFC terms, a missing enforcement point on an otherwise-sound lattice;
our distinctive substrate is recursive LLM sub-agent dispatch, and our
distinctive observation is the "axis consulted vs. effect reachable" gap (§5.5).

**Foundational object capabilities.** Capabilities as unforgeable authority-
bearing references originate with Dennis & Van Horn [1966]; Hardy [1988] named
the confused deputy as the failure mode of ambient-authority/ACL designs.
Capability microkernels — KeyKOS [Bomberger et al. 1992], EROS [Shapiro et al.
1999], and the formally verified seL4 [Klein et al. 2009] — showed capability
discipline is both implementable and machine-verifiable. Miller [2006]
formalized the modern ocap model: POLA, robust composition, and membranes
(attenuating proxies). Caja [Miller et al. 2008] and Secure ECMAScript retrofit
the discipline onto high-level languages. Our $\mathsf{Only}(\cdot)$ scope with
intersection-as-meet is a membrane in lattice form; our novelty is not the
capability but the *explicit product-lattice algebra* over agent-relevant axes
and the end-to-end enforcement audit.

**Authority lattices and caveat delegation.** Denning [1976] established lattices
as the structure of secure information flow, with monotone, compositional
reasoning. Macaroons [Birgisson et al. 2014] make attenuation a cryptographic,
add-only caveat discipline on bearer tokens — the most direct ancestor of our
*caveat* axis and our cert-chain re-check (`agent_key.rs:228–231`). Where
macaroons attenuate by appending opaque predicate caveats verified at the target,
we give the caveat space an explicit meet-semilattice with a decidable order and
a withheld-join property, and — critically — we study what happens *below* the
token: whether the runtime consuming it actually mediates every effect. RBAC
[Ferraiolo & Kuhn 1992] and ABAC are policy-evaluation models with a central
decision point; they do not transfer authority and do not structurally prevent
the confused deputy, which is our target.

**LLM-agent security.** Adversarial-prompt and jailbreak work [Zou et al. 2023;
Carlini et al. 2024] and indirect prompt-injection studies [Greshake et al. 2023]
establish that instruction channels to an agent are untrusted — the premise of
our threat model — but propose detection/robustness, which is symptomatic in our
framing. Tool-use frameworks [Schick et al. 2023] scope *which* tools an agent
may call, typically via prompt-level tool lists, i.e. *prompt-based* enforcement
that a compromised model can ignore. Agent-security benchmarks such as AgentDojo
[Debenedetti et al. 2024] empirically measure attack/defense efficacy on tool-
using agents but do not provide a lattice-ordered authority algebra. To our
knowledge no deployed agent system combines a *lattice-ordered, attenuation-only*
authority algebra with a mechanically-corroborated law suite and an explicit
*complete-mediation audit* across dispatch paths.

**Positioning and honest novelty.** Our positive contributions (C1–C3) are an
engineering synthesis of well-established ideas — ocap, Denning lattices,
macaroon caveats — specialized to agent axes and property-tested. We do not
overclaim them as new theory. C4 is an *experience report*: complete mediation,
restated for capability axes, with a real runtime audit that (a) found a genuine
incomplete-mediation gap and (b) caught its own initial overclaim about the
effect surface. The field's enthusiasm for clean capability models risks exactly
the error we document — and committed — mistaking a verified algebra for a
secured system.

---

## 8. Discussion and Limitations

**Human-grant interaction.** Authority can be widened only at the top, by the
root user who anchors every cert chain (`agent_key.rs`); an interactive
permission grant re-clamps via `meet` (per the role/preset boundary,
`role_profile.rs:241–247`). We have *not* formally modeled the case where a human
*raises* a session mid-run; in the algebra that is naming a *new, higher* element
(legitimate, since the human is root), not an agent-reachable join. The
interaction between a mid-run human elevation and in-flight delegated children is
unmodeled and a likely source of subtle bugs; we flag it as **open**.

**Recursion bound.** Crew/team dispatch is recursive (an overseer spawns
sub-agents that may themselves spawn). The attenuation invariant gives a
*depth-independent* bound *if* applied at every level (Proposition 1). On the
crew path it is applied at *no* level (§5), so recursion currently compounds the
missing meet rather than the bound — bounded in practice today only by
`MAX_ATTEMPTS`/`MAX_SUBTASKS` (`crew_runner.rs:28–29`), which are structural caps,
not caveat-derived. Wiring `max_calls` through the dispatch path is the natural
recursion fuel-gauge and a prerequisite for any caveat-based termination/fairness
claim. We make **no** termination claim here.

**Mesh confidentiality vs. authority.** The cert chain proves *authority*
(who may do what, attenuating at each hop) and is verifiable by any peer. It is
*not* a confidentiality mechanism: caveats and metadata travel in the clear
within the chain for verifiability. An axis member (e.g. a private hostname in
`net`, a path in `fs_read`) is therefore visible to any chain verifier.
Authority-minimization and information-minimization are different goals; we
address only the former. This is distinct from — and compounds — the
sub-worktree `fs_read` confidentiality gap of §5.4.

**Enforcement coverage is audited, not proven.** §5.3's matrices are the product
of manual audit of the two engines' consumers. We do not claim exhaustiveness
over the whole runtime; there may be further consumption sites. The honest
status is: Engine A's floor is *near-total for its surface*; Engine B's floor is
*known non-total* (the meet is skipped; three axes unchecked at the member),
with a *small audited effect surface* that bounds the present harm.

**Prefix vs. exact semantics.** The lattice uses exact-member semantics by
design (§2.3); real enforcement (e.g. Landlock) interprets a path as a prefix.
The *soundness* of the bridge between exact-lattice membership and prefix-
enforcement is assumed, not proved, and is a known seam where an enforcement
layer could be *more* permissive than the algebra intends. **Conjecture:** a
monotone, order-preserving denotation from exact members to prefix-closed sets
preserves $\sqsubseteq$ and hence the bound; we have not proved it.

**No formal proof of the algebra.** Theorems 1–2 are hand proofs; §4 is property
testing. A mechanized proof (Lean/Coq) and a refinement-typed or session-typed
enforcement floor that makes complete mediation a *compile-time obligation* are
the two highest-value next steps.

---

## 9. Conclusion

We presented *Caveats*, a bounded meet-semilattice authority algebra for LLM
agents over six independent axes, with attenuation-only delegation realized as
the meet, a withheld-join structure that yields a structural confused-deputy
bound, and a property-tested suite of lattice laws (proof-sketch-plus-
corroboration, not machine-checked). The algebra is sound (Theorems 1–2; §4).
Our central, and deliberately uncomfortable, result is that *this is not enough*:
a sound capability algebra secures a system only when authority is checked before
every effect — complete mediation, 1975, specialized to capability axes. We
audited a deployed runtime and found two engines: a single-agent coder loop that
gates all four effect-bearing axes (near-total), and a crew dispatch engine that
omits the attenuating meet and consults only `fs_write` at the member level (not
total). The honest finding is a *sub-worktree read-scope confidentiality gap* and
a *latent escalation vector*, not the live privilege escalation an algebra-only
reading suggested — and the most useful result is that our own first audit made
exactly that overclaim, reasoning from unchecked axes to reachable effects
without auditing the effect surface. The lattice is a fine telescope; security is
whether the running system looks through it *and* what the running system can
actually reach. Build the algebra — then audit, type, and test the floor for
complete mediation, effect surface by effect surface, because the algebra's
soundness will quietly promise a guarantee the floor does not keep.

---

## References

- Anderson, J. P. (1972). *Computer Security Technology Planning Study.*
  ESD-TR-73-51, U.S. Air Force Electronic Systems Division. (Reference-monitor
  concept.)
- Birgisson, A., Politz, J. G., Erlingsson, Ú., Taly, A., Vrable, M., Lentczner,
  M. (2014). *Macaroons: Cookies with Contextual Caveats for Decentralized
  Authorization in the Cloud.* NDSS.
- Bomberger, A. C., Frantz, W. S., Hardy, A. C., Hardy, N., Landau, C. R.,
  Shapiro, J. S. (1992). *The KeyKOS Nanokernel Architecture.* USENIX Workshop
  on Micro-kernels and Other Kernel Architectures.
- Carlini, N., et al. (2024). *Jailbreaking Black-Box Large Language Models.*
  arXiv.
- Debenedetti, E., Zhang, J., Balunović, M., Beurer-Kellner, L., Fischer, M.,
  Tramèr, F. (2024). *AgentDojo: A Dynamic Environment to Evaluate Attacks and
  Defenses for LLM Agents.* NeurIPS Datasets and Benchmarks.
- Denning, D. E. (1976). *A Lattice Model of Secure Information Flow.*
  Communications of the ACM, 19(5).
- Dennis, J. B., Van Horn, E. C. (1966). *Programming Semantics for
  Multiprogrammed Computations.* Communications of the ACM, 9(3).
- Efstathopoulos, P., Krohn, M., VanDeBogart, S., Frey, C., Ziegler, D., Kohler,
  E., Mazières, D., Kaashoek, F., Morris, R. (2005). *Labels and Event Processes
  in the Asbestos Operating System.* SOSP.
- Ferraiolo, D. F., Kuhn, D. R. (1992). *Role-Based Access Control.* National
  Computer Security Conference.
- Greshake, K., Abdelnabi, S., Mishra, S., Endres, C., Holz, T., Fritz, M.
  (2023). *Not What You've Signed Up For: Compromising Real-World LLM-Integrated
  Applications with Indirect Prompt Injection.* ACM AISec.
- Hardy, N. (1988). *The Confused Deputy (or Why Capabilities Might Have Been
  Invented).* ACM Operating Systems Review, 22(4).
- Klein, G., et al. (2009). *seL4: Formal Verification of an OS Kernel.* SOSP.
- Krohn, M., Yip, A., Brodsky, M., Cliffer, N., Kaashoek, M. F., Kohler, E.,
  Morris, R. (2007). *Information Flow Control for Standard OS Abstractions.*
  SOSP.
- Miller, M. S. (2006). *Robust Composition: Towards a Unified Approach to Access
  Control and Concurrency Control.* PhD thesis, Johns Hopkins University.
- Miller, M. S., Samuel, M., Laurie, B., Awad, I., Stay, M. (2008). *Caja: Safe
  Active Content in Sanitized JavaScript.* Google Technical Report.
- Myers, A. C., Liskov, B. (1999). *JFlow: Practical Mostly-Static Information
  Flow Control.* POPL.
- Saltzer, J. H., Schroeder, M. D. (1975). *The Protection of Information in
  Computer Systems.* Proceedings of the IEEE, 63(9). (Complete mediation; POLA.)
- Schick, T., Dwivedi-Yu, J., Dessì, R., Raileanu, R., Lomeli, M., Zettlemoyer,
  L., Cancedda, N., Scialom, T. (2023). *Toolformer: Language Models Can Teach
  Themselves to Use Tools.* NeurIPS.
- Shapiro, J. S., Smith, J. M., Farber, D. J. (1999). *EROS: A Fast Capability
  System.* SOSP.
- Zeldovich, N., Boyd-Wickizer, S., Kohler, E., Mazières, D. (2006). *Making
  Information Flow Explicit in HiStar.* OSDI.
- Zou, A., Wang, Z., Carlini, N., et al. (2023). *Universal and Transferable
  Adversarial Attacks on Aligned Language Models.* arXiv:2307.15043.
- Hartsock, S. (2026). *The Age of the Confused Deputy — Object-Capability
  Security for LLM Agent Harnesses.* Gilamonster Foundation position paper
  (companion to this work).

---

### Code provenance

All algebra citations are to `agent-mesh/agent-mesh-protocol/src/caveats.rs`
(canonical, re-exported by `agent-bridle-core`) and `.../src/agent_key.rs`
(cert-chain attenuation). Enforcement-floor citations are to the `newt-agent`
runtime, across **two engines**: the single-agent coder
(`newt-coder/src/coder.rs:270, 394, 471, 488`) and the crew/team scheduler
(`newt-scheduler/src/crew.rs:243–412` incl. member `fs_write` at `:348`, team at
`:169`; single-shot dispatch `newt-scheduler/src/dispatch.rs:46–66`;
`newt-cli/src/crew_runner.rs` step-up `:246–256`, `fs_write` `:259`, verify exec
`:276`, no-meet roster calls `:324, :329`, candid comment `:233–234`, structural
caps `:28–29`; worktree confinement `newt-cli/src/crew.rs:42–47, 187–219`), and
the plan path that re-enters the crew engine (`newt-core/src/plan.rs:266–274`
meet; `newt-core/src/agentic/plan_exec.rs:119` exec-only, `:123` dispatch
"crew"), with role/preset clamp at `newt-core/src/role_profile.rs:241–247` and
the contradicted docstring at `newt-core/src/agentic/crew_tool.rs:13–16`. We
verified that `newt-scheduler` does not depend on `newt-coder`, so the two
engines are architecturally disjoint. Line numbers are as of the audited revision
and should be re-pinned before camera-ready.

[^venues]: **Candidate venues.** Reframed as an experience report, this targets
workshops, not the top-tier systems-security framing the first draft aspired to.
(1) *PLAS* (Programming Languages and Analysis for Security, an ACM SIGPLAN
workshop) — the lattice algebra, mechanized laws, and the complete-mediation-as-
type-obligation framing fit a PL-security audience and a workshop's appetite for
a sharp, honest, partly-negative result. (2) *SafeGenAI / Agentic-AI security
workshops at NeurIPS/ICML* — for reaching the LLM-agent community with the "sound
algebra, leaky floor, audit the effect surface" warning while the area is still
forming. A top-tier systems-security submission (S&P/USENIX) would require a
demonstrated exploit and a built type-level floor, neither of which we claim.
