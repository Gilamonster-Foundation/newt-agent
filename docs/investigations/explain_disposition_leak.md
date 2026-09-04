# Investigation — the Explain disposition reads as a cage (#2051)

Status: **investigation only.** No source change is proposed for landing here;
a slash-command-registry workstream lands first. Every line/file reference is
against `dc49a077` (`main`, clean at investigation start).

Observed: on `llama3.1:8b` (ollama, local), a bare `hello?` produced an answer
plus *"(Also, please note that this is an "explain" turn, so I won't be making
any changes to the workspace.)"*

---

## 1. How `PromptDisposition` reaches the model — the actual call path

There are **three** distinct model-visible channels, not one. The issue names
only the first.

### Channel A — the protected active-prompt system card (always present)

```
PromptIntake::model_card()                      newt-core/src/agentic/prompt_intake.rs:696
  └─ match self.disposition → `instruction`     :700-719   ← the leaked sentence's source
  └─ formats "[NEWT PROMPT COMPREHENSION v1]\n disposition: …\n {instruction}"  :724-735
        (PROMPT_COMPREHENSION_MODEL_CARD_PREFIX, :21)
        └─ request_refinement_model_card(&prompt)        :1015 (appended, unrelated)
        └─ noted_facts / noted_instruction block         :739-763

prompt_read::ensure_active_prompt_card_with_intake(msgs, ctx, intake)
                                                 newt-core/src/agentic/prompt_read.rs:450
  └─ active_prompt_card(ctx)          → "[NEWT ACTIVE PROMPT v1] …"   :535 / :583
  └─ active_prompt_card_with_intake(base, intake)                     :509
      └─ append_prompt_comprehension_model_card(base, &intake.model_card())  :513
         (appends INTO the one existing system card — deliberately not a
          second system message, so compression's protected-head logic still
          sees exactly one card adjacent to the verbatim user turn)
  └─ insert_active_prompt_card(...)                                   :459
      └─ splices [system: card, user: active_text] after the leading
         system messages                                              :495-505
```

Callers of `ensure_active_prompt_card_with_intake` — the four agentic loop
entry points, each doing the same thing:

- `newt-core/src/agentic/mod.rs:2101`
- `newt-core/src/agentic/mod.rs:6082`
- `newt-core/src/agentic/mod.rs:8220`
- `newt-core/src/agentic/mod.rs:9911`

(each with an `else ensure_active_prompt_card(...)` arm at +2 for the
no-intake case, which emits the base card with **no** disposition line.)

The card is composed **once, before the tool-round loop**, from a `Copy` of the
disposition (`prompt_disposition`, `mod.rs:1202`). It is never recomputed
mid-turn.

Exact bytes a `hello?` turn ships (reproduced, §4):

```
[NEWT PROMPT COMPREHENSION v1]
disposition: explain
atomic_ask_count: 1
decision_count: 0
pending_decision_count: 0
locked_decision_count: 0
harness_action: answer without mutation; bounded read/recovery tools only
```

### Channel B — the disposition refusal, returned as a tool result

`disposition_tool_denied_message` — `newt-core/src/agentic/tools/catalog.rs:544-566`,
reached from the dispatcher at `tools.rs:4128` (`disposition != Act && name != "request_user_input"`).
Its Explain arm reads:

> "**This is an Explain turn**: only the bounded read-only evidence and recovery tools are available."

**This is the closest textual match in the codebase to what the model said**,
including the "Explain turn" noun phrase that the card itself never uses. The
issue attributes the leak to the card alone; the refusal string is at least as
likely a source, and the screenshot does contain a rejected tool call
(`request_user_input`, missing `question`). Whether that particular rejection
produced this string is unproven — a schema-validation rejection is a different
path — but any fix that only rewords the card leaves this sentence in place.

### Channel C — the tool-discovery scope note

`execute_tool_search_for_disposition` — `newt-core/src/agentic/tool_search.rs:221-239`,
appended to `tool_search` results in any non-`Act` turn whose query looks like
it wants execution. It interpolates `disposition.as_str()` and ends with:

> "`/mode` changes working style but cannot widen an already accepted turn."

### Adjacent, same vocabulary, also model-visible

- `select_operating_mode` tool description — `newt-core/src/agentic/operating_mode.rs:26-31`
  ("never changes the authority or disposition of the current turn").
- `exit_plan_mode` result and description — `tools.rs:3588`, `scheduled.rs:336`.

### What the model is *not* told

Nothing in any channel says the disposition is a **harness inference from the
operator's words**. Nothing says it is **not to be narrated to the operator**.
Channel A is a bare fact list plus one imperative — and the card *does*
elsewhere instruct the model on what to say (`noted_instruction:
"acknowledge these in your reply …"`, `prompt_intake.rs:757-762`), so the card
is already a place where speech instructions live. Its silence about the
disposition line is therefore a gap, not a policy.

---

## 2. Can a model change disposition mid-turn?

**No. Plainly: there is no seam, at any layer.**

- The only mutator is `PromptIntake::enforce_read_only`
  (`prompt_intake.rs:388`). It is attenuate-only by construction — it
  `debug_assert!`s and returns unless the argument is `Explain | Research |
  Plan`, and it refuses to move an `Ask`.
- Its production callers are all **pre-turn**, from the TUI's operating-mode
  application: `newt-tui/src/lib.rs:1792` / `:1795`, via
  `apply_operating_mode_to_intake`, whose doc comment states the rule ("A mode
  can preserve or narrow prompt intake, never widen it").
- The loop takes `prompt_disposition: PromptDisposition` (a `Copy` scalar,
  `mod.rs:1202`) and reads it in three places per loop —
  `filter_tools_for_disposition` (`mod.rs:2171`, `:6145`, `:8279`, `:9935`),
  `tool_round_limit`, and dispatch (`tools.rs:4128`). No `&mut` reaches it.
- The one model-callable mode tool, `select_operating_mode`, is explicit that
  it applies to a *future* turn (`operating_mode.rs:9-16`, and the tool
  description itself). `exit_plan_mode` removes only a model-entered
  self-clamp; it cannot lift a human `/mode plan`.

So the issue's claim (2) is **confirmed, and stronger than stated**: the model
has no move, and the harness tells it so twice (Channel C, and the
`select_operating_mode` description) without offering an alternative.

**What would have to exist**, minimally, for a model-requested widening:

1. A model-callable tool (`request_disposition` / a widened
   `request_user_input`) carrying a justification.
2. A **human-root amplification gate**. Widening `Explain → Act` is
   amplification; per the Authority Register's *attenuate-never-amplify* and
   *amplification-needs-the-human-root*, the model may only *request* — the
   grant must come from the operator. A self-grant is out of the question.
3. A mutable disposition on the in-flight turn state, recomputed into the
   card, the advertised catalog (`filter_tools_for_disposition` is called once,
   before the loop), the round budget (`tool_round_limit`, already consumed),
   and the dispatcher — one effective value, the invariant
   `enforce_read_only`'s doc comment already names.
4. A durable record of the *change*. `artifact_hooks.rs:127-141` validates
   `disposition` as a single scalar across v1/v2/v3; a turn that changed
   disposition is not representable in that schema today. This is the piece
   that makes (2) a **contract** change rather than surface work.

Non-headless-safe caveat: an interactive grant path has to degrade to a
recoverable refusal on the piped/headless/wyvern path, not a hang.

---

## 3. What codex and crush actually do

Both read from local checkouts (not guessed):

- codex — `~/workspaces/Gilamonster-Foundation/codex` @ `312709252d36` (2026-09-02)
- crush — `~/workspaces/Gilamonster-Foundation/crush` @ `e3c970336d7c` (2026-09-02)

### codex: an affordance, and the mode text is pure data

codex composes the model-facing permission text from **`.md` templates on
disk**, selected by policy and interpolated — `codex-rs/prompts/templates/permissions/`:

```
permissions/sandbox_mode/{read_only,workspace_write,danger_full_access}.md
permissions/approval_policy/{never,unless_trusted,on_request,on_request_rule_request_permission}.md
```

composed by `codex-rs/prompts/src/permissions_instructions.rs:19-50`
(`include_str!` + `codex_utils_template::Template`, one template per mode).
This is the language-pack shape CLAUDE.md asks for, applied to authority
vocabulary. `sandbox_mode/read_only.md` in full:

> "Filesystem sandboxing defines which files can be read or written.
> `sandbox_mode` is `read-only`: The sandbox only permits reading files.
> Network access is `{{ network_access }}`."

Note the frame: it describes **the sandbox's behaviour**, not the model's
obligations. It is a fact about the environment, not an instruction the model
is complying with.

The escalation affordance is a **parameter on the tool call**, not a separate
mode-change protocol — `SandboxPermissions { UseDefault, RequireEscalated,
WithAdditionalPermissions }` (`codex-rs/protocol/src/models.rs:56-68`),
enforced in `codex-rs/core/src/tools/sandboxing.rs:262-301`. The model is told
how to use it in `approval_policy/on_request.md`:

> "To request approval to execute a command that will require escalated
> privileges: provide the `sandbox_permissions` parameter with the value
> `"require_escalated"`; include a short question asking the user … in
> `justification` … If you run a command that … fails because of sandboxing …
> rerun the command with `"require_escalated"`. **ALWAYS proceed to use the
> `justification` parameter — do not message the user before requesting
> approval for the command.**"

That last clause is the direct analogue of #2051: codex explicitly tells the
model *not to narrate the permission situation in prose*, and gives it a
structured move instead. And when the policy has no such move, the template
says so and closes the loop — `approval_policy/never.md` in full:

> "Approval policy is currently never. Do not provide the
> `sandbox_permissions` for any reason, commands will be rejected."

Two things newt lacks: the request is **attached to the work** (a parameter on
the command being attempted, so there is nothing to narrate separately), and
the "you have no move" case is stated as a rule about a *tool parameter*, not
as a constraint on the model's behaviour.

### crush: no cage in the prompt at all

crush does **not** put permission or mode state into the system prompt. Its
prompt templates (`internal/agent/templates/{coder.md.tpl,task.md.tpl,…}`)
mention permissions only in passing (`coder.md.tpl:7`, as one of the external
limits that may block a task). Authority is enforced **just-in-time, per tool
call**, by a human dialog: `permission.Service.Request`
(`internal/permission/permission.go:80`, `:181`), called at the point of use in
each tool (`tools/bash.go:244`, `tools/edit.go:153`, `tools/ls.go:121`, …).

A denial returns a three-word tool error and ends the turn
(`internal/agent/tools/tools.go:64-70`):

```go
resp := fantasy.NewTextErrorResponse("User denied permission")
resp.StopTurn = true
```

So crush's answer to "how do you frame the mode to the model" is: *you don't*.
There is nothing standing in context for a small model to notice and report,
and the mode is never a *prediction* about a turn — it is a fact discovered at
the moment of the call. The cost is that crush has no read-only *disposition*
concept to attenuate a catalog with, which is a real capability newt has and
should keep.

**Answering the issue's framing:** neither treats mode as "an external cage the
model narrates". codex makes it an *affordance attached to the attempted
action*; crush makes it *invisible until it bites*. Both remove the standing
sentence that #2051's model read aloud.

---

## 4. Claim (3): does `hello?` reach Explain only via the `?` fallback?

**Confirmed** — and it exposes something worse that the issue does not mention.

Proof by running the real classifier (scratch crate depending on `newt-core`
by path; no repo file was added or modified):

| prompt | disposition |
|---|---|
| `hello?` | **Explain** |
| `hello` | **Act** |
| `hi there?` | Explain |
| `good morning?` | Explain |
| `thanks?` | Explain |
| `explain ownership` | Explain |

Reading `infer_disposition_with` (`prompt_intake.rs:1252-1279`) confirms the
path: `hello?` matches no `action`, `research`, or `explain` needle
(`:1080-1163`), so it falls to `lower.trim_end().ends_with('?')` → `:1272`
returns `lexicon.question_mark_disposition`, which defaults to `Explain`
(`:1164`). The informational-asks arm at `:1274` never runs. That is exactly
the issue's claim.

### The thing that contradicts the issue's emphasis

The issue treats the `?` fallback as the defect. **It is the only thing that
saved this turn.** Bare `hello` matches nothing at all and reaches the terminal
fallback at `:1278` — `PromptDisposition::Act`, "ordinary execution authority
is available", the full tool catalog, and the Act round budget, for a greeting.

The `#1971` doc comment at `:1226-1250` argues at length that `Act`-on-silence
is correct because all 22 measured ordinary imperatives reach `Act` only that
way. That argument holds for imperatives. It does not hold for social openers,
which are not imperatives and are not statements either — the `:1274`
statement-narrowing arm does not catch them, because a greeting has no stative
marker. **Greetings are a hole in the lexicon, in both directions.** Removing
or retuning the `?` fallback without adding greeting data would move `hello?`
from a read-only turn to a full-authority one.

### Second finding: the `?` fallback is already configuration

`question_mark_disposition` is an operator-settable key
(`newt-core/src/config/shell.rs:149-160`, tests at `:490-535`), accepting
`explain` / `research` / `act`, alongside full replacement of the `action`,
`research`, and `explain` needle lists via `IntakeConfig::to_lexicon`. The
issue's "revisit the `?` fallback" is therefore a **data** change, already
supported — it needs no code. What is *not* data is the greeting vocabulary
(there is no `social` list) and, critically, the disposition **card text**.

---

## 5. Candidate fixes, ranked

Terminology per CLAUDE.md: **contract** = survives the wyvern rewrite (wire and
schema types, tool names and schemas, observable behaviour, security
invariants); **surface** = a descendant rewrites it.

### #1 — Give the disposition vocabulary one owner, as pure data, and make it state provenance and non-narration

**What.** Today the same five-way vocabulary is hand-written in at least four
model-visible places: `model_card` (`prompt_intake.rs:700-719`),
`disposition_tool_denied_message` (`catalog.rs:544-566`), the `tool_search`
scope note (`tool_search.rs:231-238`), and the `select_operating_mode` /
`exit_plan_mode` descriptions (`operating_mode.rs:26-31`, `scheduled.rs:336`).
This is the sprawl shape CLAUDE.md's reuse discipline names, and it is why the
issue's "reword the card" instinct is insufficient — Channel B carries the
literal phrase "This is an Explain turn."

Widen the abstraction that already exists rather than adding one:
`DispositionLexicon` (`prompt_intake.rs:~1080`) is already pure data,
config-overridable through `IntakeConfig::to_lexicon`, and merge-by-name. Add
the *output* half — one droppable table keyed by disposition, holding the card
line, the refusal guidance, and the discovery note — so all four sites read
from one place. Then add two fields the model currently has to infer:

- **provenance**: this classification is the harness's inference from your
  words, not an instruction the operator gave;
- **non-narration**: it is harness plumbing and is not to be reported to the
  operator.

Both must be phrased for a 9b model — short, imperative, concrete, in the same
register as the existing `noted_instruction:` line, which is the in-repo proof
that a directive-shaped card line is read and obeyed at this tier. codex's
`on_request.md` ("do not message the user before requesting approval") is the
external precedent.

**Tradeoffs.** Cheapest thing that addresses the *observed* defect, and the only
proposal that fixes all three channels at once. It does not give the model a
move, so a misclassification is still terminal — it just stops being narrated,
which is arguably worse for diagnosis (the operator loses the signal that the
harness got it wrong). Prompt text alone is never a guarantee; a small model may
still narrate.

**Contract vs surface.** The *seam* — one owner, config-driven, droppable — is
durable and is what a rewrite inherits. The exact wording is surface.

**Tests that would prove it.**
- Unit, fully mocked: one golden card per `PromptDisposition` variant asserting
  the provenance and non-narration clauses; the existing assertions at
  `prompt_intake.rs:1746`, `:2194`, `:2414` are the pattern.
- A single-owner conformance test (the ratchet shape CLAUDE.md prescribes):
  the disposition vocabulary appears in exactly one module; count may only go
  down. This is what stops a fifth copy appearing.
- Config round-trip for the new table, mirroring
  `intake_config_overrides_round_trip_and_resolve` (`config/shell.rs:~500`).
- **9b-tier acceptance (mandatory, per the issue).** A BAT replaying a
  recorded `llama3.1:8b` transcript through a `wiremock` backend, asserting the
  reply contains no disposition narration. Recorded, mocked, per-PR — with a
  weekly/release real-ollama run as the ground-truth tier that proves the
  recording still matches reality. A frontier-only check does not discharge
  this.

### #2 — Give the model a move: a request-to-widen affordance

**What.** A model-callable request that a non-`Act` turn be widened, carrying a
justification, routed to the operator for grant — never self-granted. Reuse
discipline points at the existing `request_user_input` seam (already the
sanctioned escalation, already allowed in every non-`Act` disposition,
`catalog.rs:509` and `tools.rs:4128`) rather than a parallel tool. Follow
codex's shape: attach the request to the attempted work so there is nothing to
narrate separately.

**Tradeoffs.** The structurally right answer, and the one that makes the
classifier's mistakes cheap — which matters precisely because §4 shows the
classifier is wrong on a whole category. But it is the largest change, it
crosses the authority boundary (§2 items 2-4: a human-root grant, one effective
disposition across card/catalog/budget/dispatcher, and an artifact schema that
can represent a change rather than a scalar), and a widening path is exactly
where a security regression would land. It also does not, by itself, stop the
narration — a 9b model handed a new affordance may narrate that too. **Do not
do this without #1.**

**Contract vs surface.** Contract, unambiguously: a tool name and schema, an
observable authority behaviour, a security invariant, and an artifact metadata
version bump. This is the piece worth the most care under
"contracts survive a rewrite".

**Tests that would prove it.**
- Dispatch security: the request alone never widens the in-flight turn; a
  denied or unanswered request leaves every gate exactly where it was.
- `enforce_read_only`'s invariant holds under the new path — no route reaches
  `Act` from `Ask`.
- Headless: no interactive gate ⇒ recoverable message, never a hang (the
  existing `request_user_input` no-human behaviour, `catalog.rs:~509`).
- Artifact metadata v4 round-trips a disposition change and rejects a v3
  reader's assumptions (`artifact_hooks.rs:127-141`).
- Catalog/advertisement parity across all four loop entry points.

### #3 — Close the greeting hole in the lexicon (data only)

**What.** Add a `social` needle list to `DispositionLexicon` (same shape as the
`#1260` research additions) so greetings and thanks classify by **content**,
reaching `Explain` whether or not they end in `?` — which fixes `hello` → `Act`
(§4) at the same time. Operators already control
`question_mark_disposition` and the three needle lists, so this ships as data
plus one new droppable list.

**Tradeoffs.** Narrowest and safest; pure data, matching the three Cs exactly.
It fixes the *example* in the issue, not the class — the next
misclassification narrates just the same. And it must land **with** #1, never
instead of it: retuning the `?` fallback on its own is the change that could
hand a greeting `Act` authority.

**Contract vs surface.** Mostly surface (needle data), but the *vocabulary* —
that a `social` category exists and what it maps to — is config surface a
rewrite would inherit, so keep it in the same droppable-table shape as the
rest.

### Recommended order

**#1, then #3 in the same body of work, then #2 as its own authorized step.**
#1 stops the observed leak at all three channels and creates the single owner
that #2 and #3 both write into; #3 is a few lines of data once that owner
exists; #2 is a contract change that deserves its own PR and its own review.

---

## 6. What I found that is not in the issue

1. **Three model-visible channels, not one.** The issue traces only the card.
   The refusal string at `catalog.rs:554` contains the literal phrase "This is
   an Explain turn" — a closer match to what the model said than the card's own
   wording — and `tool_search.rs:231` adds a third. A card-only fix leaves the
   sentence in the codebase.
2. **`hello` (no `?`) classifies as `Act`.** The `?` fallback the issue wants
   revisited is the only reason that turn was read-only. Removing it without
   greeting data makes things worse, not better.
3. **The `?` fallback is already operator configuration**
   (`question_mark_disposition`, `config/shell.rs:149`), as are all three
   needle lists. That part of the issue needs data, not code.
4. **The card already gives the model speech instructions** —
   `noted_instruction: "acknowledge these in your reply …"`
   (`prompt_intake.rs:757`). So a non-narration clause is consistent with the
   card's existing design, and there is in-repo evidence that a directive line
   there is read.
5. **The disposition vocabulary is duplicated across four-plus sites** with no
   owner — the exact sprawl pattern CLAUDE.md's reuse discipline was written
   against (#1312). That, rather than any single string's wording, is what
   makes this bug expensive to fix in one place.
6. **codex's permission text is `include_str!`'d `.md` templates selected by
   policy** — the language-pack model, applied to authority vocabulary. It is a
   working external instance of the three Cs for exactly this problem, worth
   copying in shape.
7. **The harness tells the model twice that it has no move**
   (`tool_search.rs:236`, `operating_mode.rs:29`) while offering no
   alternative. Under that framing, narrating compliance is close to the only
   response left to a small model.
8. Not investigated, per the issue's own scope note: the rejected
   `request_user_input` call (missing `question`) visible in the same
   screenshot. Flagging only that it is a plausible trigger for Channel B and
   so may be less separable from this issue than the issue assumes.
