#!/usr/bin/env python3
"""A consumer of Newt Markup that is not Newt (epic #1803, slice G1).

This is the external proof. Every claim this epic made about
`newt-interaction` — that a definition carries semantics and not
presentation, that a response is typed, that identity is content-addressed
— is checkable only by something written against the published contract
rather than against Newt's assumptions. So this file:

1. **Verifies identity independently.** It re-derives each golden vector's
   canonical DAG-CBOR bytes from the vector's JSON, then its BLAKE3 digest,
   then its CIDv1 — with its own encoder and its own hash, in another
   language, from the specifications. Nothing here calls Newt.
2. **Renders a definition it has never seen.** Labels, requirement, control
   kinds, and choice options are read out of the record; the presentation is
   this file's own. If a definition needed presentation baked in, this step
   could not exist.
3. **Refuses fail-closed.** A definition that demands a surface feature this
   consumer does not have is refused before rendering — Law 11: a definition
   may come from untrusted markup, so an author-assigned field never talks
   this consumer into a capability it lacks.
4. **Authors typed responses.** It emits a response record per control kind,
   derived from the definition and instance alone, minted with its own
   ContentId. `newt-interaction/tests/external_consumer.rs` decodes those and
   runs them through `binding::validate_response`. Rust accepting bytes this
   file produced is the falsifiable half.

**Deliberately stdlib-only.** `pip install blake3` would be a second binding
over the same reference implementation, and `content-addressable`'s own
Python package is a PyO3 binding over the *same Rust core* — an id it
computes agrees with Rust because it IS Rust. Neither can falsify anything.
The BLAKE3 and DAG-CBOR here are written from the specs precisely so that
agreement means something.

Usage:
    newt_conformance.py --self-test   # verify this consumer against known answers
    newt_conformance.py verify        # re-derive every golden vector's id
    newt_conformance.py render        # render the definition vector as text
    newt_conformance.py author        # (re)write responses.json
    newt_conformance.py check         # `author` output matches what is committed
"""

from __future__ import annotations

import base64
import json
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
VECTORS = HERE.parent / "tests/data/interaction-vectors.json"
RESPONSES = HERE / "responses.json"

# ---------------------------------------------------------------------------
# BLAKE3, from the specification. Hash mode only: no keyed hashing, no key
# derivation, no extendable output. 32 bytes out, which is all a CID needs.
# ---------------------------------------------------------------------------

BLOCK_LEN = 64
CHUNK_LEN = 1024

CHUNK_START = 1 << 0
CHUNK_END = 1 << 1
PARENT = 1 << 2
ROOT = 1 << 3

IV = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A,
    0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
]

MSG_PERMUTATION = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8]


def _rotr(x: int, n: int) -> int:
    return ((x >> n) | (x << (32 - n))) & 0xFFFFFFFF


def _g(s: list[int], a: int, b: int, c: int, d: int, mx: int, my: int) -> None:
    s[a] = (s[a] + s[b] + mx) & 0xFFFFFFFF
    s[d] = _rotr(s[d] ^ s[a], 16)
    s[c] = (s[c] + s[d]) & 0xFFFFFFFF
    s[b] = _rotr(s[b] ^ s[c], 12)
    s[a] = (s[a] + s[b] + my) & 0xFFFFFFFF
    s[d] = _rotr(s[d] ^ s[a], 8)
    s[c] = (s[c] + s[d]) & 0xFFFFFFFF
    s[b] = _rotr(s[b] ^ s[c], 7)


def _round(s: list[int], m: list[int]) -> None:
    _g(s, 0, 4, 8, 12, m[0], m[1])
    _g(s, 1, 5, 9, 13, m[2], m[3])
    _g(s, 2, 6, 10, 14, m[4], m[5])
    _g(s, 3, 7, 11, 15, m[6], m[7])
    _g(s, 0, 5, 10, 15, m[8], m[9])
    _g(s, 1, 6, 11, 12, m[10], m[11])
    _g(s, 2, 7, 8, 13, m[12], m[13])
    _g(s, 3, 4, 9, 14, m[14], m[15])


def _compress(cv, block, counter, block_len, flags):
    state = cv[:8] + IV[:4] + [
        counter & 0xFFFFFFFF,
        (counter >> 32) & 0xFFFFFFFF,
        block_len,
        flags,
    ]
    m = list(block)
    for i in range(7):
        _round(state, m)
        if i < 6:
            m = [m[MSG_PERMUTATION[j]] for j in range(16)]
    for i in range(8):
        state[i] ^= state[i + 8]
        state[i + 8] ^= cv[i]
    return state


def _words(block: bytes) -> list[int]:
    padded = block + b"\0" * (BLOCK_LEN - len(block))
    return [int.from_bytes(padded[i:i + 4], "little") for i in range(0, BLOCK_LEN, 4)]


class _Output:
    """The last block of a node, not yet compressed.

    It is held rather than compressed because a node does not know until
    the tree is finished whether it is the root, and the ROOT flag changes
    the answer.
    """

    def __init__(self, cv, block, counter, block_len, flags):
        self.cv, self.block = cv, block
        self.counter, self.block_len, self.flags = counter, block_len, flags

    def chaining_value(self) -> list[int]:
        return _compress(self.cv, self.block, self.counter, self.block_len, self.flags)[:8]

    def root_hash(self) -> bytes:
        words = _compress(self.cv, self.block, 0, self.block_len, self.flags | ROOT)
        return b"".join(w.to_bytes(4, "little") for w in words[:8])


def _chunk_output(chunk: bytes, counter: int) -> _Output:
    cv = IV[:]
    blocks = [chunk[i:i + BLOCK_LEN] for i in range(0, len(chunk), BLOCK_LEN)] or [b""]
    for i, block in enumerate(blocks):
        flags = (CHUNK_START if i == 0 else 0)
        if i == len(blocks) - 1:
            return _Output(cv, _words(block), counter, len(block), flags | CHUNK_END)
        cv = _compress(cv, _words(block), counter, BLOCK_LEN, flags)[:8]
    raise AssertionError("unreachable: every chunk has a last block")


def _parent_output(left: list[int], right: list[int]) -> _Output:
    return _Output(IV[:], left + right, 0, BLOCK_LEN, PARENT)


def blake3(data: bytes) -> bytes:
    """32-byte BLAKE3 hash of `data`."""
    n_chunks = max(1, (len(data) + CHUNK_LEN - 1) // CHUNK_LEN)
    stack: list[list[int]] = []
    for i in range(n_chunks):
        out = _chunk_output(data[i * CHUNK_LEN:(i + 1) * CHUNK_LEN], i)
        if i == n_chunks - 1:
            while stack:
                out = _parent_output(stack.pop(), out.chaining_value())
            return out.root_hash()
        cv = out.chaining_value()
        # Merge while the number of completed chunks is even: the standard
        # rule that keeps the stack a binary counter of subtree heights.
        total = i + 1
        while total % 2 == 0:
            cv = _parent_output(stack.pop(), cv).chaining_value()
            total //= 2
        stack.append(cv)
    raise AssertionError("unreachable: the loop returns on the last chunk")


# ---------------------------------------------------------------------------
# Canonical DAG-CBOR, from the specification: definite lengths, smallest
# integer encoding, map keys sorted by byte length then bytewise, and CIDs
# as tag 42 over a 0x00-prefixed byte string.
# ---------------------------------------------------------------------------

TAG_CID = 42


def _head(major: int, value: int) -> bytes:
    if value < 24:
        return bytes([major << 5 | value])
    if value < 1 << 8:
        return bytes([major << 5 | 24, value])
    if value < 1 << 16:
        return bytes([major << 5 | 25]) + value.to_bytes(2, "big")
    if value < 1 << 32:
        return bytes([major << 5 | 26]) + value.to_bytes(4, "big")
    return bytes([major << 5 | 27]) + value.to_bytes(8, "big")


def cid_to_bytes(text: str) -> bytes:
    """The binary CID behind its canonical `b`-prefixed base32 rendering."""
    if not text.startswith("b"):
        raise ValueError(f"not a base32 CIDv1: {text!r}")
    body = text[1:].upper()
    return base64.b32decode(body + "=" * (-len(body) % 8))


def bytes_to_cid(raw: bytes) -> str:
    return "b" + base64.b32encode(raw).decode("ascii").rstrip("=").lower()


def encode(value, links: frozenset[str] = frozenset(), path: str = "") -> bytes:
    """Canonical DAG-CBOR for a JSON value.

    `links` names the top-level fields whose string values are CIDs. This
    is the fact JSON cannot express and a foreign implementation most needs:
    in the canonical bytes those fields are LINKS, not strings, so encoding
    the JSON verbatim will not reproduce them. Each vector ships the list.
    """
    if path in links:
        if not isinstance(value, str):
            raise TypeError(f"link field {path!r} is not a string")
        return _head(6, TAG_CID) + _head(2, len(value := b"\0" + cid_to_bytes(value))) + value
    if isinstance(value, bool):  # before int: bool is an int in Python
        return b"\xf5" if value else b"\xf4"
    if value is None:
        return b"\xf6"
    if isinstance(value, int):
        return _head(0, value) if value >= 0 else _head(1, -1 - value)
    if isinstance(value, str):
        raw = value.encode("utf-8")
        return _head(3, len(raw)) + raw
    if isinstance(value, list):
        return _head(4, len(value)) + b"".join(encode(v, links, "") for v in value)
    if isinstance(value, dict):
        keys = sorted(value, key=lambda k: (len(k.encode("utf-8")), k.encode("utf-8")))
        out = _head(5, len(keys))
        for key in keys:
            raw = key.encode("utf-8")
            child = key if not path else f"{path}.{key}"
            out += _head(3, len(raw)) + raw + encode(value[key], links, child)
        return out
    raise TypeError(f"DAG-CBOR has no encoding for {type(value).__name__}")


def content_id(canonical: bytes) -> str:
    """CIDv1 / dag-cbor / BLAKE3-256 over already-canonical bytes."""
    # 0x01 version, 0x71 dag-cbor, 0x1e blake3, 0x20 digest length.
    return bytes_to_cid(b"\x01\x71\x1e\x20" + blake3(canonical))


# ---------------------------------------------------------------------------
# The consumer proper.
# ---------------------------------------------------------------------------

# What this consumer can actually do. It is a text renderer on a pipe: it
# can take a secret by REFERENCE (it never sees or echoes one), but it has
# no diagrams and no styling. A definition demanding anything absent here
# is refused rather than approximated.
SUPPORTED_FEATURES = frozenset({"secret-input"})

RESPONSE_SCHEMA = "newt.interaction.response/v1"


class Refused(Exception):
    """This consumer will not render the definition. Fail closed."""


def load_vectors() -> list[dict]:
    return json.loads(VECTORS.read_text())


def vector(vectors: list[dict], record: str) -> dict:
    for v in vectors:
        if v["record"] == record:
            return v
    raise LookupError(f"no {record} vector")


def check_features(definition: dict) -> None:
    """Refuse a definition demanding a surface feature we do not have."""
    missing = [
        d["feature"]
        for d in definition.get("features", [])
        if d["requirement"] == "required" and d["feature"] not in SUPPORTED_FEATURES
    ]
    if missing:
        raise Refused("unsupported required feature(s): " + ", ".join(sorted(missing)))


def render(definition: dict) -> str:
    """A definition as text this consumer chose, from semantics it read.

    Nothing about the layout below is in the record. The record says a
    control is `required`, is a `choice`, and that an option's role is
    `deny`; that those become `(required)`, a numbered list, and a `[deny]`
    marker is this consumer's decision alone.
    """
    check_features(definition)
    lines = [definition["markdown"], ""]
    for control in definition.get("controls", []):
        kind = control["kind"]
        lines.append(f"  {control['label']} ({control['requirement']})")
        if isinstance(kind, dict) and "choice" in kind:
            for n, opt in enumerate(kind["choice"]["options"], start=1):
                role = f"  [{opt['role']}]" if opt.get("role") != "neutral" else ""
                lines.append(f"    {n}) {opt['label']}{role}")
        elif kind == "text":
            lines.append("    (free text)")
        elif kind == "toggle":
            lines.append("    (yes / no)")
        elif kind == "secret":
            lines.append("    (secret — supplied by reference, never echoed)")
        else:
            raise Refused(f"unknown control kind: {kind!r}")
    optional = [
        d["feature"]
        for d in definition.get("features", [])
        if d["requirement"] != "required"
    ]
    if optional:
        lines += ["", "  not shown (optional, unsupported): " + ", ".join(optional)]
    return "\n".join(lines) + "\n"


def answer_for(control: dict) -> dict:
    """A typed value answering `control`, chosen from its kind alone.

    A choice answers with the option whose role REFUSES, when there is one.
    An external consumer that cannot ask a human must not manufacture
    consent, and `role` is exactly the field that says which option is
    which without the consumer parsing the label.
    """
    kind = control["kind"]
    if isinstance(kind, dict) and "choice" in kind:
        options = kind["choice"]["options"]
        refusing = [o for o in options if o.get("role") in ("deny", "cancel")]
        chosen = (refusing or options)[0]
        return {"kind": "choice", "option": chosen["id"]}
    if kind == "text":
        return {"kind": "text", "text": "answered by the external consumer"}
    if kind == "toggle":
        return {"kind": "toggle", "on": False}
    if kind == "secret":
        return {"kind": "secret", "reference": "external-consumer-handle-1"}
    raise Refused(f"unknown control kind: {kind!r}")


def values_for(definition: dict, control: dict) -> list[dict]:
    """Every REQUIRED control answered, plus this one.

    Found the hard way, which is the point of an external consumer: a
    response that answers only the control it cares about is refused, because
    a required control with no answer is a refusal (`validate_response` rule
    8). Nothing but the `requirement` field tells a foreign consumer that,
    and reading it is not optional.
    """
    answering = [c for c in definition["controls"] if c["requirement"] == "required"]
    if control["id"] not in [c["id"] for c in answering]:
        answering.append(control)
    return [{"control": c["id"], "value": answer_for(c)} for c in answering]


def author(definition_vec: dict, instance_vec: dict) -> list[dict]:
    """One response per control, built from the two records alone."""
    definition, instance = definition_vec["json"], instance_vec["json"]
    check_features(definition)

    policy = instance["responder_policy"]
    audience = policy["audiences"][0]
    provenance = {
        "kind": "signed-assertion" if policy["requires_assertion"] else "unauthenticated",
        "subject": "operator:external-consumer",
        "audience": audience,
    }
    if policy["requires_assertion"]:
        provenance["assertion"] = "external-assertion-handle"

    out = []
    for control in definition["controls"]:
        record = {
            "schema": RESPONSE_SCHEMA,
            "definition": instance["definition"],
            "instance": instance_vec["content_id"],
            "revision": instance["revision"],
            "values": values_for(definition, control),
            "idempotency_key": f"external-{control['id']}",
            "responder_provenance": provenance,
        }
        links = frozenset({"definition", "instance"})
        canonical = encode(record, links)
        out.append({
            "name": f"external/response-{control['id']}",
            "authored_by": "newt_conformance.py (stdlib-only, no Newt code)",
            "json": record,
            "links": sorted(links),
            "dagcbor_hex": canonical.hex(),
            "content_id": content_id(canonical),
        })
    return out


def render_responses(responses: list[dict]) -> str:
    return json.dumps(responses, indent=2) + "\n"


# ---------------------------------------------------------------------------
# Commands.
# ---------------------------------------------------------------------------

def cmd_verify() -> int:
    """Re-derive every vector's bytes and id from its JSON. Fail-closed."""
    vectors = load_vectors()
    if not vectors:
        print("conformance: no vectors to verify", file=sys.stderr)
        return 1
    failures = 0
    for v in vectors:
        links = frozenset(v["links"])
        for path in links:
            if "." in path:
                print(f"conformance: {v['name']}: nested link path {path!r} is "
                      "not supported by this consumer", file=sys.stderr)
                failures += 1
        try:
            canonical = encode(v["json"], links)
        except (TypeError, ValueError) as exc:
            print(f"conformance: {v['name']}: cannot encode: {exc}", file=sys.stderr)
            failures += 1
            continue
        if canonical.hex() != v["dagcbor_hex"]:
            print(f"conformance: {v['name']}: BYTES differ", file=sys.stderr)
            print(f"  expected {v['dagcbor_hex']}", file=sys.stderr)
            print(f"  got      {canonical.hex()}", file=sys.stderr)
            failures += 1
            continue
        got = content_id(canonical)
        if got != v["content_id"]:
            print(f"conformance: {v['name']}: bytes agree but ID differs — the "
                  f"hash or the CID prefix is wrong\n  expected {v['content_id']}"
                  f"\n  got      {got}", file=sys.stderr)
            failures += 1
    if failures:
        print(f"conformance: {failures} vector failure(s)", file=sys.stderr)
        return 1
    print(f"conformance: {len(vectors)} vectors re-derived independently "
          "(bytes and id)")
    return 0


def cmd_render() -> int:
    print(render(vector(load_vectors(), "definition")["json"]), end="")
    return 0


def cmd_author() -> int:
    vectors = load_vectors()
    RESPONSES.write_text(render_responses(
        author(vector(vectors, "definition"), vector(vectors, "instance"))
    ))
    print(f"conformance: wrote {RESPONSES}")
    return 0


def cmd_check() -> int:
    vectors = load_vectors()
    fresh = render_responses(
        author(vector(vectors, "definition"), vector(vectors, "instance"))
    )
    if not RESPONSES.exists():
        print(f"conformance: {RESPONSES} is missing. Regenerate it deliberately "
              "with `author`, never by letting a check write it.", file=sys.stderr)
        return 1
    if RESPONSES.read_text() != fresh:
        print(f"conformance: {RESPONSES} no longer matches what this consumer "
              "produces. Rerun `author` and say in the PR what moved.",
              file=sys.stderr)
        return 1
    print(f"conformance: committed responses match ({len(json.loads(fresh))} records)")
    return 0


# ---------------------------------------------------------------------------
# Self-test: this consumer's own anti-vacuous twin.
#
# A conformance checker that agrees with everything proves nothing, and one
# whose hash is subtly wrong would disagree with everything for the wrong
# reason. So the BLAKE3 is pinned against the published test vectors (which
# is what makes agreement with Newt meaningful rather than circular), and
# every check below is shown to FAIL on input it should reject.
# ---------------------------------------------------------------------------

# From the BLAKE3 specification's test vector file. Input of length N is the
# repeating byte sequence 0, 1, 2, ..., 250, 0, 1, ... — chosen here to span
# one partial block, exactly one chunk, exactly two, and three.
BLAKE3_VECTORS = [
    (0, "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"),
    (1, "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213"),
    (63, "e9bc37a594daad83be9470df7f7b3798297c3d834ce80ba85d6e207627b7db7b"),
    (64, "4eed7141ea4a5cd4b788606bd23f46e212af9cacebacdc7d1f4c6dc7f2511b98"),
    (1023, "10108970eeda3eb932baac1428c7a2163b0e924c9a9e25b35bba72b28f70bd11"),
    (1024, "42214739f095a406f3fc83deb889744ac00df831c10daa55189b5d121c855af7"),
    (2048, "e776b6028c7cd22a4d0ba182a8bf62205d2ef576467e838ed6f2529b85fba24a"),
    (3072, "b98cb0ff3623be03326b373de6b9095218513e64f1ee2edd2525c7ad1e5cffd2"),
]


def _blake3_input(n: int) -> bytes:
    return bytes((i % 251) for i in range(n))


def _fails(fn, *args, **kwargs) -> bool:
    """Whether `fn` rejects — by exception or by a non-zero return."""
    try:
        result = fn(*args, **kwargs)
    except Exception:  # noqa: BLE001 — any rejection counts as a rejection
        return True
    return result != 0 if isinstance(result, int) else False


def self_test() -> int:
    failures = []

    def check(name: str, got, expected) -> None:
        if got != expected:
            failures.append(f"{name}: {got!r} != {expected!r}")

    # 1. The hash, against answers computed by neither this file nor Newt.
    for size, expected in BLAKE3_VECTORS:
        check(f"blake3(len={size})", blake3(_blake3_input(size)).hex(), expected)

    # 2. Smallest-form integer heads. A non-smallest encoding is the classic
    #    way two implementations produce different bytes for equal values.
    for value, expected in [
        (0, "00"), (23, "17"), (24, "1818"), (255, "18ff"),
        (256, "190100"), (65535, "19ffff"), (65536, "1a00010000"),
        (1 << 32, "1b0000000100000000"),
    ]:
        check(f"head({value})", encode(value).hex(), expected)
    check("negative", encode(-1).hex(), "20")
    check("true", encode(True).hex(), "f5")
    check("null", encode(None).hex(), "f6")

    # 3. Map keys sort by BYTE LENGTH first, then bytewise — not
    #    lexicographically. "z" precedes "aa"; plain sorted() reverses them.
    check("key order", encode({"aa": 1, "z": 2}).hex(), "a2617a0262616101")
    check("empty map and list", encode({"a": {}, "b": []}).hex(), "a26161a0616280")

    # 4. A type DAG-CBOR has no encoding for is refused, not coerced.
    if not _fails(encode, 1.5):
        failures.append("a float was encoded rather than refused")

    # 5. CID rendering round-trips.
    sample = vector(load_vectors(), "definition")["content_id"]
    check("cid round-trip", bytes_to_cid(cid_to_bytes(sample)), sample)

    # 6. ANTI-VACUOUS: perturbation must be caught, at each step separately.
    real = vector(load_vectors(), "definition")
    perturbed = json.loads(json.dumps(real["json"]))
    perturbed["revision"] = real["json"]["revision"] + 1
    if encode(perturbed).hex() == real["dagcbor_hex"]:
        failures.append("changing a field did not change the bytes")
    if content_id(encode(perturbed)) == real["content_id"]:
        failures.append("changing a field did not change the id")
    # ...and a byte flipped in the ENCODING must move the id even when the
    # JSON is untouched, which is the step `verify` reports separately.
    flipped = bytearray(bytes.fromhex(real["dagcbor_hex"]))
    flipped[-1] ^= 0x01
    if content_id(bytes(flipped)) == real["content_id"]:
        failures.append("flipping an encoded byte did not change the id")

    # 7. ANTI-VACUOUS: links are not strings. Encoding a link field as a
    #    plain string must NOT reproduce the bytes — this is the trap the
    #    `links` list exists to warn a foreign implementation about.
    inst = vector(load_vectors(), "instance")
    if encode(inst["json"]).hex() == inst["dagcbor_hex"]:
        failures.append("a CID encoded as a string reproduced the bytes — the "
                        "link encoding is not being exercised")
    check("instance with links", encode(inst["json"], frozenset(inst["links"])).hex(),
          inst["dagcbor_hex"])

    # 8. Fail-closed on capability, BOTH directions.
    demanding = {"markdown": "x", "controls": [], "features": [
        {"feature": "no-such-surface", "requirement": "required"}]}
    if not _fails(check_features, demanding):
        failures.append("an unsupported REQUIRED feature was accepted")
    demanding["features"][0]["requirement"] = "optional"
    if _fails(check_features, demanding):
        failures.append("an unsupported OPTIONAL feature was refused — the "
                        "capability check refuses everything")
    if _fails(check_features, real["json"]):
        failures.append("the real definition was refused; its required "
                        "feature is one this consumer declares")

    # 9. A choice answers with the REFUSING option when there is one, and
    #    falls back to the first only when there is not.
    grant_deny = {"id": "d", "label": "", "requirement": "required", "kind": {"choice": {
        "options": [{"id": "yes", "role": "allow", "label": ""},
                    {"id": "no", "role": "deny", "label": ""}]}}}
    check("refusing option chosen", answer_for(grant_deny)["option"], "no")
    neutral = json.loads(json.dumps(grant_deny))
    for opt in neutral["kind"]["choice"]["options"]:
        opt["role"] = "neutral"
    check("no refusal available", answer_for(neutral)["option"], "yes")

    # 10. A response answers every REQUIRED control, not only the one it
    #     came for — and does not pad itself with optional ones it was not
    #     asked about.
    controls = real["json"]["controls"]
    optional = next(c for c in controls if c["requirement"] == "optional")
    required = next(c for c in controls if c["requirement"] == "required")
    answered = [v["control"] for v in values_for(real["json"], optional)]
    check("required control answered", required["id"] in answered, True)
    check("optional response length", len(answered), 2)
    check("required-only response", [v["control"] for v in values_for(real["json"], required)],
          [required["id"]])

    # 11. An unknown control kind is refused rather than rendered blank.
    unknown = {"id": "u", "label": "u", "requirement": "optional", "kind": "quantum"}
    if not _fails(answer_for, unknown):
        failures.append("an unknown control kind produced a value")
    if not _fails(render, {"markdown": "x", "controls": [unknown], "features": []}):
        failures.append("an unknown control kind rendered instead of refusing")

    if failures:
        for line in failures:
            print(f"conformance self-test FAIL: {line}", file=sys.stderr)
        print(f"conformance: {len(failures)} self-test failure(s)", file=sys.stderr)
        return 1
    print(f"conformance: self-test passed ({len(BLAKE3_VECTORS)} hash vectors, "
          "11 checks)")
    return 0


COMMANDS = {
    "verify": cmd_verify,
    "render": cmd_render,
    "author": cmd_author,
    "check": cmd_check,
}


def main(argv: list[str]) -> int:
    if "--self-test" in argv:
        return self_test()
    name = argv[1] if len(argv) > 1 else "verify"
    command = COMMANDS.get(name)
    if command is None:
        print(f"usage: {argv[0]} [--self-test|{'|'.join(COMMANDS)}]", file=sys.stderr)
        return 2
    return command()


if __name__ == "__main__":
    sys.exit(main(sys.argv))
