//! `newt-flock` is a workspace member (CI and the ratchets see it) but not a
//! default member: a plain `cargo build` at the root does not build it, and it
//! is built on request with `-p newt-flock`. Its dependencies are git-only
//! until the agent-mesh flock crates are on crates.io.
//!
//! Reads the root manifest at compile time: no subprocess, no filesystem. The
//! production-features library check (`cargo check -p newt-flock --lib`) is a
//! CI build step in ci.yml, not a test.

const ROOT: &str = include_str!("../../Cargo.toml");

/// The entries of the `[workspace]` array that starts with `key = [`.
fn array(key: &str) -> Vec<&'static str> {
    let start = ROOT
        .find(&format!("\n{key} = ["))
        .unwrap_or_else(|| panic!("`{key}` moved out of the root manifest"))
        + key.len()
        + 5;
    let body = &ROOT[start..];
    let body = &body[..body.find("\n]").expect("the array ends")];
    body.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('"'))
        .map(|l| l.split('"').nth(1).unwrap())
        .collect()
}

#[test]
fn it_is_a_member_but_not_a_default_member() {
    let (members, defaults) = (array("members"), array("default-members"));
    assert!(members.contains(&"newt-flock"), "still a workspace member");
    assert!(!defaults.contains(&"newt-flock"), "{defaults:?}");
    // the default set is every other member: nothing else fell out of it
    let rest: Vec<_> = members.iter().filter(|m| **m != "newt-flock").collect();
    assert_eq!(defaults.iter().collect::<Vec<_>>(), rest);
}
