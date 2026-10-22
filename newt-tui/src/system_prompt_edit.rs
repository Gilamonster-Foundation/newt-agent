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
    /// Create a scratch file holding `text`, exclusively and under a unique
    /// name in `target`'s directory, and own it until `persist` or `discard`
    /// (or drop). Returns its path for the editor.
    fn scratch(&mut self, target: &Path, text: &str) -> io::Result<PathBuf>;
    /// Run the operator's editor on `path`, handing it the terminal.
    fn edit(&mut self, path: &Path) -> anyhow::Result<()>;
    /// Move the scratch file over `target`.
    fn persist(&mut self, target: &Path) -> io::Result<()>;
    /// Remove the scratch file if one is still owned. Idempotent.
    fn discard(&mut self);
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
/// there is no override yet. The scratch file is discarded on every exit.
pub(crate) fn run(io: &mut impl EditIo, target: &Path, assembled: &str) -> anyhow::Result<Outcome> {
    let start = match io.read(target) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => assembled.to_string(),
    };
    let outcome = edit_scratch(io, target, &start);
    io.discard();
    outcome
}

fn edit_scratch(io: &mut impl EditIo, target: &Path, start: &str) -> anyhow::Result<Outcome> {
    let scratch = io.scratch(target, start)?;
    io.edit(&scratch)?;
    let edited = io.read(&scratch)?;
    if edited.trim().is_empty() {
        return Ok(Outcome::Empty);
    }
    if edited == start {
        return Ok(Outcome::Unchanged);
    }
    io.persist(target)?;
    Ok(Outcome::Saved)
}

/// The system text for one turn: the operator's file when there is one —
/// alone, with none of the harness context the session otherwise wraps around
/// the prompt — else what the session assembled.
pub(crate) fn turn_system_text(
    override_text: Option<&str>,
    assembled: impl FnOnce() -> String,
) -> String {
    override_text.map_or_else(assembled, str::to_string)
}

/// Re-resolve the override and, when it differs from `last`, rebuild the
/// session prompt (which also refreshes the footer marker). Returns whether it
/// rebuilt. Resolve and rebuild are injected so the decision is testable.
pub(crate) fn refresh_override<T: PartialEq>(
    last: &mut T,
    resolve: impl FnOnce() -> T,
    rebuild: impl FnOnce(),
) -> bool {
    let now = resolve();
    if now == *last {
        return false;
    }
    *last = now;
    rebuild();
    true
}

/// Where an edit with no `[prompt] file` yet saves to: beside the config.
pub(crate) fn default_target() -> Option<PathBuf> {
    newt_core::Config::user_config_dir().map(|dir| dir.join("system-prompt.md"))
}

/// The real filesystem, with the editor run through `run_shell` — the
/// surface's terminal hand-off. The scratch file is a `NamedTempFile`, so
/// dropping this (or `discard`) removes it whatever path the flow took.
pub(crate) struct RealIo<F> {
    run_shell: F,
    scratch: Option<tempfile::NamedTempFile>,
}

impl<F> RealIo<F> {
    pub(crate) fn new(run_shell: F) -> Self {
        Self {
            run_shell,
            scratch: None,
        }
    }
}

impl<F: FnMut(&str) -> anyhow::Result<()>> EditIo for RealIo<F> {
    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
    fn scratch(&mut self, target: &Path, text: &str) -> io::Result<PathBuf> {
        use std::io::Write;
        let dir = target.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        let mut file = tempfile::Builder::new()
            .prefix(".system-prompt-")
            .suffix(".edit")
            .tempfile_in(dir)?;
        file.write_all(text.as_bytes())?;
        file.flush()?;
        let path = file.path().to_path_buf();
        self.scratch = Some(file);
        Ok(path)
    }
    fn edit(&mut self, path: &Path) -> anyhow::Result<()> {
        (self.run_shell)(&editor_command(path))
    }
    fn persist(&mut self, target: &Path) -> io::Result<()> {
        let file = self
            .scratch
            .take()
            .ok_or_else(|| io::Error::other("no scratch file to save"))?;
        // A failed rename hands the file back; dropping it removes it.
        file.persist(target).map(|_| ()).map_err(|e| e.error)
    }
    fn discard(&mut self) {
        self.scratch = None;
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

    type Editor = Box<dyn Fn(&str) -> String>;

    #[derive(Default)]
    struct Fake {
        files: HashMap<PathBuf, String>,
        scratch: Option<PathBuf>,
        /// What the "editor" turns the scratch text into.
        editor: Option<Editor>,
        editor_fails: bool,
        persist_fails: bool,
        scratches_made: usize,
    }

    impl EditIo for Fake {
        fn read(&self, path: &Path) -> io::Result<String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| io::ErrorKind::NotFound.into())
        }
        fn scratch(&mut self, target: &Path, text: &str) -> io::Result<PathBuf> {
            self.scratches_made += 1;
            let path = target.with_file_name(format!(".scratch-{}.edit", self.scratches_made));
            self.files.insert(path.clone(), text.into());
            self.scratch = Some(path.clone());
            Ok(path)
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
        fn persist(&mut self, target: &Path) -> io::Result<()> {
            if self.persist_fails {
                return Err(io::Error::other("rename failed"));
            }
            let from = self.scratch.take().unwrap();
            let text = self.files.remove(&from).unwrap();
            self.files.insert(target.into(), text);
            Ok(())
        }
        fn discard(&mut self) {
            if let Some(path) = self.scratch.take() {
                self.files.remove(&path);
            }
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
    fn a_failed_save_is_an_error_and_cleans_up() {
        let mut io = fake(|_| "changed\n".to_string());
        io.persist_fails = true;
        assert!(run(&mut io, Path::new(TARGET), "assembled").is_err());
        assert!(io.files.is_empty(), "{:?}", io.files);
    }

    #[test]
    fn a_pre_existing_edit_sibling_is_never_the_scratch_file() {
        let mut io = fake(|_| "changed\n".to_string());
        let sibling = Path::new("/home/x/.newt/system-prompt.md.edit");
        io.files.insert(sibling.into(), "unrelated\n".into());
        run(&mut io, Path::new(TARGET), "assembled").unwrap();
        assert_eq!(io.files[sibling], "unrelated\n");
    }

    /// Grounds the fake above in the real filesystem: `NamedTempFile` really
    /// does create exclusively under a unique name, and really is removed when
    /// the rename fails. Real-resource tier (CI only; see the pre-push hook).
    #[test]
    fn real_fs_pre_existing_sibling_survives_and_a_failed_rename_leaves_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("system-prompt.md");
        let sibling = dir.path().join("system-prompt.md.edit");
        std::fs::write(&sibling, "unrelated\n").unwrap();

        // A no-op editor: the sibling is untouched and nothing new remains.
        let mut io = RealIo::new(|_: &str| Ok(()));
        assert_eq!(
            run(&mut io, &target, "assembled\n").unwrap(),
            Outcome::Unchanged
        );
        assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "unrelated\n");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        // The target is a directory, so the rename fails: error, no scratch.
        std::fs::remove_file(&sibling).unwrap();
        std::fs::create_dir(&target).unwrap();
        let scratch_of = |io_cmd: &str| {
            let quoted = io_cmd
                .rsplit(' ')
                .next()
                .unwrap()
                .trim_matches('\'')
                .to_string();
            std::fs::write(&quoted, "changed\n").unwrap();
        };
        let mut io = RealIo::new(|cmd: &str| {
            scratch_of(cmd);
            Ok(())
        });
        assert!(run(&mut io, &target, "assembled\n").is_err());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "only the directory"
        );
    }

    #[test]
    fn the_override_is_the_whole_turn_system_text_and_skips_the_assembly() {
        let text = turn_system_text(Some("pruned\n"), || panic!("assembled anyway"));
        assert_eq!(text, "pruned\n");
        assert_eq!(turn_system_text(None, || "assembled".into()), "assembled");
    }

    #[test]
    fn a_changed_override_rebuilds_once_and_an_unchanged_one_does_not() {
        let mut last: Option<&str> = None;
        let mut rebuilds = 0;
        for (now, expect) in [
            (Some("f"), true),  // set
            (Some("f"), false), // unchanged
            (None, true),       // reset
            (None, false),
        ] {
            assert_eq!(
                refresh_override(&mut last, || now, || rebuilds += 1),
                expect
            );
        }
        assert_eq!(rebuilds, 2);
    }

    /// An unusable selection is a state of its own, so moving into one (from
    /// unset, or from a different bad file) rebuilds — and the rebuild is what
    /// warns — while staying in the same bad state does not repeat itself.
    #[test]
    fn moving_into_an_unusable_file_rebuilds_so_it_can_warn() {
        type State = Result<Option<&'static str>, &'static str>;
        let mut last: State = Ok(None);
        let mut rebuilds = 0;
        for (now, expect) in [
            (Err("missing a"), true),  // unset -> bad
            (Err("missing a"), false), // same bad
            (Err("empty b"), true),    // bad -> different bad
            (Ok(Some("f")), true),     // bad -> good
            (Err("unreadable"), true), // good -> bad
        ] {
            assert_eq!(refresh_override(&mut last, || now, || rebuilds += 1), expect);
        }
        assert_eq!(rebuilds, 4);
    }

    #[test]
    fn the_editor_line_quotes_the_path_and_honours_visual_then_editor() {
        let line = editor_command(Path::new("/a b/it's.md"));
        assert!(line.starts_with("${VISUAL:-${EDITOR:-vi}} "), "{line}");
        assert!(!line.contains("/a b/it's.md"), "must be quoted: {line}");
    }
}
