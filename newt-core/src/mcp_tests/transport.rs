use super::*;

#[test]
fn transport_keywords_round_trip() {
    for kind in [
        TransportKind::Stdio,
        TransportKind::Sse,
        TransportKind::Http,
    ] {
        assert_eq!(TransportKind::from_keyword(kind.as_str()), Some(kind));
    }
    assert_eq!(TransportKind::Stdio.as_str(), "stdio");
    assert_eq!(TransportKind::from_keyword("grpc"), None);
}
