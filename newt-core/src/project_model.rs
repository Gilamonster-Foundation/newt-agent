//! Project packs + the project model — the "IDE for LLMs" structural spine
//! (#1288, epic #1277; spec `docs/spec/ide-project-model.md`).
//!
//! newt detects the project's **build system** and derives a **project model**:
//! a per-unit catalog (crate / package / module → source roots, dependencies,
//! languages). A [`ProjectPack`] is pure DATA — the same three-Cs shape as the
//! [`crate::config::LanguagePack`] — that recognises one build system and says
//! how to read its units. **A new build system is config, not code.**
//!
//! Out-of-the-box: Rust (`Cargo.toml`), Python (`pyproject.toml`), Dart
//! (`pubspec.yaml`), TypeScript (`package.json`). Java / C# / … drop in as
//! `~/.newt/project-packs/*.toml` — no core change.
//!
//! ## Correctness (formal)
//! [`derive`] is a **pure per-unit fold** — it mirrors
//! `formal/ProjectModel/Basic.lean` (`derive p t = fun u => deriveUnit p (t u)`):
//! - **PO-A** determinism — same `(pack, markers)` ⇒ same model
//!   ([`derive_is_deterministic`]);
//! - **PO-E** a non-detecting pack derives the empty model
//!   ([`detect_pack`] returns `None` ⇒ [`scan_project`] yields `None`).
//!
//! The parse/detect are pure over injected data; the fs walk is the thin wrapper
//! [`scan_project`].

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// The marker-file format a pack parses. All three deserialize into a common
/// [`serde_json::Value`], so the locators below are format-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackFormat {
    Toml,
    Yaml,
    Json,
}

/// A project pack: pure data recognising one build system and locating its
/// units. Locators are dotted paths into the parsed marker (`a.b.c`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectPack {
    /// Stable id (`rust`); a config pack with a built-in's name replaces it.
    pub name: String,
    /// Marker file(s) whose presence at the root detects this build system.
    pub markers: Vec<String>,
    /// How to parse a marker.
    pub format: PackFormat,
    /// Dotted path to a list of workspace member dirs/globs
    /// (Cargo `workspace.members`, npm `workspaces`). `None` ⇒ single-unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_members_at: Option<String>,
    /// Dotted path to a unit's name within its marker (`package.name`).
    pub unit_name_at: String,
    /// Dotted path to the dependency table/list (`dependencies`). Table keys or
    /// an array of `name` / `name>=ver` strings are both read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deps_at: Option<String>,
    /// Source roots, unit-dir-relative (`["src"]`).
    #[serde(default)]
    pub source_roots: Vec<String>,
    /// Which [`crate::config::LanguagePack`]s extract this unit's symbols.
    #[serde(default)]
    pub languages: Vec<String>,
}

/// One derived project unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectUnit {
    /// Unit name (from the marker, else the dir's file name).
    pub name: String,
    /// Workspace-relative dir (`"."` for a single-unit project's root).
    pub dir: String,
    /// Source roots, dir-relative.
    pub source_roots: Vec<String>,
    /// Dependency names (version specifiers stripped).
    pub deps: Vec<String>,
    /// Languages extracting this unit's symbols.
    pub languages: Vec<String>,
}

/// The derived project model: which pack detected + its units.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectModel {
    /// The detecting pack's name (empty for the default/empty model).
    pub pack: String,
    /// The units, in discovery order.
    pub units: Vec<ProjectUnit>,
}

/// Navigate a dotted path (`a.b.c`) into a JSON value. `None` at any missing
/// step. An empty path returns the value itself.
#[must_use]
pub fn dotted_get<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    if path.is_empty() {
        return Some(cur);
    }
    for key in path.split('.') {
        cur = cur.get(key)?;
    }
    Some(cur)
}

/// Parse a marker's contents into a [`serde_json::Value`] per `format`. `None`
/// on a parse error (a malformed marker yields no unit, never a panic).
#[must_use]
pub fn parse_marker(format: PackFormat, contents: &str) -> Option<Value> {
    match format {
        PackFormat::Toml => toml::from_str(contents).ok(),
        PackFormat::Yaml => serde_yaml::from_str(contents).ok(),
        PackFormat::Json => serde_json::from_str(contents).ok(),
    }
}

/// Extract dependency names from a value: object keys (Cargo/npm tables), or an
/// array of `name` / `name>=1.2` strings (PEP 621 `dependencies`). Version
/// specifiers and extras are stripped to the bare name.
fn deps_of(v: &Value) -> Vec<String> {
    match v {
        Value::Object(map) => map.keys().cloned().collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(bare_dep_name)
            .collect(),
        _ => Vec::new(),
    }
}

/// The bare package name from a PEP 508-ish requirement: up to the first version
/// operator / extras bracket / whitespace (`requests[socks]>=2.0` → `requests`).
fn bare_dep_name(req: &str) -> String {
    let end = req
        .find(|c: char| "<>=!~[; ".contains(c))
        .unwrap_or(req.len());
    req[..end].trim().to_string()
}

/// Derive one unit from its parsed marker — **pure** (the `deriveUnit` of the
/// Lean model). Reads name/deps per the pack's locators; falls back to the
/// dir's file name when the marker names nothing.
#[must_use]
pub fn derive_unit(pack: &ProjectPack, dir: &str, marker: &Value) -> ProjectUnit {
    let name = dotted_get(marker, &pack.unit_name_at)
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| dir_leaf(dir));
    let deps = pack
        .deps_at
        .as_deref()
        .and_then(|p| dotted_get(marker, p))
        .map(deps_of)
        .unwrap_or_default();
    ProjectUnit {
        name,
        dir: dir.to_string(),
        source_roots: pack.source_roots.clone(),
        deps,
        languages: pack.languages.clone(),
    }
}

/// The final path component of a workspace-relative dir (`"."` → `"."`).
fn dir_leaf(dir: &str) -> String {
    dir.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(dir)
        .to_string()
}

/// Derive the whole model from a detecting pack and its units' parsed markers —
/// **pure**, the per-unit fold `derive p t = fun u => deriveUnit p (t u)`
/// (`formal/ProjectModel/Basic.lean`). `unit_markers` is `(dir, parsed marker)`.
#[must_use]
pub fn derive(pack: &ProjectPack, unit_markers: &[(String, Value)]) -> ProjectModel {
    ProjectModel {
        pack: pack.name.clone(),
        units: unit_markers
            .iter()
            .map(|(dir, marker)| derive_unit(pack, dir, marker))
            .collect(),
    }
}

/// The first pack any of whose markers is present, per an injected presence
/// predicate — **pure** (the fs check lives in [`scan_project`]). `None` ⇒ no
/// build system detected (PO-E: the empty model).
#[must_use]
pub fn detect_pack<'a>(
    packs: &'a [ProjectPack],
    present: &dyn Fn(&str) -> bool,
) -> Option<&'a ProjectPack> {
    packs.iter().find(|p| p.markers.iter().any(|m| present(m)))
}

/// The built-in project packs (Rust, Python, Dart, TypeScript). Pure data —
/// these four define + freeze the plugin shape; further build systems drop in as
/// `~/.newt/project-packs/*.toml`.
#[must_use]
pub fn builtin_project_packs() -> Vec<ProjectPack> {
    vec![
        ProjectPack {
            name: "rust".into(),
            markers: vec!["Cargo.toml".into()],
            format: PackFormat::Toml,
            workspace_members_at: Some("workspace.members".into()),
            unit_name_at: "package.name".into(),
            deps_at: Some("dependencies".into()),
            source_roots: vec!["src".into()],
            languages: vec!["rust".into()],
        },
        ProjectPack {
            name: "python".into(),
            markers: vec!["pyproject.toml".into()],
            format: PackFormat::Toml,
            workspace_members_at: None,
            unit_name_at: "project.name".into(),
            deps_at: Some("project.dependencies".into()),
            source_roots: vec!["src".into()],
            languages: vec!["python".into()],
        },
        ProjectPack {
            name: "dart".into(),
            markers: vec!["pubspec.yaml".into()],
            format: PackFormat::Yaml,
            workspace_members_at: None,
            unit_name_at: "name".into(),
            deps_at: Some("dependencies".into()),
            source_roots: vec!["lib".into()],
            languages: vec!["dart".into()],
        },
        ProjectPack {
            name: "typescript".into(),
            markers: vec!["package.json".into()],
            format: PackFormat::Json,
            workspace_members_at: Some("workspaces".into()),
            unit_name_at: "name".into(),
            deps_at: Some("dependencies".into()),
            source_roots: vec!["src".into()],
            languages: vec!["typescript".into()],
        },
    ]
}

/// Merge project-pack layers **by name** (later layers win) — the SC-L1 fold,
/// mirroring [`crate::api_surface::merge_packs`]. Order within a layer is
/// preserved; a later same-name pack replaces an earlier one in place.
#[must_use]
pub fn merge_project_packs(layers: Vec<Vec<ProjectPack>>) -> Vec<ProjectPack> {
    let mut out: Vec<ProjectPack> = Vec::new();
    for layer in layers {
        for pack in layer {
            if let Some(slot) = out.iter_mut().find(|p| p.name == pack.name) {
                *slot = pack;
            } else {
                out.push(pack);
            }
        }
    }
    out
}

/// Load drop-in project packs from `dir` (`~/.newt/project-packs/` or
/// `.newt/project-packs/`): each `*.toml` is one [`ProjectPack`]. A missing dir
/// or a malformed file is skipped (never fatal). Thin fs wrapper.
#[must_use]
pub fn load_project_packs_from_dir(dir: &Path) -> Vec<ProjectPack> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut packs = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "toml") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(pack) = toml::from_str::<ProjectPack>(&text) {
                    packs.push(pack);
                }
            }
        }
    }
    packs
}

/// Scan `root` and derive its project model — the thin fs wrapper over the pure
/// core. Detects the build system, gathers each unit's marker (the root, plus
/// workspace members when the pack + marker declare them), and folds via
/// [`derive`]. `None` when no pack detects (PO-E) — the caller uses no model.
#[must_use]
pub fn scan_project(root: &Path, packs: &[ProjectPack]) -> Option<ProjectModel> {
    let pack = detect_pack(packs, &|m| root.join(m).is_file())?;
    let marker_file = pack.markers.iter().find(|m| root.join(m).is_file())?;
    let root_text = std::fs::read_to_string(root.join(marker_file)).ok()?;
    let root_val = parse_marker(pack.format, &root_text)?;

    // Workspace members (Cargo `workspace.members`, npm `workspaces`): each
    // member dir carries its own marker of the same name. A missing/malformed
    // member marker is skipped. Absent workspace list ⇒ the root is the unit.
    let members: Vec<String> = pack
        .workspace_members_at
        .as_deref()
        .and_then(|p| dotted_get(&root_val, p))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .flat_map(|g| expand_member_glob(root, g))
                .collect()
        })
        .unwrap_or_default();

    let mut unit_markers: Vec<(String, Value)> = Vec::new();
    if members.is_empty() {
        unit_markers.push((".".to_string(), root_val));
    } else {
        for dir in members {
            let mpath = root.join(&dir).join(marker_file);
            if let Ok(text) = std::fs::read_to_string(&mpath) {
                if let Some(val) = parse_marker(pack.format, &text) {
                    unit_markers.push((dir, val));
                }
            }
        }
    }
    Some(derive(pack, &unit_markers))
}

/// Expand one workspace-member entry to concrete dirs: a literal dir, or a
/// single trailing `*` glob (`crates/*`) matched against `root`'s subdirs.
/// Deeper globs are out of scope (the first cut); returns sorted dirs.
fn expand_member_glob(root: &Path, entry: &str) -> Vec<String> {
    if let Some(prefix) = entry
        .strip_suffix("/*")
        .or_else(|| entry.strip_suffix('*').map(|s| s.trim_end_matches('/')))
    {
        let base = root.join(prefix);
        let Ok(rd) = std::fs::read_dir(&base) else {
            return Vec::new();
        };
        let mut dirs: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(|n| format!("{prefix}/{n}")))
            .collect();
        dirs.sort();
        dirs
    } else {
        vec![entry.to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rust() -> ProjectPack {
        builtin_project_packs()
            .into_iter()
            .find(|p| p.name == "rust")
            .unwrap()
    }

    #[test]
    fn dotted_get_navigates_and_misses() {
        let v = json!({"a": {"b": {"c": 1}}});
        assert_eq!(dotted_get(&v, "a.b.c"), Some(&json!(1)));
        assert_eq!(dotted_get(&v, "a.x"), None);
        assert_eq!(dotted_get(&v, ""), Some(&v));
    }

    #[test]
    fn derive_unit_reads_name_and_deps_else_falls_back() {
        let p = rust();
        let marker =
            json!({"package": {"name": "newt-core"}, "dependencies": {"serde": "1", "regex": "1"}});
        let u = derive_unit(&p, "newt-core", &marker);
        assert_eq!(u.name, "newt-core");
        assert_eq!(u.source_roots, vec!["src".to_string()]);
        assert_eq!(u.languages, vec!["rust".to_string()]);
        let mut deps = u.deps.clone();
        deps.sort();
        assert_eq!(deps, vec!["regex".to_string(), "serde".to_string()]);
        // No name in the marker → the dir leaf.
        let u2 = derive_unit(&p, "crates/foo", &json!({}));
        assert_eq!(u2.name, "foo");
    }

    #[test]
    fn python_deps_strip_version_specifiers() {
        let p = builtin_project_packs()
            .into_iter()
            .find(|p| p.name == "python")
            .unwrap();
        let marker = json!({"project": {"name": "app", "dependencies": ["requests>=2.0", "rich", "httpx[cli]==0.27"]}});
        let u = derive_unit(&p, ".", &marker);
        assert_eq!(u.name, "app");
        assert_eq!(
            u.deps,
            vec![
                "requests".to_string(),
                "rich".to_string(),
                "httpx".to_string()
            ]
        );
    }

    #[test]
    fn parse_marker_handles_all_three_formats() {
        assert_eq!(
            parse_marker(PackFormat::Toml, "[package]\nname='x'\n").unwrap()["package"]["name"],
            json!("x")
        );
        assert_eq!(
            parse_marker(PackFormat::Yaml, "name: y\n").unwrap()["name"],
            json!("y")
        );
        assert_eq!(
            parse_marker(PackFormat::Json, "{\"name\":\"z\"}").unwrap()["name"],
            json!("z")
        );
        assert!(parse_marker(PackFormat::Toml, "not = = valid").is_none());
    }

    #[test]
    fn derive_is_deterministic() {
        // PO-A: same (pack, markers) ⇒ same model.
        let p = rust();
        let ms = vec![
            ("a".to_string(), json!({"package": {"name": "a"}})),
            ("b".to_string(), json!({"package": {"name": "b"}})),
        ];
        assert_eq!(derive(&p, &ms), derive(&p, &ms));
        assert_eq!(derive(&p, &ms).units.len(), 2);
        assert_eq!(derive(&p, &ms).pack, "rust");
    }

    #[test]
    fn detect_pack_matches_marker_or_none() {
        // PO-E basis: no marker present ⇒ None ⇒ the empty model.
        let packs = builtin_project_packs();
        assert_eq!(
            detect_pack(&packs, &|m| m == "Cargo.toml").map(|p| p.name.as_str()),
            Some("rust")
        );
        assert_eq!(
            detect_pack(&packs, &|m| m == "pubspec.yaml").map(|p| p.name.as_str()),
            Some("dart")
        );
        assert!(detect_pack(&packs, &|_| false).is_none());
    }

    #[test]
    fn merge_project_packs_is_by_name_later_wins() {
        let base = builtin_project_packs();
        let override_rust = ProjectPack {
            source_roots: vec!["lib".into()],
            ..rust()
        };
        let merged = merge_project_packs(vec![base.clone(), vec![override_rust]]);
        assert_eq!(merged.len(), base.len(), "override replaces, not appends");
        let r = merged.iter().find(|p| p.name == "rust").unwrap();
        assert_eq!(r.source_roots, vec!["lib".to_string()]);
    }

    #[test]
    fn all_four_targets_present_and_distinct_markers() {
        let names: Vec<_> = builtin_project_packs()
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(names, vec!["rust", "python", "dart", "typescript"]);
    }
}
