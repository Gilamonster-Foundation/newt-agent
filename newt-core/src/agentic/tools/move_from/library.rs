//! Prove the edited file lies on an unconditional, ordinary library module path.
//! A green --lib check says nothing about a disconnected or cfg-disabled file.
use crate::caveats::Caveats;
use std::path::Path;

pub(super) fn source_in_library(
    library: &Path,
    source: &Path,
    caveats: &Caveats,
) -> Result<(), String> {
    verify(library, source, |path| read_source(path, caveats))
}

pub(super) fn read_source(path: &Path, caveats: &Caveats) -> Result<String, String> {
    let target = path.to_string_lossy();
    if !super::super::tui_permits_path(&caveats.fs_read, &target) {
        return Err(super::super::denied_fs_result("fs_read", &target));
    }
    super::super::file_capture::read_for_edit(&caveats.fs_read, path, &target)
}

fn verify(
    library: &Path,
    source: &Path,
    mut read: impl FnMut(&Path) -> Result<String, String>,
) -> Result<(), String> {
    let mut current = library.to_owned();
    let mut directory = library.parent().ok_or("library has no parent")?.to_owned();
    loop {
        let text = read(&current)?;
        let tree = crate::ast_outline::parse_rust(&text)?;
        let root = tree.root_node();
        if root.has_error() {
            return Err("library module does not parse".into());
        }
        // Inner cfg/path transformations can disable or relocate this entire file.
        let mut walk = root.walk();
        if root.named_children(&mut walk).any(|n| {
            n.kind() == "inner_attribute_item" && {
                let value = &text[n.byte_range()];
                matches!(
                    value
                        .trim_start_matches("#![")
                        .trim_start()
                        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                        .next(),
                    Some("cfg" | "cfg_attr" | "path")
                )
            }
        }) {
            return Err("conditional/relocated library modules are not supported".into());
        }
        if current == source {
            return Ok(());
        }
        let relative = source
            .strip_prefix(&directory)
            .map_err(|_| "source is not reachable from the library target")?;
        let component = relative
            .components()
            .next()
            .ok_or("source is not a library module")?;
        let name = Path::new(component.as_os_str())
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("invalid module path")?;
        declared_child(&text, name)?;
        let flat = directory.join(format!("{name}.rs"));
        let nested = directory.join(name).join("mod.rs");
        // Do not guess between two layouts. The read seam preserves fs grants.
        let flat_text = read(&flat);
        let nested_text = read(&nested);
        current = match (flat_text.is_ok(), nested_text.is_ok()) {
            (true, false) => flat,
            (false, true) => nested,
            _ => {
                return Err(
                    "library child must resolve to exactly one readable ordinary module file"
                        .into(),
                )
            }
        };
        directory = directory.join(name);
    }
}

fn declared_child(text: &str, name: &str) -> Result<(), String> {
    let tree = crate::ast_outline::parse_rust(text)?;
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut attributes = false;
    let mut found = 0;
    for node in root.named_children(&mut cursor) {
        match node.kind() {
            "attribute_item" => {
                attributes = true;
                continue;
            }
            "line_comment" | "block_comment" => continue,
            "mod_item"
                if node
                    .child_by_field_name("name")
                    .is_some_and(|n| &text[n.byte_range()] == name) =>
            {
                if attributes || node.child_by_field_name("body").is_some() {
                    return Err(
                        "conditional, attributed or inline module paths are not supported".into(),
                    );
                }
                found += 1;
            }
            _ => {}
        }
        attributes = false;
    }
    if found == 1 {
        Ok(())
    } else {
        Err("source must have one unconditional declaration in the library module tree".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    /// #2724: --lib must actually reach the selected source, not just exit zero.
    #[test]
    fn only_unconditional_library_paths_count() {
        let mut files = BTreeMap::from([
            (PathBuf::from("src/lib.rs"), "mod outer;"),
            (PathBuf::from("src/outer.rs"), "mod inner;"),
            (PathBuf::from("src/outer/inner.rs"), "fn selected() {}"),
        ]);
        let root = Path::new("src/lib.rs");
        let target = Path::new("src/outer/inner.rs");
        let check = |files: &BTreeMap<PathBuf, &str>, target| {
            verify(root, target, |p| {
                files.get(p).map(|s| (*s).into()).ok_or("missing".into())
            })
        };
        check(&files, target).unwrap();
        assert!(check(&files, Path::new("src/main.rs")).is_err());
        for declaration in [
            "#[cfg(feature = \"off\")] mod outer;",
            "#[path=\"outer.rs\"] mod outer;",
            "mod outer {}",
            "mod outer; mod outer;",
        ] {
            files.insert(root.into(), declaration);
            assert!(check(&files, target).is_err());
        }
        files.insert(root.into(), "mod outer;");
        files.insert("src/outer/mod.rs".into(), "mod inner;");
        assert!(check(&files, target).is_err());
        files.remove(Path::new("src/outer/mod.rs"));
        files.insert("src/outer.rs".into(), "#![cfg(feature=\"off\")] mod inner;");
        assert!(check(&files, target).is_err());
    }
}
