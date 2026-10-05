//! Observed preimages and held inverses; no persistent undo format.
//! Comparisons detect intervening edits, but do not lock out external writers.
use super::extract::Extracted;

#[derive(Clone, Copy, Debug)]
pub(super) enum File {
    Source,
    Child,
}

pub(super) trait Files {
    fn read(&self, file: File) -> Result<Option<String>, String>;
    /// Capture and compare expected bytes before publishing without overwriting a new entry.
    /// An error may occur after publication (e.g. a durability failure).
    fn change(&self, file: File, expected: Option<&str>, next: Option<&str>) -> Result<(), String>;
}

pub(super) fn run(
    files: &impl Files,
    before: &str,
    after: &Extracted,
    mut check: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    matches(files, File::Source, Some(before))?;
    matches(files, File::Child, None)?;
    // Baseline failures cannot be attributed to an extraction; leave it untouched.
    check().map_err(|e| format!("baseline check failed; source files unchanged: {e}"))?;
    matches(files, File::Source, Some(before))?;
    matches(files, File::Child, None)?;
    let mut inverse = Inverse {
        files,
        before,
        after,
        armed: true,
    };
    let result = (|| {
        files.change(File::Child, None, Some(&after.child))?;
        files.change(File::Source, Some(before), Some(&after.source))?;
        check()?;
        matches(files, File::Source, Some(&after.source))?;
        matches(files, File::Child, Some(&after.child))
    })();
    match result {
        Ok(()) => {
            inverse.armed = false;
            Ok(())
        }
        Err(error) => {
            let restored = inverse.restore();
            inverse.armed = false;
            Err(format!(
                "{error}; {}",
                match restored {
                    Ok(()) if error.contains("CONFLICT") => "rollback observed original paths; CONFLICT recovery entries still require inspection".into(),
                    Ok(()) => "original files restored; displaced entries retained as .newt-move-*.saved".into(),
                    Err(e) => format!("rollback incomplete; preserve and inspect both files and .newt-move-*.saved recovery entries: {e}"),
                }
            ))
        }
    }
}

fn matches(files: &impl Files, file: File, expected: Option<&str>) -> Result<(), String> {
    if files.read(file)?.as_deref() == expected {
        Ok(())
    } else {
        Err(format!(
            "stale {file:?} preimage/postimage; reread before retrying"
        ))
    }
}

struct Inverse<'a, F: Files> {
    files: &'a F,
    before: &'a str,
    after: &'a Extracted,
    armed: bool,
}
impl<F: Files> Inverse<'_, F> {
    fn restore(&self) -> Result<(), String> {
        let mut errors = Vec::new();
        for (file, old, written) in [
            (
                File::Source,
                Some(self.before),
                Some(self.after.source.as_str()),
            ),
            (File::Child, None, Some(self.after.child.as_str())),
        ] {
            let restore = (|| {
                if self.files.read(file)?.as_deref() == old {
                    return Ok(());
                }
                self.files.change(file, written, old)?;
                matches(self.files, file, old)
            })();
            if let Err(e) = restore {
                errors.push(e);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}
impl<F: Files> Drop for Inverse<'_, F> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.restore();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    struct Memory(RefCell<[Option<String>; 2]>);
    impl Files for Memory {
        fn read(&self, f: File) -> Result<Option<String>, String> {
            Ok(self.0.borrow()[f as usize].clone())
        }
        fn change(
            &self,
            f: File,
            expected: Option<&str>,
            next: Option<&str>,
        ) -> Result<(), String> {
            matches(self, f, expected)?;
            self.0.borrow_mut()[f as usize] = next.map(str::to_owned);
            Ok(())
        }
    }
    fn fixture() -> (Memory, Extracted) {
        (
            Memory(RefCell::new([Some("before".into()), None])),
            Extracted {
                source: "after".into(),
                child: "moved".into(),
            },
        )
    }
    /// #2724: success requires both checks and exact submitted postimages.
    #[test]
    fn verified_success() {
        let (files, after) = fixture();
        let mut calls = 0;
        run(&files, "before", &after, || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(files.read(File::Source).unwrap().as_deref(), Some("after"));
        assert_eq!(files.read(File::Child).unwrap().as_deref(), Some("moved"));
    }
    /// #2724: failed and unavailable checks restore both original preimages.
    #[test]
    fn failed_check_restores() {
        for failure in ["compiler exit 101", "executor unavailable"] {
            let (files, after) = fixture();
            let mut calls = 0;
            let result = run(&files, "before", &after, || {
                calls += 1;
                if calls == 2 {
                    Err(failure.into())
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert!(result.contains("original files restored"));
            assert_eq!(*files.0.borrow(), [Some("before".into()), None]);
        }
    }
    /// #2724: a queued approval/check cannot overwrite an intervening edit.
    #[test]
    fn stale_preimages_and_existing_destination_are_refused() {
        let (files, after) = fixture();
        assert!(run(&files, "stale", &after, || panic!("no check")).is_err());
        files.0.borrow_mut()[1] = Some("foreign".into());
        assert!(run(&files, "before", &after, || panic!("no check")).is_err());
        files.0.borrow_mut()[1] = None;
        assert!(run(&files, "before", &after, || {
            files.0.borrow_mut()[0] = Some("concurrent".into());
            Ok(())
        })
        .is_err());
        assert_eq!(*files.0.borrow(), [Some("concurrent".into()), None]);
    }
    /// #2724: do not report success or roll back somebody else's new bytes.
    #[test]
    fn concurrent_postimage_is_preserved() {
        let (files, after) = fixture();
        let mut calls = 0;
        let error = run(&files, "before", &after, || {
            calls += 1;
            if calls == 2 {
                files.0.borrow_mut()[0] = Some("foreign".into());
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("rollback incomplete"));
        assert_eq!(*files.0.borrow(), [Some("foreign".into()), None]);
    }
    /// #2724: publication can fail after making bytes visible; invert that too.
    #[test]
    fn failed_publication_restores_and_baseline_failure_never_writes() {
        struct FailOnce {
            memory: Memory,
            armed: std::cell::Cell<bool>,
        }
        impl Files for FailOnce {
            fn read(&self, f: File) -> Result<Option<String>, String> {
                self.memory.read(f)
            }
            fn change(&self, f: File, old: Option<&str>, new: Option<&str>) -> Result<(), String> {
                self.memory.change(f, old, new)?;
                if self.armed.replace(false) {
                    Err("published then failed".into())
                } else {
                    Ok(())
                }
            }
        }
        let (memory, after) = fixture();
        let files = FailOnce {
            memory,
            armed: std::cell::Cell::new(true),
        };
        let error = run(&files, "before", &after, || Ok(())).unwrap_err();
        assert!(error.contains("original files restored"));
        assert_eq!(*files.memory.0.borrow(), [Some("before".into()), None]);
        assert!(run(&files, "before", &after, || Err("baseline".into()))
            .unwrap_err()
            .contains("source files unchanged"));
        assert_eq!(*files.memory.0.borrow(), [Some("before".into()), None]);
    }

    /// #2724: unwinding through an injected checker still attempts the inverses.
    #[test]
    fn panicking_check_restores() {
        let (files, after) = fixture();
        let mut calls = 0;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run(&files, "before", &after, || {
                calls += 1;
                assert_ne!(calls, 2, "checker panic");
                Ok(())
            });
        }));
        assert_eq!(*files.0.borrow(), [Some("before".into()), None]);
    }
}
