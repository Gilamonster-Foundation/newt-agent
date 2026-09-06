use super::*;
use newt_core::caveats::Scope;

#[test]
fn gated_reports_the_granted_remote_host_count() {
    let caveats = Caveats {
        net: Scope::only(["api.github.com".to_string(), "gitlab.com".to_string()]),
        ..Caveats::top()
    };
    // Proxy engaged → Gated with the allow-list size.
    assert_eq!(net_posture(&caveats, true, false), NetPosture::Gated(2));
    // Not engaged (fence not emittable on this host) → advisory, honestly.
    assert_eq!(net_posture(&caveats, false, false), NetPosture::Advisory);
    // An approved private HTTP origin is one pinned gate, without a proxy.
    assert_eq!(net_posture(&caveats, false, true), NetPosture::Gated(1));
}

#[test]
fn all_and_deny_all_are_advisory_when_unproxied() {
    // `net: All` never warrants a proxy.
    assert_eq!(
        net_posture(&Caveats::top(), false, false),
        NetPosture::Advisory
    );
    let deny = Caveats {
        net: Scope::only([] as [String; 0]),
        ..Caveats::top()
    };
    assert_eq!(net_posture(&deny, false, false), NetPosture::Advisory);
}
