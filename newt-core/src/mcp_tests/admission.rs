use super::*;

#[test]
fn admit_denies_untrusted_and_disabled_admits_trusted() {
    // step-1.1: enabled != trusted != approved, decided at ONE gate.
    // A trusted, enabled server is admitted (its witness carries the entry).
    let trusted = stdio("trusted", "/bin/echo");
    let ok = admit(&trusted).unwrap();
    assert_eq!(ok.entry().name, "trusted");

    // A discovered (untrusted) STDIO overlay is refused — it may not spawn
    // without approval outside the repo; headless has no interactive path.
    let untrusted = McpServerEntry {
        trust: McpTrust::Untrusted,
        ..stdio("evil", "/bin/sh")
    };
    assert!(matches!(
        admit(&untrusted),
        Err(AdmissionDenied::UntrustedNotApproved)
    ));

    // Transport-agnostic: an untrusted HTTP overlay is refused too.
    let untrusted_http = McpServerEntry {
        trust: McpTrust::Untrusted,
        transport: TransportKind::Http,
        command: None,
        url: Some("https://evil.example".into()),
        ..stdio("evil-http", "")
    };
    assert!(matches!(
        admit(&untrusted_http),
        Err(AdmissionDenied::UntrustedNotApproved)
    ));

    // A disabled entry is never admitted, regardless of trust (enabled is a
    // visibility switch, not a trust decision).
    let disabled = McpServerEntry {
        enabled: false,
        ..stdio("off", "/bin/echo")
    };
    assert!(matches!(admit(&disabled), Err(AdmissionDenied::Disabled)));
}
