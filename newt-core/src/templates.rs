//! **Project templates** (#892) — the DATA sibling of tooling packs (#880).
//!
//! A [`ProjectTemplate`] says how to bring a repo *into existence* already wired
//! for its lifecycle; a [`ToolingPack`](crate::tooling::ToolingPack) says how to
//! *run* the lifecycle of one that already exists. They round-trip: a project
//! scaffolded from the `pyo3` template lays down the exact marker files
//! (`Cargo.toml` + `pyproject.toml`) the `pyo3` pack detects, so its `check` /
//! `test` / `format` phases resolve out of the box.
//!
//! Built-ins cover the operator's favored modes — **Rust + PyO3 (maturin)**,
//! **Python**, **Rust** — kept deliberately minimal-but-buildable. Drop-in
//! `~/.newt/templates/<name>.toml` (a serialized `ProjectTemplate`) overrides a
//! built-in by name or adds a new ecosystem, the same merge model as packs. Pure
//! data + pure render — no filesystem here; the CLI (`newt new`) materializes the
//! rendered `(path, body)` pairs.

use serde::{Deserialize, Serialize};

/// One file a template lays down. `path` is repo-relative (forward slashes).
/// Both `path` and `body` may use the placeholders `{{name}}` (the project name
/// verbatim) and `{{name_snake}}` ([`snake_case`], a legal Rust/Python module
/// identifier) — substituted at [`render`](ProjectTemplate::render) time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateFile {
    /// Repo-relative destination path (forward slashes; may contain `{{name}}`
    /// / `{{name_snake}}`).
    pub path: String,
    /// File contents (may contain `{{name}}` / `{{name_snake}}`).
    pub body: String,
}

/// A project scaffold, as data. `name` is the ecosystem key (`pyo3` / `python` /
/// `rust`) and the drop-in merge key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectTemplate {
    /// Ecosystem key + drop-in merge key. A drop-in with an existing name
    /// overrides the built-in; a new name adds an ecosystem.
    pub name: String,
    /// The files to lay down, in order.
    pub files: Vec<TemplateFile>,
}

impl ProjectTemplate {
    /// Render the template for `project_name`: substitute `{{name}}` and
    /// `{{name_snake}}` in every path and body, returning `(path, body)` pairs.
    #[must_use]
    pub fn render(&self, project_name: &str) -> Vec<(String, String)> {
        let snake = snake_case(project_name);
        let subst = |s: &str| {
            s.replace("{{name_snake}}", &snake)
                .replace("{{name}}", project_name)
        };
        self.files
            .iter()
            .map(|f| (subst(&f.path), subst(&f.body)))
            .collect()
    }
}

/// Convert a project name into a legal snake_case module identifier: lowercase,
/// every run of non-alphanumeric characters collapsed to a single `_`, a leading
/// digit prefixed with `_` (so it is a valid Rust/Python identifier). Empty →
/// `project`.
#[must_use]
pub fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_us = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        return "project".to_string();
    }
    if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// The built-in project templates — minimal but **buildable**: a `pyo3` repo
/// `maturin develop`s and imports; a `python` repo `pip install -e .`s and
/// imports; a `rust` repo `cargo test`s. Each carries a `.gitignore` that ignores
/// build artifacts and the `.scratch/` fence.
#[must_use]
pub fn builtin_project_templates() -> Vec<ProjectTemplate> {
    vec![pyo3_template(), python_template(), rust_template()]
}

fn file(path: &str, body: &str) -> TemplateFile {
    TemplateFile {
        path: path.to_string(),
        body: body.to_string(),
    }
}

/// Rust + PyO3 (maturin). Matches the `pyo3` tooling pack (Cargo.toml +
/// pyproject.toml). pyo3 version tracks the newt workspace (0.28, `Bound` API).
fn pyo3_template() -> ProjectTemplate {
    ProjectTemplate {
        name: "pyo3".into(),
        files: vec![
            file(
                "Cargo.toml",
                "[package]\n\
                 name = \"{{name}}\"\n\
                 version = \"0.1.0\"\n\
                 edition = \"2021\"\n\
                 \n\
                 [lib]\n\
                 name = \"{{name_snake}}\"\n\
                 crate-type = [\"cdylib\"]\n\
                 \n\
                 [dependencies]\n\
                 pyo3 = { version = \"0.28\", features = [\"extension-module\"] }\n",
            ),
            file(
                "pyproject.toml",
                "[build-system]\n\
                 requires = [\"maturin>=1.5,<2\"]\n\
                 build-backend = \"maturin\"\n\
                 \n\
                 [project]\n\
                 name = \"{{name}}\"\n\
                 version = \"0.1.0\"\n\
                 requires-python = \">=3.9\"\n\
                 \n\
                 [tool.maturin]\n\
                 features = [\"pyo3/extension-module\"]\n",
            ),
            file(
                "src/lib.rs",
                "use pyo3::prelude::*;\n\
                 \n\
                 /// Add two integers (example).\n\
                 #[pyfunction]\n\
                 fn add(a: i64, b: i64) -> i64 {\n    \
                 a + b\n\
                 }\n\
                 \n\
                 /// The `{{name_snake}}` extension module.\n\
                 #[pymodule]\n\
                 fn {{name_snake}}(m: &Bound<'_, PyModule>) -> PyResult<()> {\n    \
                 m.add_function(wrap_pyfunction!(add, m)?)?;\n    \
                 Ok(())\n\
                 }\n",
            ),
            file(".gitignore", "/target\n*.so\n__pycache__/\n.scratch/\n"),
        ],
    }
}

/// Pure-Python (hatchling backend).
fn python_template() -> ProjectTemplate {
    ProjectTemplate {
        name: "python".into(),
        files: vec![
            file(
                "pyproject.toml",
                "[build-system]\n\
                 requires = [\"hatchling\"]\n\
                 build-backend = \"hatchling.build\"\n\
                 \n\
                 [project]\n\
                 name = \"{{name}}\"\n\
                 version = \"0.1.0\"\n\
                 requires-python = \">=3.9\"\n",
            ),
            file(
                "src/{{name_snake}}/__init__.py",
                "\"\"\"The {{name}} package.\"\"\"\n\
                 \n\
                 \n\
                 def add(a: int, b: int) -> int:\n    \
                 \"\"\"Return the sum of two integers.\"\"\"\n    \
                 return a + b\n",
            ),
            file(
                "tests/test_smoke.py",
                "from {{name_snake}} import add\n\
                 \n\
                 \n\
                 def test_add() -> None:\n    \
                 assert add(2, 2) == 4\n",
            ),
            file(
                ".gitignore",
                "__pycache__/\n*.egg-info/\ndist/\n.scratch/\n",
            ),
        ],
    }
}

/// Pure-Rust library crate.
fn rust_template() -> ProjectTemplate {
    ProjectTemplate {
        name: "rust".into(),
        files: vec![
            file(
                "Cargo.toml",
                "[package]\n\
                 name = \"{{name}}\"\n\
                 version = \"0.1.0\"\n\
                 edition = \"2021\"\n\
                 \n\
                 [dependencies]\n",
            ),
            file(
                "src/lib.rs",
                "/// Add two integers (example).\n\
                 pub fn add(a: i64, b: i64) -> i64 {\n    \
                 a + b\n\
                 }\n\
                 \n\
                 #[cfg(test)]\n\
                 mod tests {\n    \
                 use super::*;\n\
                 \n    \
                 #[test]\n    \
                 fn add_sums() {\n        \
                 assert_eq!(add(2, 2), 4);\n    \
                 }\n\
                 }\n",
            ),
            file(".gitignore", "/target\n.scratch/\n"),
        ],
    }
}

/// Load drop-in templates from `dir` (`~/.newt/templates/`): each `*.toml` is a
/// serialized [`ProjectTemplate`]. Missing dir / unreadable / malformed files are
/// skipped (best-effort, like the tooling-pack loader). Order is unspecified;
/// [`merge_project_templates`] resolves collisions by name.
#[must_use]
pub fn load_templates_from_dir(dir: &std::path::Path) -> Vec<ProjectTemplate> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        if let Ok(Ok(t)) =
            std::fs::read_to_string(&path).map(|s| toml::from_str::<ProjectTemplate>(&s))
        {
            out.push(t);
        }
    }
    out
}

/// Merge template layers by `name`, later layers overriding earlier ones (so a
/// drop-in with a built-in's name REPLACES it; a new name is ADDED). Result order
/// is first-seen.
#[must_use]
pub fn merge_project_templates(layers: Vec<Vec<ProjectTemplate>>) -> Vec<ProjectTemplate> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: std::collections::HashMap<String, ProjectTemplate> =
        std::collections::HashMap::new();
    for layer in layers {
        for t in layer {
            if !by_name.contains_key(&t.name) {
                order.push(t.name.clone());
            }
            by_name.insert(t.name.clone(), t);
        }
    }
    order
        .into_iter()
        .filter_map(|n| by_name.remove(&n))
        .collect()
}

/// The resolved template set: built-ins merged under the `~/.newt/templates/`
/// drop-ins. This is what `newt new` selects from.
#[must_use]
pub fn resolved_project_templates() -> Vec<ProjectTemplate> {
    let dropins = crate::config::Config::user_config_dir()
        .map(|d| load_templates_from_dir(&d.join("templates")))
        .unwrap_or_default();
    merge_project_templates(vec![builtin_project_templates(), dropins])
}

/// The resolved template named `name`, if any.
#[must_use]
pub fn resolved_project_template(name: &str) -> Option<ProjectTemplate> {
    resolved_project_templates()
        .into_iter()
        .find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_cover_the_favored_modes() {
        let builtins = builtin_project_templates();
        let names: Vec<&str> = builtins.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"pyo3"), "{names:?}");
        assert!(names.contains(&"python"), "{names:?}");
        assert!(names.contains(&"rust"), "{names:?}");
    }

    #[test]
    fn snake_case_makes_a_legal_identifier() {
        assert_eq!(snake_case("my-cool.pkg"), "my_cool_pkg");
        assert_eq!(snake_case("My Pkg"), "my_pkg");
        assert_eq!(snake_case("2fast"), "_2fast");
        assert_eq!(snake_case("---"), "project");
        assert_eq!(snake_case("already_snake"), "already_snake");
    }

    #[test]
    fn render_substitutes_name_and_leaves_no_placeholders() {
        let files = pyo3_template().render("mypkg");
        for (path, body) in &files {
            assert!(!path.contains("{{"), "unsubstituted path: {path}");
            assert!(!body.contains("{{"), "unsubstituted body in {path}");
        }
        // The lib module + Cargo [lib] name use the snake identifier.
        let lib = files
            .iter()
            .find(|(p, _)| p == "src/lib.rs")
            .map(|(_, b)| b.as_str())
            .unwrap();
        assert!(lib.contains("fn mypkg(m: &Bound"), "{lib}");
    }

    #[test]
    fn pyo3_scaffold_round_trips_with_the_pyo3_pack() {
        // A scaffolded pyo3 project lays down exactly the marker files the pyo3
        // tooling pack detects — so its lifecycle resolves out of the box. Pure:
        // we compare rendered paths to the pack's `detect`, no filesystem.
        let paths: Vec<String> = pyo3_template()
            .render("demo")
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        let pyo3_pack = crate::tooling::builtin_tooling_packs()
            .into_iter()
            .find(|p| p.name == "pyo3")
            .unwrap();
        for marker in &pyo3_pack.detect {
            assert!(
                paths.contains(marker),
                "scaffold should include the pack marker {marker}: {paths:?}"
            );
        }
    }

    #[test]
    fn a_dropin_overrides_a_builtin_and_adds_a_new_ecosystem() {
        let dropins = vec![
            ProjectTemplate {
                name: "rust".into(),
                files: vec![file("Cargo.toml", "# custom\n")],
            },
            ProjectTemplate {
                name: "zig".into(),
                files: vec![file("build.zig", "// zig\n")],
            },
        ];
        let merged = merge_project_templates(vec![builtin_project_templates(), dropins]);
        let rust = merged.iter().find(|t| t.name == "rust").unwrap();
        assert_eq!(rust.files.len(), 1, "drop-in replaced the built-in rust");
        assert!(
            merged.iter().any(|t| t.name == "zig"),
            "new ecosystem added"
        );
    }

    #[test]
    fn template_parses_from_toml() {
        let t: ProjectTemplate = toml::from_str(
            "name = \"mini\"\n\
             [[files]]\n\
             path = \"README.md\"\n\
             body = \"# {{name}}\\n\"\n",
        )
        .unwrap();
        assert_eq!(t.name, "mini");
        assert_eq!(t.render("hello")[0].1, "# hello\n");
    }
}
