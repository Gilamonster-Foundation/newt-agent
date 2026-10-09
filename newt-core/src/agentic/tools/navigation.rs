//! Navigation reads use the same admitted roots as file tools. Live traversal
//! retains directory capabilities; cached aggregate indices require their
//! entire source workspace, rather than filtering already-derived answers.

use super::*;
use crate::caveats::lexically_normalize;
use crate::navigator::NavToolCtx;
use crate::{Caveats, Scope};
use std::path::{Component, Path, PathBuf};

pub(super) fn execute(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    caveats: &Caveats,
    ctx: &NavToolCtx<'_>,
) -> String {
    if lexically_normalize(ctx.workspace) != lexically_normalize(workspace) {
        return denied_fs_result("fs_read", ctx.workspace);
    }
    if name == "text_search" {
        let relative = match relative_path(args["path"].as_str().unwrap_or(".")) {
            Ok(relative) => relative,
            Err(error) => return error,
        };
        let full = Path::new(workspace).join(relative);
        if !tui_permits_path(&caveats.fs_read, &full.to_string_lossy()) {
            return denied_fs_result("fs_read", &full.to_string_lossy());
        }
        return search(args, workspace, caveats, ctx);
    }
    // Indices are derived from the whole gathered workspace. A file/subtree
    // grant cannot authorize cross-file facts or cached source snippets.
    if !tui_permits_path(&caveats.fs_read, workspace) {
        return denied_fs_result("fs_read", workspace);
    }
    if name == "impact" {
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        if matches!(caveats.fs_read, Scope::All) {
            // Existing unrestricted read authority covers the legacy backend.
            // Scoped grants still require object-bound reads below.
            return crate::navigator::execute_nav_tool(name, args, ctx)
                .unwrap_or_else(|| format!("unknown tool: {name}"));
        }
        return impact(args, workspace, ctx);
    }
    if !matches!(caveats.fs_read, Scope::All)
        && (!cfg!(any(target_os = "linux", target_os = "macos")) || ctx.files.is_none())
    {
        return "capability denied: fs_read cannot establish a bounded workspace corpus for this navigation index".into();
    }
    crate::navigator::execute_nav_tool(name, args, ctx)
        .unwrap_or_else(|| format!("unknown tool: {name}"))
}

fn relative_path(path: &str) -> Result<PathBuf, String> {
    let path = path.trim();
    let path = if path.is_empty() { "." } else { path };
    let relative = Path::new(path);
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(denied_fs_result("fs_read", path));
    }
    Ok(relative.to_path_buf())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod bounded {
    use super::*;
    use crate::fs_cap::WorkspaceDir;
    use crate::navigator::{NavResult, TextHit, MAX_HITS};
    use std::io::Read;
    use std::os::unix::fs::MetadataExt as _;

    enum SearchRoot {
        Directory(WorkspaceDir, PathBuf),
        File(std::fs::File),
    }

    fn read_error(path: &Path, error: &std::io::Error) -> String {
        if is_fs_containment_denied(error) {
            denied_fs_result("fs_read", &path.to_string_lossy())
        } else {
            format!("error: reading {}: {error}", path.display())
        }
    }

    // Intersect the admitted root with the workspace, retaining the narrower
    // descriptor. The lexical test is only admission; every subsequent open is
    // beneath that descriptor, including every recursive child and symlink.
    fn search_root(
        workspace: &str,
        caveats: &Caveats,
        relative: &Path,
    ) -> Result<SearchRoot, String> {
        let full = Path::new(workspace).join(relative);
        let full_str = full.to_string_lossy();
        let grant = authorizing_root(&caveats.fs_read, &full_str)
            .ok_or_else(|| denied_fs_result("fs_read", &full_str))?;
        let directory = WorkspaceDir::open_root(Path::new(workspace))
            .map_err(|error| read_error(Path::new(workspace), &error))?;
        let Some(grant) = grant
            .filter(|root| lexically_normalize(root).starts_with(lexically_normalize(workspace)))
        else {
            return Ok(SearchRoot::Directory(directory, relative.to_path_buf()));
        };
        let grant_relative = contained_relative(grant, workspace);
        match directory.open_dir(&grant_relative) {
            Ok(root) => Ok(SearchRoot::Directory(
                root,
                contained_relative(&full_str, grant),
            )),
            Err(error) if lexically_normalize(grant) == lexically_normalize(&full_str) => {
                // An exact-file grant exposes this file, never its parent or a
                // final link to another file. Use the opened object for reads.
                directory
                    .open_regular(&grant_relative, true)
                    .map(SearchRoot::File)
                    .map_err(|file_error| {
                        read_error(
                            relative,
                            if is_fs_containment_denied(&error) {
                                &error
                            } else {
                                &file_error
                            },
                        )
                    })
            }
            Err(error) => Err(read_error(relative, &error)),
        }
    }

    fn read_contents(mut file: std::fs::File, path: &Path) -> Result<String, String> {
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|error| read_error(path, &error))?;
        Ok(text)
    }

    fn append_file(
        file: std::fs::File,
        relative: &Path,
        re: &regex::Regex,
        hits: &mut Vec<TextHit>,
    ) -> Result<(), String> {
        let content = read_contents(file, relative)?;
        crate::navigator::search_contents(
            &relative.to_string_lossy().replace('\\', "/"),
            &content,
            re,
            hits,
        );
        Ok(())
    }

    fn walk(
        directory: &WorkspaceDir,
        relative: &Path,
        re: &regex::Regex,
        hits: &mut Vec<TextHit>,
        warnings: &mut Vec<String>,
        visited: &mut std::collections::HashSet<(u64, u64)>,
    ) -> Result<(), String> {
        let identity = directory
            .open(Path::new("."))
            .and_then(|file| file.metadata())
            .map_err(|error| read_error(relative, &error))?;
        if !visited.insert((identity.dev(), identity.ino())) {
            return Ok(());
        }
        let mut entries = directory
            .read_dir(Path::new("."))
            .map_err(|error| read_error(relative, &error))?;
        entries.sort();
        for name in entries {
            if hits.len() >= MAX_HITS {
                break;
            }
            let text = name.to_string_lossy();
            if text.starts_with('.') || text == "target" || text == "node_modules" {
                continue;
            }
            let child = relative.join(&name);
            let result = match directory.open_dir(Path::new(&name)) {
                Ok(subdirectory) => walk(&subdirectory, &child, re, hits, warnings, visited),
                Err(directory_error) => match directory.open_regular(Path::new(&name), false) {
                    Ok(file) => append_file(file, &child, re, hits),
                    Err(file_error) => Err(read_error(
                        &child,
                        if is_fs_containment_denied(&directory_error) {
                            &directory_error
                        } else {
                            &file_error
                        },
                    )),
                },
            };
            if let Err(error) = result {
                // Keep useful admitted hits, but do not call a cut search complete.
                if warnings.len() < 8 {
                    warnings.push(error);
                }
            }
        }
        Ok(())
    }

    pub(super) fn search(
        args: &serde_json::Value,
        workspace: &str,
        caveats: &Caveats,
        ctx: &NavToolCtx<'_>,
    ) -> String {
        let requested = args["path"].as_str().unwrap_or(".").trim();
        let relative = match relative_path(requested) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let root = match search_root(workspace, caveats, &relative) {
            Ok(root) => root,
            Err(error) => return error,
        };
        let query = args["query"].as_str().unwrap_or("").trim();
        if query.is_empty() {
            return "error: text_search requires a non-empty query".into();
        }
        let re = match regex::Regex::new(query) {
            Ok(re) => re,
            Err(error) => return format!("error: invalid regex: {error}"),
        };
        let mut hits = Vec::new();
        let mut result = NavResult::empty(
            crate::agentic::semantic::EvidenceKind::Lexical,
            "lexical-regex",
            crate::navigator::index_id(ctx),
        );
        let searched = match root {
            SearchRoot::File(file) => append_file(file, &relative, &re, &mut hits),
            SearchRoot::Directory(directory, path) => match directory.open_dir(&path) {
                Ok(root) => walk(
                    &root,
                    &relative,
                    &re,
                    &mut hits,
                    &mut result.warnings,
                    &mut Default::default(),
                ),
                Err(directory_error) => match directory.open_regular(&path, false) {
                    Ok(file) => append_file(file, &relative, &re, &mut hits),
                    Err(file_error) => Err(read_error(
                        &relative,
                        if is_fs_containment_denied(&directory_error) {
                            &directory_error
                        } else {
                            &file_error
                        },
                    )),
                },
            },
        };
        if let Err(error) = searched {
            result.warnings.push(error);
        }
        if !result.warnings.is_empty() {
            result.complete = false;
        }
        crate::navigator::finish_text_search(query, &re, hits, result)
            .paged(
                args["page"].as_u64().map(|page| page as usize),
                crate::navigator::TEXT_SEARCH_PAGE_SIZE,
            )
            .render()
    }

    fn optional_text(root: &WorkspaceDir, path: &Path) -> Result<Option<String>, String> {
        match root.open_regular(path, false) {
            Ok(file) => read_contents(file, path).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(read_error(path, &error)),
        }
    }

    fn project(root: &WorkspaceDir) -> Result<Option<crate::project_model::ProjectModel>, String> {
        use crate::project_model::{builtin_project_packs, derive, dotted_get, parse_marker};
        for pack in builtin_project_packs() {
            for marker in &pack.markers {
                let marker_path = relative_path(marker)?;
                let Some(text) = optional_text(root, &marker_path)? else {
                    continue;
                };
                let Some(value) = parse_marker(pack.format, &text) else {
                    return Ok(None);
                };
                let members = pack
                    .workspace_members_at
                    .as_deref()
                    .and_then(|key| dotted_get(&value, key))
                    .and_then(serde_json::Value::as_array);
                let mut dirs = Vec::new();
                for member in members
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                {
                    if let Some(prefix) = member.strip_suffix('*') {
                        let path = relative_path(prefix.trim_end_matches('/'))?;
                        let directory = root
                            .open_dir(&path)
                            .map_err(|error| read_error(&path, &error))?;
                        for name in directory
                            .read_dir(Path::new("."))
                            .map_err(|error| read_error(&path, &error))?
                        {
                            match directory.open_dir(Path::new(&name)) {
                                Ok(_) => dirs.push(path.join(name)),
                                Err(error) if is_fs_containment_denied(&error) => {
                                    return Err(read_error(&path.join(name), &error))
                                }
                                Err(_) => {}
                            }
                        }
                    } else {
                        dirs.push(relative_path(member)?);
                    }
                }
                dirs.sort();
                let mut markers = Vec::new();
                if dirs.is_empty() {
                    markers.push((".".into(), value));
                } else {
                    for dir in dirs {
                        if let Some(text) = optional_text(root, &dir.join(&marker_path))? {
                            if let Some(value) = parse_marker(pack.format, &text) {
                                markers.push((dir.to_string_lossy().into_owned(), value));
                            }
                        }
                    }
                }
                return Ok(Some(derive(&pack, &markers)));
            }
        }
        Ok(None)
    }

    pub(super) fn impact(
        args: &serde_json::Value,
        workspace: &str,
        ctx: &NavToolCtx<'_>,
    ) -> String {
        let root = match WorkspaceDir::open_root(Path::new(workspace)) {
            Ok(root) => root,
            Err(error) => return read_error(Path::new(workspace), &error),
        };
        let model = match project(&root) {
            Ok(Some(model)) => model,
            Ok(None) => {
                return "error: impact unavailable: no admitted project marker in this workspace"
                    .into()
            }
            Err(error) => return error,
        };
        let coverage = match optional_text(&root, Path::new("lcov.info")) {
            Ok(text) => text,
            Err(error) => return error,
        };
        let unit = args["unit"]
            .as_str()
            .or_else(|| args["symbol"].as_str())
            .unwrap_or("")
            .trim();
        crate::navigator::impact_from_admitted(
            unit,
            &model,
            ctx.files.unwrap_or(&[]),
            coverage.as_deref(),
        )
        .render()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
use bounded::{impact, search};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn search(
    args: &serde_json::Value,
    _workspace: &str,
    caveats: &Caveats,
    ctx: &NavToolCtx<'_>,
) -> String {
    if matches!(caveats.fs_read, Scope::All) {
        // This backend claims no containment: it is used only when every read
        // is already authorized, never as a fallback for a narrower grant.
        return crate::navigator::execute_nav_tool("text_search", args, ctx)
            .expect("text_search is a registered navigation tool");
    }
    "error: text_search unavailable: object-bound navigation traversal is not implemented on this platform".into()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn impact(_args: &serde_json::Value, _workspace: &str, _ctx: &NavToolCtx<'_>) -> String {
    "error: impact unavailable: object-bound project and coverage reads are not implemented on this platform".into()
}
