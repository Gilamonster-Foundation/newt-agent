//! Production Brush route, complementing the rule-install race proof.
#[path = "linked_git_fixture.rs"]
mod fixture_support;

pub async fn run() {
    let (_fixture, _, linked) = fixture_support::fixture();
    newt_core::git_hardening::own_gitdir_grants(&linked);
    let output = newt_core::execute_tool(
        "run_command",
        &serde_json::json!({"command": "git show HEAD:tracked && cargo --version"}),
        linked.to_str().unwrap(),
        false,
        100,
        &newt_core::Caveats::top(),
        &mut newt_core::NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(
        output.contains("original") && output.contains("cargo "),
        "{output}"
    );
    assert!(!output.contains("not a git repository"), "{output}");
    println!("LINKED_BUILD_BRUSH_READS_PASSED");
}
