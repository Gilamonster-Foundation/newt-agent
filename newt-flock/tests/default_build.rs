//! `newt-flock` is a workspace member (CI and the ratchets see it) but not a
//! default member: a plain `cargo build` at the root does not build it, and it
//! is built on request with `-p newt-flock`. Its dependencies are git-only
//! until the agent-mesh flock crates are on crates.io.

use std::process::Command;

#[test]
fn it_is_a_member_but_not_a_default_member() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let out = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--no-deps",
            "--offline",
            "--format-version",
            "1",
        ])
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids = |key: &str| -> Vec<String> {
        meta[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    };
    let is_flock = |id: &String| id.contains("/newt-flock#");
    let (members, defaults) = (ids("workspace_members"), ids("workspace_default_members"));
    assert!(members.iter().any(is_flock), "still a workspace member");
    assert!(!defaults.iter().any(is_flock), "{defaults:?}");
    // the default set is every other member: nothing else fell out of it
    assert_eq!(defaults.len() + 1, members.len());
}

/// The library as a consumer gets it: no dev-dependencies, so none of the
/// writer features the test fixture turns on (`serve` through feature
/// unification) are available. `cargo test` alone cannot see a production
/// build break, because the fixture's dev-dependency enables them.
#[test]
fn the_library_builds_with_production_features_only() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let out = Command::new(env!("CARGO"))
        .args(["check", "--offline", "--locked", "--lib", "--manifest-path"])
        .arg(manifest)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "newt-flock --lib (no dev-dependencies) does not build:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
