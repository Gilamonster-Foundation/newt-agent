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

/// **An unresolvable root is reported, not omitted.** The store is the layer
/// that owns root resolution — `agent-frame`'s admission proves only that the
/// id is well formed, and `verify_unit` never touches it.
///
/// So the distinction has to be visible here or nowhere: a unit whose root
/// event is absent still VERIFIES (its source, span and elided claims are all
/// true) while `root_unresolved` says the provenance could not be followed.
/// Reporting a verified unit with silently missing `root_kind` would let a
/// reader take a green verdict for a provenance statement it never made.
#[test]
fn a_root_that_does_not_resolve_is_reported_not_silently_dropped() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (uid, _, _) = seed(&dir);

    // Anti-vacuous: with the root event present, it resolves and is named.
    let v = json_of(
        &n.args(["frame", "verify", &uid, "--json", "--frame"])
            .arg(&dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(v["root_kind"], "OperatorPrompt");
    assert!(v["root_unresolved"].is_null());

    // Remove only the root event. Nothing about the unit's own claims changes.
    let root_id = v["root"].as_str().unwrap().to_string();
    std::fs::remove_file(dir.join(format!("{root_id}.json"))).unwrap();

    let mut n2 = common::newt();
    let v = json_of(
        &n2.args(["frame", "verify", &uid, "--json", "--frame"])
            .arg(&dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(
        v["verified"], true,
        "the unit's own claims still re-derive: the root is not one of them"
    );
    assert_eq!(v["root"], root_id, "the id is still reported");
    assert!(
        v["root_kind"].is_null(),
        "nothing may be claimed about an event that was not read: {v}"
    );
    assert!(
        v["root_unresolved"].is_string(),
        "the failure to resolve must be SAID, not left as an absent field: {v}"
    );
}

/// **A two-parent node is not an origin, and must not render as one.**
///
/// The regression this holds: while `Packet` derived `Deserialize`, a Merkle
/// node with two parents decoded straight into a trusted `Packet`. `prior()`
/// returned `None` — it names a link only when there is exactly one — so this
/// command reported `outcome: "genesis"`, *"no parents: this frame is an
/// origin, and the chain ends here"*, of a packet with two.
///
/// That is the third outcome this file exists to keep apart, in its worst form:
/// not "I could not resolve it" rendered as an origin, but "I dropped the links
/// I did resolve" rendered as an origin. The node here is legitimately
/// addressed and filed under its true id, so the store's own id check passes
/// and only admission can object.
#[test]
fn a_two_parent_node_is_refused_not_reported_as_genesis() {
    let mut n = common::newt();
    let dir = n.home().join("frame");
    let (_, head, gid) = seed(&dir);

    // A real DAG node over the two packets the seed laid down.
    let forked = agent_frame::RawPacket::new(
        agent_frame::PacketBody { units: vec![] },
        [
            head.parse::<agent_frame::PacketId>()
                .unwrap()
                .into_content_id(),
            gid.parse::<agent_frame::PacketId>()
                .unwrap()
                .into_content_id(),
        ],
    );
    assert_eq!(forked.parents().len(), 2, "the fixture must really fork");
    let fid = content_addressable::ContentAddressable::content_id(&forked).unwrap();
    std::fs::write(
        dir.join(format!("{fid}.json")),
        serde_json::to_vec(&forked).unwrap(),
    )
    .unwrap();

    // Anti-vacuous: the one-parent sibling in the same store still answers.
    let v = json_of(
        &n.args(["frame", "parents", &head, "--json", "--frame"])
            .arg(&dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(
        v["outcome"], "parent",
        "the honest chain link still reports"
    );

    // The forked node: a loud failure, never `genesis`.
    let mut n2 = common::newt();
    let out = n2
        .args(["frame", "parents", &fid.to_string(), "--json", "--frame"])
        .arg(&dir)
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("genesis"),
        "a two-parent node must not render as an origin: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("CHAIN"),
        "the refusal must say what was wrong with it: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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

/// Grounds the harness's canonical store in the actual forensic executable.
#[test]
fn canonical_harness_units_and_packets_use_the_existing_reports() {
    let fixture = common::newt();
    let dir = fixture.home().join("frame");
    let mut store = agent_harness::store::FrameStore::open(&dir).unwrap();
    store.put_source(SRC).unwrap();
    store.put_source(b"summarise the log").unwrap();
    let root = RootEvent::new(RootKind::OperatorPrompt, b"summarise the log", 7);
    let root_id = store.put(&root).unwrap();
    let unit = Unit::seal(Op::Elide, SRC, Span::new(4, 19), root_id).unwrap();
    let uid = store.put(&unit).unwrap();
    let packet = agent_frame::Packet::genesis(vec![uid.into()]);
    let pid = store.put(&packet).unwrap();
    for command in ["verify", "explain"] {
        let mut process = common::newt();
        let output = process
            .args(["frame", command, &uid.to_string(), "--json", "--frame"])
            .arg(&dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let report = json_of(&output);
        assert_eq!(
            report["source"],
            content_addressable::RawContentId::from_content(SRC).to_string()
        );
        assert_eq!(report["root_kind"], "OperatorPrompt");
        assert_eq!(report["depth"], 0);
    }
    let mut process = common::newt();
    let output = process
        .args(["frame", "parents", &pid.to_string(), "--json", "--frame"])
        .arg(&dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(json_of(&output)["units"][0], uid.to_string());

    // A legacy JSON twin cannot hide corruption of the authoritative format.
    std::fs::write(
        dir.join(format!("{uid}.json")),
        serde_json::to_vec(&unit).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.join(format!("{uid}.cbor")), b"substituted").unwrap();
    let mut process = common::newt();
    process
        .args(["frame", "verify", &uid.to_string(), "--frame"])
        .arg(&dir)
        .assert()
        .failure();
}

/// Grounds byte-perfect replay in a cold process, then changes an addressed
/// source on disk and checks the verifier refuses before emitting any request.
#[test]
fn replay_reconstructs_recorded_bytes_and_refuses_substitution() {
    let fixture = common::newt();
    let dir = fixture.home().join("frame");
    let mut session = agent_harness::Session::open(&dir, Default::default()).unwrap();
    let request = session
        .record_request(
            serde_json::json!({
                "model":"fixture-model", "messages":[
                    {"role":"system","content":"rules"}, {"role":"user","content":"task"}
                ], "tools":[], "temperature":0.2
            }),
            "openai",
        )
        .unwrap();
    drop(session);
    let mut process = common::newt();
    let output = process
        .args(["frame", "replay", &request.id.to_string(), "--frame"])
        .arg(&dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(output, request.bytes);
    let mut process = common::newt();
    let output = process
        .args([
            "frame",
            "replay",
            &request.id.to_string(),
            "--json",
            "--frame",
        ])
        .arg(&dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let receipt = json_of(&output);
    assert_eq!(receipt["verified"], true);
    assert_eq!(receipt["byte_count"], request.bytes.len());
    assert_eq!(receipt["body"].as_str().unwrap().as_bytes(), request.bytes);
    let source = content_addressable::RawContentId::from_content(&request.bytes);
    std::fs::write(dir.join(source.to_string()), b"substituted").unwrap();
    let mut process = common::newt();
    let assertion = process
        .args(["frame", "replay", &request.id.to_string(), "--frame"])
        .arg(&dir)
        .assert()
        .failure();
    assert!(assertion.get_output().stdout.is_empty());
}

/// Grounds bounded session/event inspection in the actual CLI after restart.
#[test]
fn a_session_head_explains_its_verdict_and_exposes_immediate_links() {
    let fixture = common::newt();
    let dir = fixture.home().join("frame");
    let mut session = agent_harness::Session::open(&dir, Default::default()).unwrap();
    let request = session
        .record_request(
            serde_json::json!({"messages":[{"role":"user","content":"task"}]}),
            "openai",
        )
        .unwrap();
    let reply = session.record_reply(request.id, b"Which path?").unwrap();
    session
        .record_verdict(reply, agent_harness::Verdict::Question)
        .unwrap();
    let head = session.head().to_string();
    drop(session);
    for command in ["explain", "parents"] {
        let mut process = common::newt();
        let output = process
            .args(["frame", command, &head, "--json", "--frame"])
            .arg(&dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let report = json_of(&output);
        assert_eq!(report["inspection"]["kind"], "journal");
        assert_eq!(
            report["inspection"]["record"]["verdict"]["reply"],
            reply.to_string()
        );
        assert_eq!(report["inspection"]["graph_admitted"], false);
    }
    let mut process = common::newt();
    process
        .args(["frame", "explain", &head, "--max-bytes", "1", "--frame"])
        .arg(&dir)
        .assert()
        .failure();
    std::fs::remove_file(dir.join(format!("{reply}.cbor"))).unwrap();
    let mut process = common::newt();
    let assertion = process
        .args(["frame", "parents", &head, "--frame"])
        .arg(&dir)
        .assert()
        .failure();
    assert!(assertion.get_output().stdout.is_empty());
}
