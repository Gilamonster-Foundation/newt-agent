# G1: the external proof

**Slice G1 of epic #1803 (#1934). Depends on G0
([`g0_extraction_readiness.md`](g0_extraction_readiness.md)).**

The epic gates publication on two real Newt views **and an external proof**.
This is the external proof, and its centrepiece is not a test suite: it is a
consumer that is not Newt.

## What was built

[`newt-interaction/conformance/newt_conformance.py`](../../newt-interaction/conformance/newt_conformance.py)
— a stdlib-only Python consumer, about 400 lines, carrying its own BLAKE3,
its own canonical DAG-CBOR encoder, and its own CIDv1 renderer, all written
from the specifications. It links against nothing, imports no third-party
package, and has never seen this crate's source.

It does four things, and the fourth is the one that matters:

1. **Re-derives every golden vector independently** — JSON to canonical
   bytes to digest to `ContentId`, comparing at each step so a mismatch says
   *which* step. All six vectors reproduce.
2. **Renders a definition it was not written against**, choosing its own
   presentation from the record's semantics alone.
3. **Refuses fail-closed** when a definition demands a surface feature it
   does not have — Law 11, applied by the consumer to the author.
4. **Authors typed responses**, which
   [`external_consumer.rs`](../../newt-interaction/tests/external_consumer.rs)
   decodes through the checked door, re-encodes byte-identically, mints to
   the id Python predicted, and puts through `binding::validate_response`.
   They are accepted.

**The protocol does not require a Newt application crate.** The hard
constraint this slice was told to test held: a consumer with no Rust, no
terminal crate, no web crate, and no Newt dependency of any kind can read an
offer and answer it in a form the protocol accepts.

### Why it vendors a hash rather than installing one

`pip install blake3`, and `content-addressable`'s own Python package, are
both bindings over the **same Rust core** Newt uses — an id computed through
either agrees with Newt's because it *is* Newt's. Such a consumer cannot
falsify anything. The implementations here are pinned against BLAKE3's own
published test vectors, which is what makes agreement with Newt evidence
rather than tautology.

## What the external consumer found

Writing a consumer against the published contract, rather than against
Newt's assumptions, is what produced both of these. Neither was visible from
inside.

### Finding 1 — three of the four published response vectors were not valid

`interaction-vectors.json`'s positive corpus is documented as records a
foreign implementation may reproduce *and submit*: the invalid corpus is a
separate file precisely so the positive one contains nothing "that A3 will
reject, labelled only by prose someone has to read". That claim lived in a
doc comment and nowhere else.

Three of the four response vectors answered a single **optional** control
and left the required `decision` unanswered. `validate_response` rule 8
refuses that (`MissingRequiredControl`). A conforming implementation that
reproduced `response/value-text` and submitted it would have been refused,
with the corpus telling it nothing.

**Fixed here**, because the doc comment argues for the property rather than
merely describing it: each positive response now answers the required
control as well as its kind-specific one, and the invariant is a test —
`every_positive_response_vector_is_accepted_against_the_offer`, with
`every_invalid_vector_is_refused_by_that_same_path` as its anti-vacuous
twin. Four of six vector ids moved; the compatibility policy names the
re-baseline.

### Finding 2 — the untrusted-markup path has no ingest scan (OPEN)

Law 11 says a definition may come from untrusted markup. A definition's
`markdown` and a control's `label` are therefore attacker-chosen strings,
and `markup::plain::render` passes them through verbatim — **including bidi
override controls**. `U+202E` in a permission prompt can render `deny` and
`allow` in visually swapped order, which is a spoof of exactly the decision
the prompt exists to take.

The machinery already exists and is not wired to this path:
`newt_core::notes_scan` rejects `U+202E`, `U+200E/F`, the isolate controls,
tag characters, and the zero-width set — with the exact diagnostic
(`"bidirectional embedding/override control"`) — but it is applied to
**notes**, not to interaction definitions.

**Not fixed here.** The fix has a real design question in it — reject at
ingest, escape at render, or annotate — and changing what a renderer emits
for untrusted text is a slice, not a footnote. It is reported rather than
worked around, per this slice's own constraint. The reuse discipline already
names the answer's shape: widen `notes_scan`, do not stand a second scanner
up beside it.

## Scope 3: what was already proven, and what was not

The issue's inventory was checked against the checkout rather than trusted,
and it was partly stale: **C2a is #1876 (the interaction view model and
`markup::spans`) and C2b is #1891 (the one raw-mode guard)** — neither is a
monochrome or narrow-width PTY test. The properties are nonetheless covered,
by other means and in some places more strongly.

| Property | Where it is proven | Status |
|---|---|---|
| Keyboard / focus | `newt-web` `controls_carry_accessible_semantics` (`:focus-visible`) | proven |
| Screen-reader order and semantics | same test: `<fieldset>`/`<legend>`, every `<label for=…>` resolving to a real `id`, `aria-live` | proven |
| Works with no JS | `a_losing_no_js_answer_is_not_redirected_as_success`, `a_winning_no_js_answer_is_redirected_but_no_other_outcome_is` | proven |
| Reduced motion | same test: the page carries `prefers-reduced-motion` | proven |
| Contrast without hardcoded colour | E0b (#1869): the shell styles from `currentColor` / `color-mix`, so contrast is the page's own | proven |
| Monochrome — the decision | `color_enabled_for_applies_mode_against_tty`, `no_color_beats_persisted_config_but_not_the_flag`, and the `ColorMode` keyword tests | proven |
| Monochrome — the rendering | **new here**: `the_plain_projection_never_emits_an_escape` — for *any* author-supplied markdown and label, the plain projection emits no ESC and no CR | closed by this slice |
| Narrow width | `the_plain_projection_reads_no_ambient_width` — stronger than rendering correctly at a width: the projection cannot *learn* one, so width cannot change it | proven |
| Unicode width | `a_combining_mark_costs_no_column`, `a_cjk_string_measures_in_cells_not_chars_or_bytes` | proven |
| Bidi | `notes_scan`'s control table, on the **notes** path only | **gap — finding 2** |
| Secret redaction | `a_planted_secret_reaches_no_surface_and_no_record` (`newt-tui`), `a_planted_secret_never_appears_in_canonical_bytes_or_stored_record` (`newt-core`) | proven |

Most of scope 3 was indeed already proven, as the issue predicted. The
residual was two rows, and this slice closes one of them and reports the
other.

## Scope 1: fuzzing

Two property suites, both `proptest` — already this workspace's fuzz tool
(#1528 B2), needing no nightly, and running inside the per-PR gate rather
than beside it. A property that only runs weekly is one nobody's PR is
measured against.

- [`newt-core/tests/untrusted_markup_fuzz.rs`](../../newt-core/tests/untrusted_markup_fuzz.rs)
  — front matter, the envelope round trip (#1848's unforgeable marker,
  stated over all inputs), the Markdown parser, the plain projection,
  Mermaid measurement, and table-cell escaping.
- [`newt-interaction/tests/fuzz.rs`](../../newt-interaction/tests/fuzz.rs)
  — arbitrary bytes into the decoder, corruption of real records, author
  text and identity, A3's rule that a responder-supplied string decides
  nothing, and validated scalars.

Every property carries an anti-vacuous twin, because a property guarded by
`if let Ok(…)` over a function that fails for everything is true and
worthless. Failure persistence is off: the unit tier does no filesystem I/O,
and proptest prints the reproducing seed regardless.

Both suites were mutation-proved. Removing the pipe escape in
`markup::table` and injecting an SGR sequence into `markup::plain` made
three of the checks fail — the properties *and* their twins — and reverting
made them pass.

## Readiness

Publication is the operator's call and is **not** decided here. What this
slice can report:

- The external proof **passes**. The protocol is renderer-neutral in the
  falsifiable sense: something that is not Newt can read an offer and answer
  it acceptably.
- The conformance fixtures and their compatibility policy are published, at
  [`newt-interaction/conformance/README.md`](../../newt-interaction/conformance/README.md).
- Finding 2 is **open**, and it is a spoofing gap on the path Law 11 names
  as untrusted. It is the one thing here an operator should weigh before
  publishing.
- G0's conclusion is unchanged. Nothing in this slice created a consumer
  outside `newt-core`'s compile graph, so nothing became extractable; and
  the cross-repository ownership conflict G0 reported is still unresolved.
