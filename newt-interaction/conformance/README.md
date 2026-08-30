# Newt Markup conformance — fixtures, policy, and an external consumer

This directory is the **cross-language contract** for the Newt Markup
interaction protocol, and the proof that the contract is one another
implementation can actually meet.

Everything here is stdlib-only Python and committed JSON. Nothing in it
links against a Newt crate.

## What is published

| File | What it is |
|---|---|
| `../tests/data/interaction-vectors.json` | The golden corpus. Six records — one definition, one instance, four responses — each as JSON, as canonical DAG-CBOR hex, and as a `ContentId`. |
| `../tests/data/interaction-vectors-invalid.json` | Records that encode and address correctly but are **not valid in context**, each carrying `invalid_because`. |
| `../schema/*.json` | JSON Schema for each record, behind the off-by-default `schemars` feature, each `const`-pinning its own version tag. |
| `newt_conformance.py` | An external consumer: its own BLAKE3, its own DAG-CBOR encoder, its own CID renderer, written from the specifications. |
| `responses.json` | Responses **authored by that consumer**, which `../tests/external_consumer.rs` decodes and validates. |

## The compatibility policy

1. **The bytes are the contract; the JSON is the documentation.** Identity
   is minted over the canonical DAG-CBOR, never over the JSON. Where they
   appear to disagree, the bytes win and the JSON is the thing to fix.

2. **A field listed in `links` is a CID link, not a string.** This is the one
   fact JSON cannot express: in the canonical encoding those fields are tag
   42 over a `0x00`-prefixed binary CID. Encoding the published JSON verbatim
   will *not* reproduce the published bytes, and that is not a defect in the
   corpus. Each vector ships its own `links` list so no implementation has to
   guess.

3. **Three steps, checked separately, in order.** Encode the JSON and compare
   `dagcbor_hex`; only then hash and compare `content_id`. An implementation
   that compares only ids learns *that* it is wrong and never *where* —
   whether its map ordering, its integer encoding, its hash, or its multihash
   prefix. `newt_conformance.py verify` reports the step.

4. **Canonical means exactly one encoding per value.** Definite lengths,
   smallest-form integers, map keys sorted by byte length then bytewise, no
   floats. A decoder that accepts a non-canonical encoding lets one record
   carry two identities, so Newt's decoder refuses it (`ProtocolError::NonCanonical`)
   and a conforming implementation must too.

5. **The positive corpus is valid in context, not merely well-formed.** Every
   record in `interaction-vectors.json` is one a conforming consumer may
   reproduce *and submit*. Records that are correctly addressed but invalid
   live in the separate `-invalid.json` file and say why. Both halves are
   enforced, in both directions, by
   `vectors::every_positive_response_vector_is_accepted_against_the_offer`
   and `vectors::every_invalid_vector_is_refused_by_that_same_path`.

6. **Re-baselining is deliberate and announced.** The vectors regenerate only
   under `NEWT_GOLDEN_UPDATE=1`; a missing file fails rather than being
   silently rewritten. A change to the published bytes is a change every
   foreign consumer reads, and the PR that makes one says what moved on the
   wire.

7. **No compatibility is owed yet.** `newt-interaction` is `publish = false`
   and the project is unadvertised. Until it is published, the corpus may be
   re-baselined; the policy is that it is done *visibly*, not that it is
   never done.

## What a conforming implementation must know that the records do not say

These are the facts a foreign consumer needs and cannot read off a
definition. They were each found by writing one — see `newt_conformance.py`.

- **The response schema tag** is `newt.interaction.response/v1`. A definition
  does not name the tag of the record that answers it.
- **Every required control must be answered.** A response that answers only
  the control it cares about is refused (`MissingRequiredControl`). The
  `requirement` field is the only thing that says so, and reading it is not
  optional — see `values_for`.
- **Handlers are the host's, never the definition's.** A consumer can author
  a valid response naming an option, and still cannot know what running it
  will invoke: `ResolvedAction`'s handler comes from the caller's
  registration. This is deliberate — a definition that could name a handler
  would be a definition that could choose one.
- **`audience` must be one the instance's `responder_policy` admits**, and
  when that policy sets `requires_assertion`, an assertion must be present.
  Both are in the instance record.

## Running it

```bash
python3 newt_conformance.py --self-test   # the consumer's own guard
python3 newt_conformance.py verify        # re-derive every vector's bytes and id
python3 newt_conformance.py render        # render the definition as text
python3 newt_conformance.py check         # responses.json is what this consumer produces
cargo test -p newt-interaction --test external_consumer
```

CI runs all five. `just check` runs the Python half.

## Why the consumer vendors a hash instead of installing one

`pip install blake3` and `content-addressable`'s own Python package are both
bindings over the **same Rust core** Newt uses. An id computed through either
agrees with Newt's because it *is* Newt's — such a consumer cannot falsify
anything. The BLAKE3 and DAG-CBOR here are written from the specifications
and pinned against BLAKE3's own published test vectors, which is what makes
agreement with Newt evidence rather than tautology.
