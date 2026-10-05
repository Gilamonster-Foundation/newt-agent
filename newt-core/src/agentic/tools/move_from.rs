//! Checked, deliberately narrow Rust free-function extraction (#2724).
mod extract;
mod files;
mod library;
mod transaction;

use super::{PermissionDecision, PermissionGate};
use crate::caveats::Caveats;
use crate::confined_exec::{build_tool_request, ConfinedOutput, ConstrainedExecutor, ExecRequest};
use files::DiskFiles;
use std::path::{Component, Path, PathBuf};

pub(super) struct Moved {
    pub source: String,
    pub before: String,
    pub parent: String,
    pub child: String,
}

pub(super) fn execute(
    args: &serde_json::Value,
    workspace: &str,
    caveats: &Caveats,
    harness: Option<&super::super::smart_harness::SmartHarness>,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<Moved, String> {
    if args.get("copy_from").is_some() || args["content"].as_str() != Some("") {
        return Err("move_from requires empty content and no copy_from".into());
    }
    let spec = &args["move_from"];
    let source_label = spec["path"]
        .as_str()
        .ok_or("move_from needs path and items")?;
    let names: Vec<String> = serde_json::from_value(spec["items"].clone())
        .map_err(|_| "move_from items must be an array of function names")?;
    let launch = Path::new(workspace)
        .canonicalize()
        .map_err(|e| format!("workspace: {e}"))?;
    let source = launch.join(source_label);
    let source_parent = source.parent().ok_or("source has no parent")?;
    // Locate the owning package before selecting its fence: with fs_write=All,
    // the shared selector deliberately fences just the requested build cwd.
    // Manifest contents are read only after root admission and fs_read checks.
    let cwd = source_parent
        .ancestors()
        .find(|dir| dir.join("Cargo.toml").is_file())
        .unwrap_or(source_parent);
    let (root, _) = super::build_shell::build_directory(workspace, cwd, caveats)
        .map_err(|(reason, _)| reason)?;
    let source = local_path(&root, &source.to_string_lossy())?;
    let child = launch.join(args["path"].as_str().ok_or("missing destination path")?);
    let child = local_path(&root, &child.to_string_lossy())?;
    let module = child
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("invalid child filename")?;
    let source_parent = source.parent().ok_or("source has no parent")?;
    let (crate_dir, package, library) = owning_crate(&root, source_parent, caveats)?;
    library::source_in_library(&library, &source, caveats)?;
    let module_dir = if source == library {
        source_parent.to_owned()
    } else {
        match source.file_name().and_then(|s| s.to_str()) {
            Some("mod.rs") => source_parent.to_path_buf(),
            _ => source.with_extension(""),
        }
    };
    if source.extension().is_none_or(|e| e != "rs")
        || child != module_dir.join(format!("{module}.rs"))
    {
        return Err("destination must be a new .rs child module beside lib.rs/mod.rs, or under the source file's stem directory".into());
    }
    if source.canonicalize().map_err(|e| e.to_string())? != source {
        return Err("symlinked source paths are not supported".into());
    }
    for (scope, axis, path) in [
        (&caveats.fs_read, "fs_read", source.as_path()),
        (&caveats.fs_read, "fs_read", child.as_path()),
        // Atomic publication stages in these directories. Exact-file write
        // grants cannot implicitly grant sibling staging paths.
        (&caveats.fs_write, "fs_write", source_parent),
        (&caveats.fs_write, "fs_write", module_dir.as_path()),
    ] {
        if !super::tui_permits_path(scope, &path.to_string_lossy()) {
            return Err(super::denied_fs_result(axis, &path.to_string_lossy()));
        }
    }
    let before = super::file_capture::read_for_edit(&caveats.fs_read, &source, source_label)?;
    let after = extract::extract(&before, &names, module)?;
    let request = check_request(&root, &crate_dir, &package, caveats);
    if let Some(harness) = harness {
        harness
            .validate_tool_authority(request.caveats(), &root)
            .map_err(|e| format!("frame isolation: {e}"))?;
    }
    if !request.caveats().leq(caveats) {
        let permission = super::lifecycle_build_request(
            &root.to_string_lossy(),
            &format!("cargo check -p {package} --lib (before and after move_from)"),
            request.caveats(),
            None,
        );
        if !gate.as_deref_mut().is_some_and(|g| {
            matches!(
                g.ask_with_caveats(request.caveats(), &[permission]),
                PermissionDecision::Allow(allowed) if request.caveats().leq(&allowed)
            )
        }) {
            return Err("capability denied: move_from needs explicit confined build authority; nothing changed".into());
        }
    }
    if !super::confirm_unrestricted_fs_mutation(
        caveats,
        gate,
        "Move these functions into a checked child module? [y/N]",
    ) {
        return Err("user declined extraction; nothing changed".into());
    }
    let files = DiskFiles::open(&root, &source, &child)?;
    transaction::run(&files, &before, &after, || check(&request))?;
    Ok(Moved {
        source: source_label.into(),
        before,
        parent: after.source,
        child: after.child,
    })
}

fn local_path(root: &Path, label: &str) -> Result<PathBuf, String> {
    if label.is_empty() {
        return Err("empty path".into());
    }
    let full = root.join(label);
    let rel = full
        .strip_prefix(root)
        .map_err(|_| "move_from paths must remain inside the selected build root")?;
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err("move_from paths must be normalized paths without '..'".into());
    }
    // Check every existing ancestor; a not-yet-created module directory is OK.
    for ancestor in full.parent().into_iter().flat_map(Path::ancestors) {
        if ancestor == root {
            break;
        }
        match ancestor.canonicalize() {
            Ok(real) if real != ancestor => {
                return Err("symlinked parent paths are not supported".into())
            }
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(full)
}

fn owning_crate(
    root: &Path,
    start: &Path,
    caveats: &Caveats,
) -> Result<(PathBuf, String, PathBuf), String> {
    for dir in start.ancestors().take_while(|p| p.starts_with(root)) {
        let manifest = dir.join("Cargo.toml");
        if !manifest.try_exists().map_err(|e| e.to_string())? {
            continue;
        }
        let text = library::read_source(&manifest, caveats)?;
        let parsed: toml::Value = toml::from_str(&text).map_err(|e| format!("Cargo.toml: {e}"))?;
        let name = parsed
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
            .ok_or("nearest Cargo.toml must name a package with a library target")?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        {
            return Err("invalid package name".into());
        }
        let library_path = parsed
            .get("lib")
            .and_then(|v| v.get("path"))
            .and_then(toml::Value::as_str)
            .unwrap_or("src/lib.rs");
        let library = local_path(dir, library_path)?;
        return Ok((dir.to_owned(), name.into(), library));
    }
    Err("no owning Cargo package inside selected build root; nothing changed".into())
}

fn check_request(root: &Path, cwd: &Path, package: &str, caveats: &Caveats) -> ExecRequest {
    build_tool_request(
        root,
        cwd,
        "cargo",
        ["check", "-p", package, "--lib"],
        &caveats.net,
    )
    .timeout(super::shell::LIFECYCLE_BUILD_TIMEOUT)
    .env("CARGO_BUILD_JOBS", "4")
    .env("RUSTC_WRAPPER", "")
}

fn check(request: &ExecRequest) -> Result<(), String> {
    match ConstrainedExecutor::run(request) {
        Ok(out) => checked_output(&out),
        Err(e) => Err(format!(
            "confined cargo check unavailable: {}",
            bounded(&e.to_string())
        )),
    }
}
fn checked_output(out: &ConfinedOutput) -> Result<(), String> {
    if out.success
        && out.code == Some(0)
        && !out.timed_out
        && out.sandbox_kind != agent_bridle::SandboxKind::None
    {
        Ok(())
    } else {
        Err(format!(
            "cargo check failed (exit {:?}, timed_out={}, sandbox={:?}): {}",
            out.code,
            out.timed_out,
            out.sandbox_kind,
            bounded(&format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout)
            ))
        ))
    }
}
fn bounded(text: &str) -> String {
    const LIMIT: usize = 6000;
    let mut kept: String = text.chars().take(LIMIT).collect();
    if text.chars().count() > LIMIT {
        kept.push_str("\n[diagnostics truncated]");
    }
    kept
}

#[cfg(test)]
mod tests;
