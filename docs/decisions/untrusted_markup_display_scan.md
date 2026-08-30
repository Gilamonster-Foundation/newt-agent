# Untrusted markup reaches a human: neutralise at the surface

**#1941. Found by G1 ([`g1_external_proof.md`](g1_external_proof.md)), whose
external consumer surfaced it as finding 2.**

## The defect

ADR law 11: **a definition may come from untrusted markup.** So a definition's
`markdown`, a control's `label`, an option's `label`, and a note are all
attacker-chosen text — and every surface rendered them verbatim.

`U+202E` RIGHT-TO-LEFT OVERRIDE in a permission prompt renders `allow` and
`deny` in visually swapped order. The operator reads one thing and authorises
the other. Nothing else is wrong: the definition is well-formed, its
`ContentId` is honest, `validate_response` accepts the answer. **Only what the
human saw was wrong**, which is why no invariant this epic added caught it.

Verified empirically on every surface, not inferred. `ammonia` was the
instructive one — it allowlists tags and attributes and does not touch the
characters inside a text node, so with the fix reverted the page renders
`<p>allow \u{202e}yned once</p>` and a browser dutifully reorders it.

A raw `ESC` in the same slot was found by the same reproduction and is the
same class: it repaints the terminal rather than reordering it.

## The decision: neutralise at the surface

Three candidates. They are not equivalent, and the two rejected lose for
reasons that are specific rather than aesthetic.

### 1. Reject at ingest — rejected

Cleanest on paper and fail-closed. It loses on three counts.

**There is no single ingest point reachable from here.** The constructor is
`InteractionDefinition::new` in `newt-interaction`, which is the inward
protocol layer: its `tests/guard.rs` holds the runtime closure at exactly
three crates and forbids any `newt-*` dependency. A scan there would mean a
*second* scanner, which is the one thing the reuse discipline forbids in this
slice. The remaining candidates (`question_to_definition`, the `interaction_form`
helpers, `agentic/tools.rs`, the `permissions` builder) are many, and "ingest"
spread over many sites is not a chokepoint.

**Rejection converts a display defect into a denial of the ceremony.** A
definition's markdown is machine-generated from an arbitrary command line. If
an embedded character means the prompt never appears, an attacker suppresses
the prompt by embedding one — and an operator who sees *nothing* is not safer
than one who sees something honest.

**The corpus is not a note's corpus.** `scan_note` rejects the write outright
and deliberately accepts collateral: its own module doc lists `\r` line
endings and ZWJ emoji among the things that fail. That is right for a short,
human-curated fact injected into a system prompt. A transcript line is
arbitrary program output, where `\r` is routine. The same policy applied here
would refuse to display ordinary tool output.

### 2. Neutralise at the surface — **chosen**

Each hazard is replaced by a visible marker, `<U+202E>`, at the point text
becomes display.

- The record is untouched, so the `ContentId` minted over it is unchanged and
  **nothing moves on the wire**.
- The hazard becomes *visible* rather than removed, which is information the
  operator needs: "this prompt contains a hidden character" is a fact worth
  showing. Dropping the character silently would hide that anything was there
  — the failure being fixed, not a fix for it.
- Legitimate RTL keeps working, per the distinction below.

**Its stated cost was measured and does not exist.** The concern was that
changing what every surface prints moves A0's frozen goldens and needs the C0c
treatment. It does not: the goldens are host-authored clean text, so the
neutraliser borrows and returns them unchanged. The full `newt-core` suite —
2,937 unit tests plus the integration binaries — passes with the scan wired at
every surface, and **no golden moved**.

### 3. Annotate — rejected

A warning *beside* a spoofed prompt is still a spoofed prompt: the operator's
eye is on the prompt text, which still lies. It is also strictly more work
than neutralising — every surface would have to render the annotation — for a
strictly weaker guarantee. It leaves the operator to do the character-level
forensics the machine had already done.

## The substance: an override is not a mark

A scan that rejected all bidi would not be fail-closed; it would be **broken
for Arabic and Hebrew**. The line is drawn by what a control can actually do.

| Codepoints | What they are | Policy |
|---|---|---|
| `U+202A`–`U+202E` (LRE, RLE, PDF, LRO, RLO) | **Embeddings and overrides.** They *force* direction on characters against their inherent class. This is the primitive that renders `deny` where `allow` should be. Unicode deprecates them in favour of the isolates. | **neutralised** |
| `U+200E`, `U+200F` (LRM, RLM) | **Marks.** They resolve the direction of neighbouring *neutrals* only; they cannot flip a strong character. Real RTL UI strings carry them. | permitted |
| `U+2066`–`U+2069` (LRI, RLI, FSI, PDI) | **Isolates.** They *bound* a run so it cannot affect its surroundings — the construct Unicode recommends in place of the embeddings above. | permitted |
| C0/C1 controls except `\n`, `\t` | `ESC` repaints the terminal; `\r` rewrites the line being read. | **neutralised** |
| zero-width set, tag characters | Let what the operator sees be a strict subset of what is there. | **neutralised** |

Arabic and Hebrew need no controls at all — the bidi algorithm orders strong
characters from their own class — so the permitted rows are a courtesy to
real-world strings rather than a necessity, and the neutralised rows are the
ones with no honest use in a prompt.

**Residual, stated rather than hidden:** an unbalanced isolate can still change
the *order of mixed-direction runs*. That is correct bidi behaviour for mixed
content rather than a forcing primitive, and neutralising it would break the
case it exists for. Separately, a benign literal `<U+202E>` typed as text is
indistinguishable from a neutralised one; that ambiguity errs toward showing a
warning for harmless text, which is the safe direction.

## One table, two policies — not a second scanner

`notes_scan::invisible_char_name` is unchanged and remains the note policy.
`display_hazard_name` is that same table with exactly two arms subtracted, and
`the_note_policy_still_rejects_what_the_display_policy_permits` pins that the
two differ by those arms and nothing else. Widening the table widens both.

This was the third time in the epic that a correct implementation was found not
to be reachable from the path that needed it — after C2b's `RawGuard` and
#1950's cursor-position rescue.

## Where the rule lives

Not at N call sites. The first attempt at this fix neutralised
`spans::project`'s *input*, which covered the markdown body and **silently
missed the option labels that carry `allow` and `deny`** — `interaction_view`
builds those into spans directly. So the rule sits in the `Span` constructors,
where every rich row's text is made, plus the three producers that do not go
through them:

| Surface | Where |
|---|---|
| plain projection (body, note, every control line) | `markup::plain::render`, on the way out |
| span projection (parser path) | `markup::spans::project`, on its input |
| rich rows (option labels, notes, fields) | `markup::spans::Span::plain` / `::styled` |
| transcript | `agentic::transcript_lines_styled`, before wrapping |
| web page | `newt_web::shell::render_markdown`, before parsing |

Neutralisation is idempotent — the marker contains no hazard — so the body
passing through twice is harmless.

`newt-core/tests/untrusted_display_scan.rs` is the inventory as a table: eight
render paths, each checked in both directions, plus
`shell::tests::c3a_bidi` for the web. A new display producer belongs in that
table. Reverting the `Span` constructors makes it fail naming exactly the path
the first attempt missed:

```
`rich spans / option label` rendered a bidi override verbatim — a permission
prompt on this surface can show `allow` and `deny` swapped
```
