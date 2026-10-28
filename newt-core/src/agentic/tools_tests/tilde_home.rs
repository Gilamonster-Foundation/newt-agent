//! #3039: a leading `~` in an fs tool path is the home directory, never a
//! workspace-relative literal — and expanding it grants nothing.
use super::*;
use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};

/// Read authority over `roots` only; no write, exec or net.
fn read_only(roots: &[&std::path::Path]) -> Caveats {
    Caveats {
        fs_read: Scope::only(roots.iter().map(|r| r.to_string_lossy().into_owned())),
        fs_write: Scope::none(),
        exec: Scope::none(),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

/// `~/x` reads `$HOME/x`. On main it read `<workspace>/~/x` and reported the
/// file missing — the wasted round-trip the issue records.
#[tokio::test]
async fn tilde_slash_resolves_under_home() {
    let _lock = env_lock().await;
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("notes.txt"), "from home").unwrap();
    let _home = EnvVar::set("HOME", &home.path().to_string_lossy());
    let caveats = read_only(&[ws.path(), home.path()]);
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "~/notes.txt"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("from home"), "{out}");
}

/// Expansion yields an ordinary absolute path, so the fs fence judges it
/// exactly as if the model had typed it: home outside the read roots is still
/// refused, and the refusal names the expanded target.
#[tokio::test]
async fn tilde_outside_read_roots_is_still_refused() {
    let _lock = env_lock().await;
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("secret.txt"), "secret").unwrap();
    let _home = EnvVar::set("HOME", &home.path().to_string_lossy());
    let caveats = read_only(&[ws.path()]);
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "~/secret.txt"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(!out.contains("secret\n") && out != "secret", "{out}");
    assert!(out.contains("capability denied"), "{out}");
    assert!(
        out.contains(
            &home
                .path()
                .join("secret.txt")
                .to_string_lossy()
                .into_owned()
        ),
        "{out}"
    );
}

/// `~user` is not looked up and not treated as a workspace literal: the model
/// gets one clear error naming the rule, instead of a false "file missing".
#[tokio::test]
async fn tilde_user_form_is_a_clear_error() {
    let _lock = env_lock().await;
    let ws = tempfile::tempdir().unwrap();
    let caveats = read_only(&[ws.path()]);
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "~someone/x"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("~user") && out.contains("#3039"), "{out}");
}

/// A missing path reports both what was written and where it resolved.
#[tokio::test]
async fn missing_tilde_path_names_written_and_resolved() {
    let _lock = env_lock().await;
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _home = EnvVar::set("HOME", &home.path().to_string_lossy());
    let caveats = read_only(&[ws.path(), home.path()]);
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "~/absent.toml"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    let resolved = dunce::simplified(&home.path().join("absent.toml"))
        .display()
        .to_string();
    assert!(out.contains("~/absent.toml"), "{out}");
    assert!(out.contains(&format!("resolved to `{resolved}`")), "{out}");
}

/// #3045 review: `~//<abs>` strips every separator after `~`, so the rest
/// stays under home. Stripping only one left `/<abs>`, which `join` let
/// replace home: read_file and write_file then acted on a different file,
/// granted but not the one written, and reported success.
#[tokio::test]
async fn repeated_separators_after_tilde_stay_under_home() {
    let _lock = env_lock().await;
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let _home = EnvVar::set("HOME", &home.path().to_string_lossy());
    let other_file = other.path().join("marker.txt");
    let suffix = other_file.strip_prefix("/").unwrap();
    let home_file = home.path().join(suffix);
    std::fs::create_dir_all(home_file.parent().unwrap()).unwrap();
    std::fs::write(&home_file, "home copy").unwrap();
    std::fs::write(&other_file, "other copy").unwrap();
    let roots = [ws.path(), home.path(), other.path()];
    let scope = Scope::only(roots.iter().map(|r| r.to_string_lossy().into_owned()));
    let caveats = Caveats {
        fs_write: scope.clone(),
        ..read_only(&roots)
    };
    let path = format!("~/{}", other_file.display());
    assert!(path.starts_with("~//"), "{path}");
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": path}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(
        out.contains("home copy") && !out.contains("other copy"),
        "{out}"
    );
    run_tool(
        "write_file",
        serde_json::json!({"path": path, "content": "written"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert_eq!(std::fs::read_to_string(&home_file).unwrap(), "written");
    assert_eq!(std::fs::read_to_string(&other_file).unwrap(), "other copy");
}
