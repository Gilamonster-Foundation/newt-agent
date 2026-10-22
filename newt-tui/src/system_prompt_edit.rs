//! `/settings system-prompt edit` (#2938): open the system prompt newt would
//! send in `$EDITOR`, and keep what the operator saves as the override file.
//!
//! The editor works on a scratch copy beside the target. The target is only
//! replaced (by rename, so a crash never leaves half a prompt) when the
//! operator actually changed the text and left something in it; quitting
//! without a change leaves no override behind.

use std::io;
use std::path::{Path, PathBuf};

/// The file edit / terminal seams, injected so the flow is testable without a
/// terminal or a filesystem.
pub(crate) trait EditIo {
    fn read(&self, path: &Path) -> io::Result<String>;
    fn write(&mut self, path: &Path, text: &str) -> io::Result<()>;
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove(&mut self, path: &Path);
    /// Run the operator's editor on `path`, handing it the terminal.
    fn edit(&mut self, path: &Path) -> anyhow::Result<()>;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The edited text is now `target`.
    Saved,
    /// Nothing changed, so nothing was written.
    Unchanged,
    /// The editor left nothing; the previous state stands.
    Empty,
}

/// Edit `target`'s current contents, or `assembled` (what newt would send) when
/// there is no override yet.
pub(crate) fn run(io: &mut impl EditIo, target: &Path, assembled: &str) -> anyhow::Result<Outcome> {
    let start = match io.read(target) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => assembled.to_string(),
    };
    let scratch = scratch_path(target);
    io.write(&scratch, &start)?;
    let edited = io.edit(&scratch).and_then(|()| Ok(io.read(&scratch)?));
    let outcome = match edited {
        Err(e) => {
            io.remove(&scratch);
            return Err(e);
        }
        Ok(text) if text.trim().is_empty() => Outcome::Empty,
        Ok(text) if text == start => Outcome::Unchanged,
        Ok(_) => Outcome::Saved,
    };
    if outcome == Outcome::Saved {
        io.rename(&scratch, target)?;
    } else {
        io.remove(&scratch);
    }
    Ok(outcome)
}

fn scratch_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".edit");
    target.with_file_name(name)
}

/// Where an edit with no `[prompt] file` yet saves to: beside the config.
pub(crate) fn default_target() -> Option<PathBuf> {
    newt_core::Config::user_config_dir().map(|dir| dir.join("system-prompt.md"))
}

/// The real filesystem, with the editor run through `run_shell` — the
/// surface's terminal hand-off.
pub(crate) struct RealIo<F>(pub F);

impl<F: FnMut(&str) -> anyhow::Result<()>> EditIo for RealIo<F> {
    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
    fn write(&mut self, path: &Path, text: &str) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, text)
    }
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn remove(&mut self, path: &Path) {
        let _ = std::fs::remove_file(path);
    }
    fn edit(&mut self, path: &Path) -> anyhow::Result<()> {
        (self.0)(&editor_command(path))
    }
}

/// The override the session last built its system prompt from, for the status
/// line: what is on the wire, not what the config says. Set by every rebuild.
static ACTIVE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

pub(crate) fn set_active(path: Option<PathBuf>) {
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = path;
}

/// `[prompt: <file name>]` while an override file is in force.
#[cfg(feature = "rich-tui")]
pub(crate) fn status_badge() -> Option<String> {
    let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    active.as_deref().map(badge_for)
}

#[cfg(feature = "rich-tui")]
fn badge_for(path: &Path) -> String {
    format!(
        "[prompt: {}]",
        path.file_name().unwrap_or_default().to_string_lossy()
    )
}

/// The shell line that runs the operator's editor on `path`.
pub(crate) fn editor_command(path: &Path) -> String {
    format!(
        "${{VISUAL:-${{EDITOR:-vi}}}} {}",
        newt_core::mcp::shell_quote_arg(&path.to_string_lossy())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        files: HashMap<PathBuf, String>,
        /// What the "editor" turns the scratch text into.
        editor: Option<Box<dyn Fn(&str) -> String>>,
        editor_fails: bool,
    }

    impl EditIo for Fake {
        fn read(&self, path: &Path) -> io::Result<String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| io::ErrorKind::NotFound.into())
        }
        fn write(&mut self, path: &Path, text: &str) -> io::Result<()> {
            self.files.insert(path.into(), text.into());
            Ok(())
        }
        fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
            let text = self.files.remove(from).ok_or(io::ErrorKind::NotFound)?;
            self.files.insert(to.into(), text);
            Ok(())
        }
        fn remove(&mut self, path: &Path) {
            self.files.remove(path);
        }
        fn edit(&mut self, path: &Path) -> anyhow::Result<()> {
            anyhow::ensure!(!self.editor_fails, "no editor");
            let text = self.files[path].clone();
            let edited = self
                .editor
                .as_ref()
                .map_or_else(|| text.clone(), |f| f(&text));
            self.files.insert(path.into(), edited);
            Ok(())
        }
    }

    fn fake(edit: impl Fn(&str) -> String + 'static) -> Fake {
        Fake {
            editor: Some(Box::new(edit)),
            ..Fake::default()
        }
    }

    const TARGET: &str = "/home/x/.newt/system-prompt.md";

    #[test]
    fn first_edit_starts_from_the_assembled_prompt_and_saves_the_pruned_text() {
        let mut io = fake(|t| {
            assert_eq!(t, "soul\nworkspace\nreadme\n");
            "soul\n".to_string()
        });
        let out = run(&mut io, Path::new(TARGET), "soul\nworkspace\nreadme\n").unwrap();
        assert_eq!(out, Outcome::Saved);
        assert_eq!(io.files[Path::new(TARGET)], "soul\n");
        assert_eq!(
            io.files.len(),
            1,
            "the scratch copy is gone: {:?}",
            io.files
        );
    }

    #[test]
    fn a_later_edit_starts_from_the_saved_override_not_the_assembled_prompt() {
        let mut io = fake(|t| {
            assert_eq!(t, "pruned\n");
            "pruned more\n".to_string()
        });
        io.files.insert(TARGET.into(), "pruned\n".into());
        run(&mut io, Path::new(TARGET), "assembled").unwrap();
        assert_eq!(io.files[Path::new(TARGET)], "pruned more\n");
    }

    #[test]
    fn quitting_without_a_change_leaves_no_override() {
        let mut io = fake(str::to_string);
        let out = run(&mut io, Path::new(TARGET), "assembled").unwrap();
        assert_eq!(out, Outcome::Unchanged);
        assert!(io.files.is_empty(), "{:?}", io.files);
    }

    #[test]
    fn emptying_the_buffer_keeps_the_previous_override() {
        let mut io = fake(|_| "  \n".to_string());
        io.files.insert(TARGET.into(), "keep me\n".into());
        let out = run(&mut io, Path::new(TARGET), "assembled").unwrap();
        assert_eq!(out, Outcome::Empty);
        assert_eq!(io.files[Path::new(TARGET)], "keep me\n");
        assert_eq!(io.files.len(), 1);
    }

    #[test]
    fn a_failed_editor_is_an_error_and_cleans_up() {
        let mut io = Fake {
            editor_fails: true,
            ..Fake::default()
        };
        assert!(run(&mut io, Path::new(TARGET), "assembled").is_err());
        assert!(io.files.is_empty());
    }

    #[test]
    fn the_editor_line_quotes_the_path_and_honours_visual_then_editor() {
        let line = editor_command(Path::new("/a b/it's.md"));
        assert!(line.starts_with("${VISUAL:-${EDITOR:-vi}} "), "{line}");
        assert!(!line.contains("/a b/it's.md"), "must be quoted: {line}");
    }
}
