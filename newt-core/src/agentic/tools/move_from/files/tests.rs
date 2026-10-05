use super::*;
use crate::agentic::tools::move_from::{extract::Extracted, transaction};

/// #2724: an editor publication after the final comparison must survive either
/// source restoration or child removal. This grounds the in-memory conflict
/// test in the real filesystem, with an injected handshake instead of timing.
#[test]
fn rollback_preserves_editor_publication_between_comparison_and_act() {
    for victim in [File::Source, File::Child] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("source.rs");
        let child = root.join("child.rs");
        std::fs::write(&source, "before").unwrap();
        struct Racing {
            disk: DiskFiles,
            victim: File,
        }
        impl Files for Racing {
            fn read(&self, file: File) -> Result<Option<String>, String> {
                self.disk.read(file)
            }
            fn change(
                &self,
                file: File,
                expected: Option<&str>,
                next: Option<&str>,
            ) -> Result<(), String> {
                let entry = &self.disk.paths[file as usize];
                if file as usize == self.victim as usize
                    && (next == Some("before") || next.is_none())
                {
                    entry.change_with(expected, next, || {
                        let editor = entry.path.with_extension("editor");
                        std::fs::write(&editor, "foreign editor bytes").unwrap();
                        std::fs::rename(editor, &entry.path).unwrap();
                    })
                } else {
                    entry.change(expected, next)
                }
            }
        }
        let files = Racing {
            disk: DiskFiles::open(&root, &source, &child).unwrap(),
            victim,
        };
        let mut calls = 0;
        let error = transaction::run(
            &files,
            "before",
            &Extracted {
                source: "after".into(),
                child: "moved".into(),
            },
            || {
                calls += 1;
                if calls == 2 {
                    Err("compiler failure".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(
            files.read(victim).unwrap().as_deref(),
            Some("foreign editor bytes"),
            "{error}"
        );
        assert!(error.contains("CONFLICT"), "{error}");
        assert!(!error.contains("original files restored"), "{error}");
    }
}

/// #2724: a second writer may fill the vacated name before recovery. Neither
/// publication nor conflict recovery may overwrite it; retain the displaced one.
#[test]
fn recovery_never_overwrites_a_reappearing_entry() {
    for next in [Some("before"), None] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("source.rs");
        std::fs::write(&source, "written").unwrap();
        let files = DiskFiles::open(&root, &source, &root.join("child.rs")).unwrap();
        let error = files.paths[0]
            .change_with_hooks(
                Some("written"),
                next,
                || {},
                || {
                    std::fs::write(&source, "new occupant").unwrap();
                },
            )
            .unwrap_err();
        assert!(error.contains("CONFLICT"), "{error}");
        assert!(error.contains("displaced entry retained at"), "{error}");
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "new occupant");
        assert_eq!(saved_bytes(&root), vec!["written".to_owned()]);
    }
}

/// #2724: comparing a captured inode cannot rule out later writes through an
/// editor's open handle. Even successful replacement/removal must retain it.
#[test]
fn displaced_inode_survives_late_writes_through_an_open_handle() {
    use std::io::{Seek, SeekFrom, Write};
    for next in [Some("before"), None] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("source.rs");
        std::fs::write(&source, "written").unwrap();
        let mut editor = std::fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap();
        let files = DiskFiles::open(&root, &source, &root.join("child.rs")).unwrap();
        files.paths[0].change(Some("written"), next).unwrap();
        editor.seek(SeekFrom::Start(0)).unwrap();
        editor.write_all(b"foreign").unwrap();
        editor.sync_all().unwrap();
        assert_eq!(saved_bytes(&root), vec!["foreign".to_owned()]);
        assert_eq!(files.read(File::Source).unwrap().as_deref(), next);
    }
}

fn saved_bytes(root: &Path) -> Vec<String> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "saved"))
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect()
}
