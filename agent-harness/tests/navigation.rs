use agent_harness::navigation::{validate_selection, Candidate};
use content_addressable::ContentId;

fn candidate(name: &str, required: bool, pairs: &[&str]) -> Candidate {
    Candidate {
        id: ContentId::from_canonical_bytes(name.as_bytes()),
        bytes: 10,
        required,
        pairs: pairs.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn proposal_cannot_drop_pins_unseen_results_or_half_a_tool_pair() {
    let candidates = vec![
        candidate("operator", true, &[]),
        candidate("call", false, &["call-1"]),
        candidate("result", true, &["call-1"]),
        candidate("old", false, &[]),
    ];
    let ids: Vec<_> = candidates.iter().map(|c| c.id).collect();
    assert!(validate_selection(&candidates, &ids, 45).is_ok());
    assert!(validate_selection(&candidates, &ids[1..], 45).is_err());
    assert!(validate_selection(&candidates, &[ids[0], ids[2]], 45).is_err());
    assert!(validate_selection(&candidates, &ids, 44).is_err());
    assert!(validate_selection(&candidates, &[ids[0], ids[1], ids[2]], 34).is_ok());
    assert!(validate_selection(&candidates, &[ids[0], ids[1], ids[2], ids[2]], 45).is_err());
    assert!(validate_selection(
        &candidates,
        &[ids[0], ids[1], ids[2], candidate("foreign", false, &[]).id],
        45
    )
    .is_err());
}
