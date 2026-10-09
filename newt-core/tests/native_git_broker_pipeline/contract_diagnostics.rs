//! Publication diagnostics must not mistake a denied optional Xcode cache for
//! failed Git work, or hide any other permission/cleanup failure (#2813).
pub(super) fn clean(out: &str) -> bool {
    out.lines().all(|line| {
        // Native run 37784903122/job/113337028114 produced the attributed
        // commit, payload and task ref despite four such cache denials; its
        // separate packed-refs cleanup error MUST still fail. The required
        // publication row also checks shell completion and these outcomes.
        let cache_denial = line
            .strip_prefix("git: error: couldn't create cache file '/tmp/xcrun_db-")
            .and_then(|s| s.strip_suffix("' (errno=Operation not permitted)"))
            .is_some_and(|name| name.len() == 8 && name.bytes().all(|c| c.is_ascii_alphanumeric()));
        cache_denial
            || ![
                "packed-refs.lock",
                "Operation not permitted",
                "Permission denied",
                "error:",
            ]
            .iter()
            .any(|error| line.contains(error))
    })
}

pub(super) fn regression() {
    let cache = "git: error: couldn't create cache file '/tmp/xcrun_db-ed8lVv35' (errno=Operation not permitted)";
    assert!(
        clean(&format!("commit completed\n{cache}\n")),
        "nonfatal Xcode cache denial"
    );
    for bad in [
        cache.replace("ed8lVv35", ""),
        cache.replace("ed8lVv35", "../other"),
        cache.replace("/tmp/", "/private/tmp/"),
        cache.replace("xcrun_db-", "other-"),
        cache.replace("Operation not permitted", "Permission denied"),
        format!("prefix {cache}"),
        format!("{cache} suffix"),
        format!("{cache}\nerror: commit failed"),
        format!("{cache}\nPermission denied"),
        format!("{cache}\nerror: Unable to create 'packed-refs.lock': Operation not permitted"),
    ] {
        assert!(!clean(&bad), "unexpected diagnostic accepted: {bad}");
    }
}

#[test]
fn exact_cache_diagnostic_only() {
    regression();
}
