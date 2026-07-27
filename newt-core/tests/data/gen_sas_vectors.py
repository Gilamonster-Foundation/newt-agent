"""Independent implementation of the newt passkey SAS transcript spec.

Written from the module doc comments in newt-core/src/sas_transcript.rs, NOT
from the Rust source, so agreement between the two is evidence the spec is
portable. Stands in for the browser JS port.
"""

import base64
import hashlib
import json
import pathlib

from blake3 import blake3

TRANSCRIPT_DOMAIN = b"newt/passkey-transcript/v1"
COMMITMENT_DOMAIN = b"newt/passkey-commitment/v1"
WORD_BITS = 11
SAS_WORD_COUNT = 6

HERE = pathlib.Path(__file__).resolve().parent
WORDLIST_PATH = HERE.parent.parent / "data" / "sas-wordlist.txt"
OUT_PATH = HERE / "sas-golden-vectors.json"

with open(WORDLIST_PATH, "rb") as fh:
    raw = fh.read()
WORDS = raw.decode().splitlines()
assert len(WORDS) == 1 << WORD_BITS, len(WORDS)


def push_field(out: bytearray, field: bytes) -> None:
    out += len(field).to_bytes(8, "big")
    out += field


def commit(cose_pubkey: bytes, blinding: bytes) -> bytes:
    payload = bytearray(COMMITMENT_DOMAIN)
    push_field(payload, cose_pubkey)
    push_field(payload, blinding)
    return blake3(bytes(payload)).digest()


def transcript_id(c) -> bytes:
    payload = bytearray(TRANSCRIPT_DOMAIN)
    push_field(payload, c["rp_id"].encode())
    push_field(payload, c["issuer"].encode())
    push_field(payload, c["subject"].encode())
    push_field(payload, c["mesh_agent_fingerprint"].encode())
    push_field(payload, c["cose_alg"].to_bytes(8, "big", signed=True))
    push_field(payload, c["cose_pubkey"])
    push_field(payload, c["commitment"])
    push_field(payload, c["enroll_nonce"])
    return blake3(bytes(payload)).digest()


def sas_words(digest: bytes) -> list:
    words = []
    for i in range(SAS_WORD_COUNT):
        value = 0
        for bit in range(i * WORD_BITS, (i + 1) * WORD_BITS):
            set_ = digest[bit // 8] & (0x80 >> (bit % 8)) != 0
            value = (value << 1) | int(set_)
        words.append(WORDS[value])
    return words


def b64(data: bytes) -> str:
    return base64.b64encode(data).decode()


CASES = [
    {
        "name": "es256 typical enrollment",
        "rp_id": "newt.example",
        "issuer": "9f2c1a4b8d6e",
        "subject": "operator",
        "mesh_agent_fingerprint": "3a7b0c5d9e1f",
        "cose_alg": -7,
        "cose_pubkey": bytes(range(0x20, 0x60)),
        "blinding": bytes(range(32)),
        "enroll_nonce": bytes([0xA5] * 32),
    },
    {
        "name": "ed25519 alg negative two-byte encoding",
        "rp_id": "newt.example.org",
        "issuer": "0011223344556677889aabbccddeeff00112233445566778899aabbccddeeff0",
        "subject": "shawn",
        "mesh_agent_fingerprint": "ffffffffffff",
        "cose_alg": -8,
        "cose_pubkey": bytes([0x01, 0x02, 0x03]),
        "blinding": bytes([0xFF] * 16),
        "enroll_nonce": b"nonce-two",
    },
    {
        "name": "empty fields still frame unambiguously",
        "rp_id": "",
        "issuer": "",
        "subject": "",
        "mesh_agent_fingerprint": "",
        "cose_alg": 0,
        "cose_pubkey": b"",
        "blinding": b"",
        "enroll_nonce": b"",
    },
    {
        "name": "framing boundary a: issuer ab subject c",
        "rp_id": "rp",
        "issuer": "ab",
        "subject": "c",
        "mesh_agent_fingerprint": "fp",
        "cose_alg": -7,
        "cose_pubkey": b"k",
        "blinding": b"b",
        "enroll_nonce": b"n",
    },
    {
        "name": "framing boundary b: issuer a subject bc",
        "rp_id": "rp",
        "issuer": "a",
        "subject": "bc",
        "mesh_agent_fingerprint": "fp",
        "cose_alg": -7,
        "cose_pubkey": b"k",
        "blinding": b"b",
        "enroll_nonce": b"n",
    },
]

out_cases = []
for c in CASES:
    c["commitment"] = commit(c["cose_pubkey"], c["blinding"])
    t = transcript_id(c)
    out_cases.append(
        {
            "name": c["name"],
            "rp_id": c["rp_id"],
            "issuer": c["issuer"],
            "subject": c["subject"],
            "mesh_agent_fingerprint": c["mesh_agent_fingerprint"],
            "cose_alg": c["cose_alg"],
            "cose_pubkey_b64": b64(c["cose_pubkey"]),
            "blinding_b64": b64(c["blinding"]),
            "enroll_nonce_b64": b64(c["enroll_nonce"]),
            "commitment_hex": c["commitment"].hex(),
            "transcript_hex": t.hex(),
            "sas_words": sas_words(t),
        }
    )

# Fixed digests that pin the bit-extraction order itself, independent of any
# hashing: all-zero, all-ones, and a single leading set bit.
SAS_CASES = [
    ("all zero bits select the first word", bytes(32)),
    ("all one bits select the last word", bytes([0xFF] * 32)),
    ("leading set bit only", bytes([0x80]) + bytes(31)),
    ("alternating bytes", bytes([0xAA, 0x55] * 16)),
]
out_sas = [
    {"name": n, "transcript_hex": d.hex(), "sas_words": sas_words(d)} for n, d in SAS_CASES
]

doc = {
    "_comment": (
        "Golden vectors for the passkey enrollment transcript and SAS. "
        "Generated by an implementation written independently from the Rust "
        "source, against the spec in newt-core/src/sas_transcript.rs. Every "
        "port must reproduce these byte-for-byte."
    ),
    "wordlist_sha256": hashlib.sha256(raw).hexdigest(),
    "wordlist_blake3": blake3(raw).hexdigest(),
    "transcript_domain": TRANSCRIPT_DOMAIN.decode(),
    "commitment_domain": COMMITMENT_DOMAIN.decode(),
    "word_bits": WORD_BITS,
    "sas_word_count": SAS_WORD_COUNT,
    "cases": out_cases,
    "sas_cases": out_sas,
}

with open(OUT_PATH, "w") as fh:
    json.dump(doc, fh, indent=2)
    fh.write("\n")
print("wrote", OUT_PATH)
for c in out_sas:
    print(" ", c["name"], "->", " ".join(c["sas_words"]))
print("case0 sas:", " ".join(out_cases[0]["sas_words"]))
print("boundary a/b differ:", out_cases[3]["transcript_hex"] != out_cases[4]["transcript_hex"])
