//! Newt-owned golden vectors for the content-addressed spill record (#1528 B3
//! §2.11).
//!
//! For a FIXED session nonce `[7u8; 16]`, `data/spill-cid-vectors.json` pins,
//! per case, the canonical dag-cbor bytes, the CID handle text
//! ([`SpillCid::to_handle`]), the full CID envelope bytes
//! ([`content_addressable::ContentId::to_bytes`]), and every field needed to
//! rebuild the [`SpillRecordV1`]. Each test below rebuilds the record from the
//! pinned fields and recomputes all three, asserting byte/text equality.
//!
//! This is the cross-version / drift gate. If the schema tag
//! (`SPILL_SCHEMA_V1`), the dag-cbor encoding, or the frozen CID profile ever
//! changes, these vectors break. That break is BY DESIGN: **changing any pinned
//! value requires an explicit spill-record schema-version decision (bump
//! `SPILL_SCHEMA_V1`) — never a silent vector regeneration.** A green test that
//! pins wrong values is useless; the pinned values are the real computed ones,
//! regenerated deliberately (see the `dump` note at the bottom of this file).
//!
//! The record identity types are reached through the crate's public manifest
//! (`newt_core::agentic`), the same seam the SAS golden-vector test uses.

use content_addressable::ContentAddressable;
use newt_core::agentic::{SpillCid, SpillProvenance, SpillRecordV1, SpillScope};
use serde_json::Value;

const VECTORS: &str = include_str!("data/spill-cid-vectors.json");

fn doc() -> Value {
    serde_json::from_str(VECTORS).expect("spill golden vectors parse")
}

/// Lowercase hex of a byte slice — the exact form the vectors are pinned in.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).expect("nibble is a hex digit"));
        s.push(char::from_digit((b & 0x0f) as u32, 16).expect("nibble is a hex digit"));
    }
    s
}

/// Decode a hex string (e.g. the pinned 16-byte session nonce) into bytes.
fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "hex string has an even length");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex byte"))
        .collect()
}

/// Rebuild the [`SpillProvenance`] from the pinned `{ kind, tool_name? }` shape.
fn provenance(case: &Value) -> SpillProvenance {
    let prov = &case["provenance"];
    match prov["kind"].as_str().expect("provenance kind") {
        "tool_output" => SpillProvenance::ToolOutput {
            // A pinned JSON `null` → `None`; a string → `Some(..)`.
            tool_name: prov["tool_name"].as_str().map(str::to_owned),
        },
        "compaction_span" => SpillProvenance::CompactionSpan,
        other => panic!("unknown provenance kind: {other}"),
    }
}

/// Rebuild the full [`SpillRecordV1`] a case pins, under the fixed session nonce.
fn rebuild(doc: &Value, case: &Value) -> SpillRecordV1 {
    let nonce_bytes = unhex(
        doc["scope_session_nonce_hex"]
            .as_str()
            .expect("scope_session_nonce_hex"),
    );
    let nonce: [u8; 16] = nonce_bytes
        .as_slice()
        .try_into()
        .expect("session nonce is 16 bytes");
    let record = SpillRecordV1::new(
        SpillScope::Session(nonce),
        provenance(case),
        case["redacted_text"]
            .as_str()
            .expect("redacted_text")
            .to_owned(),
    );
    // The pinned schema tag must match the tag the constructor stamps — a bump
    // of `SPILL_SCHEMA_V1` re-addresses everything, so it is pinned explicitly.
    assert_eq!(
        record.schema,
        doc["schema"].as_str().expect("schema"),
        "the pinned schema tag drifted from SPILL_SCHEMA_V1"
    );
    record
}

#[test]
fn spill_cid_vectors_pin_canonical_bytes_cid_and_cid_bytes() {
    let doc = doc();
    let cases = doc["cases"].as_array().expect("cases array");
    assert_eq!(cases.len(), 7, "every plan case must be pinned");

    for case in cases {
        let name = case["name"].as_str().expect("case name");
        let record = rebuild(&doc, case);

        // 1. Canonical dag-cbor bytes: the exact CID pre-image.
        let canonical = record.canonical_form().expect("canonical dag-cbor");
        assert_eq!(
            hex(&canonical),
            case["canonical_dagcbor_hex"]
                .as_str()
                .expect("canonical_dagcbor_hex"),
            "{name}: canonical dag-cbor bytes drifted from the pinned vector"
        );

        // 2. CID handle text (the `bafyr4i…` a model pastes back).
        let cid = SpillCid::of(&record).expect("cid");
        let pinned_cid = case["cid"].as_str().expect("cid");
        assert_eq!(
            cid.to_handle(),
            pinned_cid,
            "{name}: CID handle drifted from the pinned vector"
        );

        // 3. Full CID envelope bytes (version + codec + multihash + digest).
        let cid_bytes = cid.as_content_id().to_bytes();
        assert_eq!(
            hex(&cid_bytes),
            case["cid_bytes_hex"].as_str().expect("cid_bytes_hex"),
            "{name}: CID envelope bytes drifted from the pinned vector"
        );
    }
}

#[test]
fn pinned_cid_text_round_trips_through_parse() {
    // The canonical handle a model pastes back must resolve: `SpillCid::parse`
    // of the pinned text yields the same CID the record recomputes to.
    let doc = doc();
    for case in doc["cases"].as_array().expect("cases array") {
        let name = case["name"].as_str().expect("case name");
        let record = rebuild(&doc, case);
        let computed = SpillCid::of(&record).expect("cid");
        let pinned_cid = case["cid"].as_str().expect("cid");

        let parsed = SpillCid::parse(pinned_cid)
            .unwrap_or_else(|e| panic!("{name}: pinned CID text must parse: {e}"));
        assert_eq!(
            parsed, computed,
            "{name}: parsed handle must equal the recomputed CID"
        );
        assert_eq!(
            parsed.to_handle(),
            pinned_cid,
            "{name}: parse→to_handle must reproduce the exact pinned text"
        );
    }
}

// To regenerate these vectors after a DELIBERATE SPILL_SCHEMA_V1 bump: rebuild
// each record above, print `hex(canonical_form())`, `cid.to_handle()`, and
// `hex(cid.as_content_id().to_bytes())`, and paste them into
// `data/spill-cid-vectors.json`. Never regenerate to make a red test green
// without that schema-version decision.
