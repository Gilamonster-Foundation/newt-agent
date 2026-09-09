//! **`newt frame` forensics, end to end.**
//!
//! The architectural claim these tests exist to hold: *the CLI and the harness
//! reach the same verification decision because they call the same function.*
//! A CLI test that only exercises the happy path cannot see a second
//! implementation drifting, so every case here is driven to both verdicts —
//! and the tamper cases change ONE thing each, so a refusal is attributable.
//!
//! Per CRAFT-20 the assertions are made against the **JSON** report, not the
//! rendered text: a fact only reachable by reading the pretty form does not
//! count as reported, and asserting on prose would let that regress unnoticed.

use agent_frame::{Op, RootEvent, RootKind, Span, Unit};
use predicates::prelude::*;
use std::path::Path;

mod common;

const SRC: &[u8] = b"the quick brown fox jumps over the lazy dog";

/// Lay down a frame store: the source blob, the root event, one unit, and a
/// two-packet chain. Returns the unit id and the head packet id.
fn seed(dir: &Path) -> (String, String, String) {
    std::fs::create_dir_all(dir).unwrap();

    // The source blob, filed under its RAW id (the opaque-bytes profile).
    let src_id = content_addressable::RawContentId::from_content(SRC);
    std::fs::write(dir.join(src_id.to_string()), SRC).unwrap();

    // The root event, filed under its dag-cbor id.
    let root = RootEvent::new(RootKind::OperatorPrompt, b"summarise the log", 7);
    let root_id = root.id().unwrap();
    std::fs::write(
        dir.join(format!("{root_id}.json")),
        serde_json::to_vec(&root).unwrap(),
    )
    .unwrap();

    let unit = Unit::seal(Op::Elide, SRC, Span::new(4, 19), root_id).unwrap();
    let uid = unit.id().unwrap();
    std::fs::write(
        dir.join(format!("{uid}.json")),
        serde_json::to_vec(&unit).unwrap(),
    )
    .unwrap();

    let genesis = agent_frame::Packet::genesis(vec![uid]);
    let gid = genesis.id().unwrap();
    std::fs::write(
        dir.join(format!("{gid}.json")),
        serde_json::to_vec(&genesis).unwrap(),
    )
    .unwrap();

    let head = agent_frame::Packet::following(gid, vec![uid]);
    let hid = head.id().unwrap();
    std::fs::write(
        dir.join(format!("{hid}.json")),
        serde_json::to_vec(&head).unwrap(),
    )
    .unwrap();

    (uid.to_string(), hid.to_string(), gid.to_string())
}

fn json_of(out: &[u8]) -> serde_json::Value {
    serde_json::from_slice(out).expect("--json must emit parseable JSON")
}

#[test]
fn verify_reports_a_good_unit_and_exits_zero() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (uid, _, _) = seed(&dir);

    let out = n
        .args(["frame", "verify", &uid, "--json", "--frame"])
        .arg(&dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v = json_of(&out);
    assert_eq!(v["command"], "frame.verify");
    assert_eq!(v["verified"], true);
    assert_eq!(v["op"], "Elide");
    assert_eq!(v["check"], "Rederive");
    assert_eq!(v["depth"], 0);
    assert_eq!(v["span"]["start"], 4);
    assert_eq!(v["span"]["end"], 19);
    assert_eq!(v["span"]["len"], 15);
    assert_eq!(v["source_len"], SRC.len());
    // The root resolved to an EVENT, so the report says which turn — not merely
    // that it was "an operator prompt".
    assert_eq!(v["root_kind"], "OperatorPrompt");
    assert_eq!(v["root_seq"], 7);
    assert!(v["failure"].is_null());
}

/// **The exit code is the contract.** A script gates on it, so a mismatch must
/// be non-zero and must still emit the report.
#[test]
fn verify_exits_non_zero_when_the_source_is_wrong() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (uid, _, _) = seed(&dir);

    let wrong = n.home().join("wrong.txt");
    std::fs::write(&wrong, b"the quick brown fox jumps over the lazy cat").unwrap();

    let out = n
        .args(["frame", "verify", &uid, "--json", "--frame"])
        .arg(&dir)
        .arg("--source")
        .arg(&wrong)
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let v = json_of(&out);
    assert_eq!(v["verified"], false);
    assert_eq!(v["failure"]["kind"], "source_mismatch");
    // Both sides named — "it did not verify" without them sends the reader to a
    // debugger.
    assert!(v["failure"]["declared"].is_string());
    assert!(v["failure"]["actual"].is_string());
    assert_ne!(v["failure"]["declared"], v["failure"]["actual"]);
}

/// A forged `elided` claim is caught by the CLI **because the CLI runs the same
/// re-derivation**. Nothing here re-implements the comparison.
#[test]
fn verify_catches_a_forged_elision_claim() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (uid, _, _) = seed(&dir);

    // Rewrite the stored unit with a lying `elided`, refiled under its NEW id
    // so the store's own id check passes and only re-derivation can object.
    let root = RootEvent::new(RootKind::OperatorPrompt, b"summarise the log", 7)
        .id()
        .unwrap();
    let mut raw = Unit::seal(Op::Elide, SRC, Span::new(4, 19), root)
        .unwrap()
        .to_raw();
    raw.elided = content_addressable::RawContentId::from_content(b"quick brown cat");
    let forged = Unit::try_from(raw).expect("shape is fine; the lie is about the world");
    let fid = forged.id().unwrap();
    std::fs::write(
        dir.join(format!("{fid}.json")),
        serde_json::to_vec(&forged).unwrap(),
    )
    .unwrap();
    assert_ne!(fid.to_string(), uid, "the forgery has its own address");

    let out = n
        .args(["frame", "verify", &fid.to_string(), "--json", "--frame"])
        .arg(&dir)
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let v = json_of(&out);
    assert_eq!(v["verified"], false);
    assert_eq!(v["failure"]["kind"], "elision_mismatch");
}

/// A file filed under an id it does not hash to is store corruption, and the
/// read must refuse rather than report the contents as fact.
#[test]
fn a_node_filed_under_the_wrong_id_is_refused() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (uid, _, _) = seed(&dir);

    // Same filename, different content: now the file lies about what it is.
    let root = RootEvent::new(RootKind::UserAction, b"different", 1)
        .id()
        .unwrap();
    let other = Unit::seal(Op::Elide, SRC, Span::new(0, 3), root).unwrap();
    std::fs::write(
        dir.join(format!("{uid}.json")),
        serde_json::to_vec(&other).unwrap(),
    )
    .unwrap();

    n.args(["frame", "verify", &uid, "--frame"])
        .arg(&dir)
        .assert()
        .failure()
        .stderr(predicate::str::contains("store corruption"));
}

#[test]
fn explain_reports_the_derivation() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (uid, _, _) = seed(&dir);

    let out = n
        .args(["frame", "explain", &uid, "--json", "--frame"])
        .arg(&dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v = json_of(&out);
    assert_eq!(v["command"], "frame.explain");
    assert_eq!(v["op"], "Elide");
    assert_eq!(v["asserts"], false, "elision asserts nothing");
    assert_eq!(v["well_formed"], true);
    assert_eq!(v["evictable"], false);
    assert_eq!(v["root_kind"], "OperatorPrompt");
    assert_eq!(v["root_seq"], 7);
    for k in ["unit", "source", "elided", "root", "root_content"] {
        assert!(v[k].is_string(), "explain must report {k}: {v}");
    }
}

/// **Genesis is stated, not implied by an absent field.** A frame with no
/// parents is the defined bottom of the chain, and a reader must be TOLD that —
/// otherwise it renders like a parent link that failed to load, and those are
/// the difference between a clean answer and a silent hole.
#[test]
fn parents_of_a_genesis_packet_says_genesis() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (uid, _, gid) = seed(&dir);

    let v = json_of(
        &n.args(["frame", "parents", &gid, "--json", "--frame"])
            .arg(&dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(v["kind"], "packet");
    assert_eq!(v["outcome"], "genesis");
    assert!(v["parent"].is_null(), "genesis has no parent to name");
    assert_eq!(v["units"][0], uid);
    assert!(
        v["meaning"].as_str().unwrap().contains("chain ends here"),
        "the meaning must be IN the data, not only in the renderer: {v}"
    );
}

/// **One link, named but not followed.** The parent id is reported so the
/// caller can compose; this command does not walk.
#[test]
fn parents_of_a_child_packet_names_one_link_and_stops() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (_, head, gid) = seed(&dir);

    let v = json_of(
        &n.args(["frame", "parents", &head, "--json", "--frame"])
            .arg(&dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(v["outcome"], "parent");
    assert_eq!(v["parent"], gid, "the ONE link, named");
    assert_ne!(
        v["outcome"], "genesis",
        "a packet with a parent must never render as an origin"
    );
    // No chain field at all: there is no walker to report one.
    assert!(v["chain"].is_null(), "this command does not walk: {v}");
}

/// **Failure to resolve is loud, and is not an absence.** A subject that cannot
/// be read exits non-zero with a message — it does not come back as `genesis`.
#[test]
fn an_unreadable_subject_is_an_error_not_a_genesis() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (_, head, gid) = seed(&dir);

    // Delete the genesis packet, then ask about it directly.
    std::fs::remove_file(dir.join(format!("{gid}.json"))).unwrap();
    n.args(["frame", "parents", &gid, "--frame"])
        .arg(&dir)
        .assert()
        .failure();

    // And the child still names it as its parent — reporting the link is honest
    // even though following it would now fail, because we do not follow it.
    let mut n2 = common::newt();
    let v = json_of(
        &n2.args(["frame", "parents", &head, "--json", "--frame"])
            .arg(&dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(v["outcome"], "parent");
    assert_eq!(v["parent"], gid);
}

/// A unit is a leaf. `parents` names what its derivation addresses and follows
/// neither.
#[test]
fn parents_of_a_unit_names_its_antecedents() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (uid, _, _) = seed(&dir);

    let v = json_of(
        &n.args(["frame", "parents", &uid, "--json", "--frame"])
            .arg(&dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(v["kind"], "unit");
    assert_eq!(v["outcome"], "antecedents");
    let a = v["antecedents"].as_array().expect("antecedents");
    let rels: Vec<&str> = a.iter().map(|x| x["relation"].as_str().unwrap()).collect();
    assert_eq!(rels, vec!["source", "root"]);
    assert_eq!(a[0]["profile"], "raw", "a source is opaque bytes");
    assert_eq!(a[1]["profile"], "dag-cbor", "a root event is a value");
}

/// **Composition, not a walker.** `verify` emits the source id it checked
/// against, which is what makes the next link the caller's one-line job — and
/// it labels the profile, so nobody pipes a raw id into a slot wanting a
/// dag-cbor one and gets a confusing parse error instead of an explanation.
#[test]
fn verify_emits_the_source_it_checked_so_a_caller_can_compose() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (uid, _, _) = seed(&dir);

    let v = json_of(
        &n.args(["frame", "verify", &uid, "--json", "--frame"])
            .arg(&dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    let src = v["source"].as_str().expect("verify must report its source");
    assert_eq!(
        src,
        content_addressable::RawContentId::from_content(SRC).to_string()
    );
    assert_eq!(v["source_profile"], "raw");
    assert!(
        src.parse::<agent_frame::UnitId>().is_err(),
        "a raw-profile source id is deliberately NOT a unit id; the report says \
         so rather than leaving a caller to discover it by a parse failure"
    );
}

/// **CRAFT-20.** The human rendering is a projection of the report, so every
/// fact in the JSON is reachable without it — and the two cannot disagree,
/// because there is only one assembly.
#[test]
fn the_human_rendering_carries_the_same_facts_as_the_json() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (uid, _, _) = seed(&dir);

    let json = json_of(
        &n.args(["frame", "explain", &uid, "--json", "--frame"])
            .arg(&dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );

    let mut n2 = common::newt();
    let text = String::from_utf8(
        n2.args(["frame", "explain", &uid, "--frame"])
            .arg(&dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();

    for key in ["unit", "source", "elided", "root", "root_content"] {
        let want = json[key].as_str().unwrap();
        assert!(
            text.contains(want),
            "the rendered form drops `{key}` ({want}), so that fact is only in the JSON:\n{text}"
        );
    }
    assert!(text.contains("OperatorPrompt") && text.contains("seq 7"));
}
