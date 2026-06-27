//! Canonical behavioral spec for T1 — the **ungameable** grade (see T0's spec).
//! Dropped into the produced tree by `ratchet.sh`; the agent never sees it.
use t1_parse_port::parse_port;

#[test]
fn parses_valid_and_rejects_invalid() {
    assert_eq!(parse_port("8080"), Some(8080));
    assert_eq!(parse_port("0"), Some(0));
    assert_eq!(parse_port("65535"), Some(65535));
    assert_eq!(parse_port("not-a-port"), None);
    assert_eq!(parse_port(""), None);
}
