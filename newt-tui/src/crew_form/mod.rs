//! `newt crew edit [name]` / `/crew edit [name]` — an interactive form for a
//! crew's settings (`[crews.<name>]`), written as a bare-`Crew` TOML file to
//! `~/.newt/crews/<name>.toml` (the per-file crew discovery path, config.rs
//! `merge_crews_from_dir`).
//!
//! **D1a (#1885): the form is interaction/controller state.** Everything the
//! operator answers lives in [`state::CrewForm`] as a sequence of
//! `InteractionDefinition`s; this module only drives it, decides the target
//! crew, and writes the file. The flow no longer carries a private
//! line-console I/O path — it asks through C1's seam
//! (`SurfaceInteraction` → `permissions::present_on_terminal`), the same one
//! `LeanSurface` and `RichSurface` use, so the form renders through the plain
//! projection and the rich TUI without knowing which it reached.
//!
//! **Plain-scroller-safe** (`docs/decisions/plain_scroller_tui.md`): the seam
//! it asks through is `read_prompt_window_line`, which branches on
//! `is_terminal()` itself — so C0b's interactive-TTY and piped-but-answered
//! states both keep working, and neither is reimplemented here.

mod state;

use std::path::{Path, PathBuf};

use newt_core::config::Crew;
use newt_core::interaction_surface::SurfaceInteraction;
use newt_core::{Config, HumanQuestionOutcome};
use newt_interaction::InteractionDefinition;

use state::{CrewForm, Fold};

/// How the form reaches the operator: C1's seam, and nothing wider.
///
/// The same shape as `permissions.rs`'s `ask_surface` on purpose — a form
/// needs exactly what a permission question needs (present a definition, hear
/// what the operator did), so it takes the seam that already exists rather
/// than a second console trait.
type Ask<'a> = &'a dyn Fn(&SurfaceInteraction) -> HumanQuestionOutcome;

/// `~/.newt/crews/` (next to `config.toml`). Falls back to `./crews` when the
/// home config path can't be resolved.
fn crews_dir() -> PathBuf {
    Config::user_config_path()
        .map(|p| p.with_file_name("crews"))
        .unwrap_or_else(|| PathBuf::from("crews"))
}

/// Entry point for `newt crew edit [name]` (real terminal). Resolves the
/// merged config, runs the form, and writes the per-file crew on confirm.
///
/// # Errors
///
/// Propagates a write failure from [`save_crew`].
pub fn run_edit(name: Option<&str>, _color: bool) -> anyhow::Result<()> {
    let cfg = Config::resolve().unwrap_or_default();
    let dir = crews_dir();
    // Byte-identical to `LeanSurface`/`RichSurface::present_interaction`: the
    // window is acquired per prompt by whoever owns the terminal, and the
    // rendering + read is the one terminal adapter.
    let ask = |interaction: &SurfaceInteraction| {
        let window = newt_core::tty::Terminal::suspend_for_prompt();
        crate::permissions::present_on_terminal(&window, interaction)
    };
    for line in edit_and_save(&ask, &cfg, name, &dir)?.report() {
        println!("{line}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// What the flow did. Returned rather than printed, so the interaction tests
/// assert on the OUTCOME instead of grepping a transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// No crew name was supplied and none could be resolved.
    NoName,
    /// The operator declined, backed out, or the input ended. Nothing written.
    Aborted,
    /// Written. `note` carries a validation warning, if the saved crew has one.
    Wrote { path: PathBuf, note: Option<String> },
}

impl Outcome {
    /// The lines to show the operator.
    fn report(&self) -> Vec<String> {
        match self {
            Self::NoName => vec!["No crew name given — nothing to edit.".to_string()],
            Self::Aborted => vec!["Aborted. Nothing written.".to_string()],
            Self::Wrote { path, note } => {
                let mut lines = vec![format!("Wrote {}.", path.display())];
                if let Some(note) = note {
                    lines.push(format!("Note: {note}"));
                    lines.push(
                        "      (saved anyway — add the missing loadout before `newt crew`.)"
                            .to_string(),
                    );
                }
                lines
            }
        }
    }
}

/// Resolve which crew to edit, run the form, and (on confirm) write
/// `<dir>/<name>.toml`.
///
/// # Errors
///
/// Propagates a write failure from [`save_crew`].
pub(crate) fn edit_and_save(
    ask: Ask,
    cfg: &Config,
    name: Option<&str>,
    dir: &Path,
) -> anyhow::Result<Outcome> {
    let Some(name) = resolve_target_name(ask, cfg, name) else {
        return Ok(Outcome::NoName);
    };
    let path = dir.join(format!("{name}.toml"));
    let Some(crew) = run_form(ask, cfg, &name, &path) else {
        return Ok(Outcome::Aborted);
    };
    let path = save_crew(dir, &name, &crew)?;
    // Saved regardless, but flag a role that names an unknown loadout — the
    // run-time `crew.validate` would reject it, and silently letting it slip
    // through would be the kind of false-OK this codebase forbids.
    let note = crew.validate(cfg).err();
    Ok(Outcome::Wrote { path, note })
}

/// Drive the form to a confirmed crew, or `None` if nothing should be written.
///
/// Pure of the filesystem: it asks, folds, and returns. The `path` is used
/// only to name the destination in the header and the confirm preview.
fn run_form(ask: Ask, cfg: &Config, name: &str, path: &Path) -> Option<Crew> {
    let is_new = !cfg.crews.contains_key(name);
    let loadouts: Vec<&str> = cfg.loadouts.keys().map(String::as_str).collect();
    let known = if loadouts.is_empty() {
        "(no [loadouts.*] defined yet — role names won't resolve until you add some)".to_string()
    } else {
        format!("known loadouts: {}", loadouts.join(", "))
    };
    let header = format!(
        "{} crew '{name}'  →  {}\n{known}",
        if is_new { "Creating" } else { "Editing" },
        path.display()
    );
    let label = format!("{} (crew '{name}')", path.display());

    let mut form = CrewForm::new(
        &cfg.crews.get(name).cloned().unwrap_or_default(),
        header,
        label,
    );
    while let Some(definition) = form.prompt() {
        let answer = ask_line(ask, definition)?;
        if form.answer(&answer) == Fold::Declined {
            return None;
        }
    }
    Some(form.finish())
}

/// Present one definition and return the submitted line.
///
/// `None` for every non-answer: Esc, Ctrl-C, EOF, or a read failure. An
/// explicitly submitted EMPTY line is an answer and stays one — that
/// distinction is the seam's, and collapsing it here is how a closed pipe
/// came to read as "yes, write the file".
fn ask_line(ask: Ask, definition: InteractionDefinition) -> Option<String> {
    match ask(&SurfaceInteraction::blocking(definition)) {
        HumanQuestionOutcome::Answer(line) => Some(line),
        _ => None,
    }
}

/// Pick the crew to edit: the explicit name, else the sole existing crew,
/// else ask. `None` means "no usable name" (the caller bails cleanly).
fn resolve_target_name(ask: Ask, cfg: &Config, explicit: Option<&str>) -> Option<String> {
    if let Some(n) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(n.to_string());
    }
    let names: Vec<&str> = cfg.crews.keys().map(String::as_str).collect();
    let body = match names.as_slice() {
        [] => "New crew name?".to_string(),
        [only] => return Some((*only).to_string()),
        many => format!(
            "Crews: {}\nEdit which crew, or type a new name?",
            many.join(", ")
        ),
    };
    let answer = ask_line(ask, state::name_prompt(body))?;
    let answer = answer.trim();
    (!answer.is_empty()).then(|| answer.to_string())
}

/// Write `<dir>/<name>.toml` as a bare `Crew` (the per-file discovery format).
fn save_crew(dir: &Path, name: &str, crew: &Crew) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{name}.toml"));
    std::fs::write(&path, toml::to_string_pretty(crew)?)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod d1a {
    use super::*;
    use newt_core::markup::plain;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// A scripted operator: hands back outcomes in order and records the
    /// PLAIN RENDERING of every definition it was shown.
    ///
    /// It answers `SurfaceInteraction`s, not strings — so a test that passes
    /// here is a test the real terminal adapter could satisfy, and the driver
    /// gets no seam of its own.
    struct Operator {
        outcomes: RefCell<VecDeque<HumanQuestionOutcome>>,
        seen: RefCell<Vec<String>>,
        blocking: RefCell<Vec<bool>>,
    }

    impl Operator {
        /// Answers, in order. Running out means the input CLOSED — which is
        /// what a short pipe actually does, and the case the old scripted
        /// console modelled (wrongly) as an empty answer.
        fn new(answers: &[&str]) -> Self {
            Self {
                outcomes: RefCell::new(
                    answers
                        .iter()
                        .map(|a| HumanQuestionOutcome::Answer((*a).to_string()))
                        .collect(),
                ),
                seen: RefCell::new(Vec::new()),
                blocking: RefCell::new(Vec::new()),
            }
        }

        fn ask(&self, interaction: &SurfaceInteraction) -> HumanQuestionOutcome {
            self.seen
                .borrow_mut()
                .push(plain::render(&interaction.definition));
            self.blocking.borrow_mut().push(interaction.is_blocking());
            self.outcomes
                .borrow_mut()
                .pop_front()
                .unwrap_or(HumanQuestionOutcome::InputClosed)
        }
    }

    fn cfg_with(loadouts: &[&str], crews: &[(&str, Crew)]) -> Config {
        Config {
            loadouts: loadouts
                .iter()
                .map(|n| ((*n).to_string(), newt_core::Loadout::default()))
                .collect(),
            crews: crews
                .iter()
                .map(|(n, cr)| ((*n).to_string(), cr.clone()))
                .collect(),
            ..Default::default()
        }
    }

    /// The nine fields, then the confirm answer.
    fn script(confirm: &'static str) -> Vec<&'static str> {
        let mut v = vec![
            "planner",
            "navigator",
            "",
            "",
            "just check",
            "4",
            "",
            "",
            "auth",
        ];
        v.push(confirm);
        v
    }

    fn run(op: &Operator, cfg: &Config, name: &str, dir: &Path) -> Outcome {
        edit_and_save(&|i| op.ask(i), cfg, Some(name), dir).expect("no write failure")
    }

    fn read_saved(dir: &Path, name: &str) -> Crew {
        let body = std::fs::read_to_string(dir.join(format!("{name}.toml"))).unwrap();
        toml::from_str::<Crew>(&body).unwrap()
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn a_confirmed_form_writes_the_crew_field_by_field() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(&["planner", "navigator"], &[]);
        let op = Operator::new(&script("y"));
        let outcome = run(&op, &cfg, "home", dir.path());

        let Outcome::Wrote { path, note } = &outcome else {
            panic!("expected a write, got {outcome:?}");
        };
        assert_eq!(path, &dir.path().join("home.toml"));
        assert_eq!(note.as_deref(), None, "every role names a known loadout");

        let saved = read_saved(dir.path(), "home");
        assert_eq!(saved.planner, "planner");
        assert_eq!(saved.navigator.as_deref(), Some("navigator"));
        assert_eq!(saved.test.as_deref(), Some("just check"));
        assert_eq!(saved.budgets.unwrap().max_attempts, Some(4));
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn a_declined_form_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(&["planner"], &[]);
        let op = Operator::new(&script("n"));
        assert_eq!(run(&op, &cfg, "home", dir.path()), Outcome::Aborted);
        assert!(!dir.path().join("home.toml").exists());
    }

    /// **Regression (D1a, #1885): running out of input must not write.**
    ///
    /// Before this slice the flow read stdin through `StdinConsole`, which
    /// returned `""` at EOF, and confirmed with `is_yes(&ans, true)` — so a
    /// piped `newt crew edit` whose script ended early wrote the crew file
    /// without anyone confirming it. C1's seam reports `InputClosed`, which
    /// is not an answer, and the driver stops.
    #[serial_test::serial(real_fs)]
    #[test]
    fn input_closing_before_the_confirm_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(&["planner"], &[]);
        // Nine fields answered, then the pipe ends.
        let op = Operator::new(&["planner", "", "", "", "", "", "", "", ""]);
        assert_eq!(run(&op, &cfg, "home", dir.path()), Outcome::Aborted);
        assert!(!dir.path().join("home.toml").exists(), "EOF is not a yes");
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn an_unknown_loadout_is_flagged_but_still_saved() {
        let dir = tempfile::tempdir().unwrap();
        // Only "planner" is a known loadout; the form names "ghost".
        let cfg = cfg_with(&["planner"], &[]);
        let op = Operator::new(&["ghost", "", "", "", "", "", "", "", "", "y"]);
        let outcome = run(&op, &cfg, "home", dir.path());
        let Outcome::Wrote { note, .. } = &outcome else {
            panic!("saved anyway, got {outcome:?}");
        };
        assert!(note.is_some(), "the unknown-loadout note is surfaced");
        assert!(dir.path().join("home.toml").exists());
        assert!(
            outcome.report().iter().any(|l| l.starts_with("Note:")),
            "and it reaches the operator: {:?}",
            outcome.report()
        );
    }

    #[test]
    fn no_name_with_a_sole_crew_edits_that_one() {
        let op = Operator::new(&[]);
        let cfg = cfg_with(&[], &[("only", Crew::default())]);
        assert_eq!(
            resolve_target_name(&|i| op.ask(i), &cfg, None).as_deref(),
            Some("only")
        );
        assert!(op.seen.borrow().is_empty(), "and asked nothing to find it");
    }

    #[test]
    fn no_name_and_no_crews_asks_for_one() {
        let op = Operator::new(&["fresh"]);
        assert_eq!(
            resolve_target_name(&|i| op.ask(i), &Config::default(), None).as_deref(),
            Some("fresh")
        );
        assert!(op.seen.borrow()[0].contains("New crew name"));
    }

    #[test]
    fn a_closed_input_at_the_name_prompt_edits_nothing() {
        let op = Operator::new(&[]);
        assert_eq!(
            edit_and_save(&|i| op.ask(i), &Config::default(), None, Path::new("crews")).unwrap(),
            Outcome::NoName,
            "no name, no write attempt"
        );
    }

    /// Every question this flow asks goes out as a `SurfaceInteraction` — the
    /// C1 seam both `LeanSurface` and `RichSurface` present. The form has no
    /// I/O path of its own, which is what lets the rich surface render it
    /// without the form knowing.
    #[serial_test::serial(real_fs)]
    #[test]
    fn every_question_goes_out_as_a_blocking_surface_interaction() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(&["planner"], &[]);
        let op = Operator::new(&script("y"));
        run(&op, &cfg, "home", dir.path());
        assert_eq!(op.seen.borrow().len(), 10, "nine fields and a confirm");
        assert!(
            op.blocking.borrow().iter().all(|b| *b),
            "the form parks on every answer"
        );
    }
}
