use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn short_commit(value: &str) -> Option<String> {
    let value = value.trim();
    (value.len() >= 7 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value[..value.len().min(12)].to_string())
}

fn packaged_commit(manifest_dir: &Path) -> Option<String> {
    let metadata = fs::read_to_string(manifest_dir.join(".cargo_vcs_info.json")).ok()?;
    let after_key = metadata.split_once("\"sha1\"")?.1;
    let after_colon = after_key.split_once(':')?.1.trim_start();
    let value = after_colon.strip_prefix('"')?.split_once('"')?.0;
    short_commit(value)
}

/// Watch `path` only when it exists. Cargo treats a missing watched path as
/// changed on every run, which rebuilt this crate on every invocation.
fn watch(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=NEWT_BUILD_GIT_COMMIT");

    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .expect("Cargo sets CARGO_MANIFEST_DIR for build scripts");
    watch(&manifest_dir.join(".cargo_vcs_info.json"));
    let repo = manifest_dir.parent().unwrap_or(&manifest_dir);

    // The commit moves with HEAD and its reflog; `-dirty` moves with the index
    // and with any tracked top-level entry (Cargo scans a watched directory).
    // `--git-path` resolves each name for a linked worktree too.
    for name in ["HEAD", "logs/HEAD", "index"] {
        if let Some(path) = git(repo, &["rev-parse", "--git-path", name]) {
            watch(&repo.join(path));
        }
    }
    if let Some(tracked) = git(repo, &["ls-tree", "--name-only", "HEAD"]) {
        tracked.lines().for_each(|entry| watch(&repo.join(entry)));
    }

    let supplied_commit = env::var("NEWT_BUILD_GIT_COMMIT")
        .ok()
        .and_then(|value| short_commit(&value));
    let commit = supplied_commit
        .clone()
        .or_else(|| {
            git(repo, &["rev-parse", "--short=12", "HEAD"])
                .as_deref()
                .and_then(short_commit)
        })
        .or_else(|| packaged_commit(&manifest_dir));
    let commit = commit.unwrap_or_else(|| "unknown".to_string());
    let dirty = supplied_commit.is_none()
        && git(
            repo,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain",
                "--untracked-files=normal",
            ],
        )
        .is_some_and(|status| !status.is_empty());
    let source_id = if dirty {
        format!("{commit}-dirty")
    } else {
        commit.clone()
    };
    let package_version =
        env::var("CARGO_PKG_VERSION").expect("Cargo sets CARGO_PKG_VERSION for build scripts");

    println!("cargo:rustc-env=NEWT_BUILD_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=NEWT_BUILD_SOURCE_ID={source_id}");
    println!("cargo:rustc-env=NEWT_BUILD_VERSION={package_version} ({source_id})");
}
