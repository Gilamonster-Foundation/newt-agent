//! `newt crew edit [name]` / `/crew edit [name]` — an interactive form for a
//! crew's settings (`[crews.<name>]`), written as a bare-`Crew` TOML file to
//! `~/.newt/crews/<name>.toml` (the per-file crew discovery path, config.rs
//! `merge_crews_from_dir`).
//!
//! **Plain-scroller-safe** (`docs/decisions/plain_scroller_tui.md`): a cooked-
//! terminal prompt/response form — the same `Console` shape `setup.rs` uses —
//! NOT a ratatui widget surface. The whole flow is parameterised over a
//! `Console`, so it is exercised end-to-end with a scripted answer queue.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use newt_core::config::{Crew, CrewBudgets};
use newt_core::Config;

// ---------------------------------------------------------------------------
// Console abstraction (real stdin/stdout vs. scripted answers in tests)
// ---------------------------------------------------------------------------

/// Line-based console I/O (mirrors `setup::Console`). The real impl talks to
/// stdin/stdout; tests feed a queue of answers and capture emitted lines.
pub trait Console {
    /// Print `prompt` (no trailing newline) and read one trimmed line.
    fn ask(&mut self, prompt: &str) -> io::Result<String>;
    /// Emit an informational line.
    fn say(&mut self, line: &str);
}

/// Real console: prompts on stdout, reads a line from stdin.
struct StdinConsole;

impl Console for StdinConsole {
    fn ask(&mut self, prompt: &str) -> io::Result<String> {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut buf = String::new();
        if io::stdin().read_line(&mut buf)? == 0 {
            // EOF (piped/closed input): behave like an empty answer so the
            // caller's "keep current" default kicks in instead of looping.
            return Ok(String::new());
        }
        Ok(buf.trim().to_string())
    }

    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// `~/.newt/crews/` (next to `config.toml`). Falls back to `./crews` when the
/// home config path can't be resolved.
fn crews_dir() -> PathBuf {
    Config::user_config_path()
        .map(|p| p.with_file_name("crews"))
        .unwrap_or_else(|| PathBuf::from("crews"))
}

/// Entry point for `newt crew edit [name]` (real stdin/stdout). Resolves the
/// merged config, runs the form, and writes the per-file crew on confirm.
pub fn run_edit(name: Option<&str>, _color: bool) -> anyhow::Result<()> {
    let cfg = Config::resolve().unwrap_or_default();
    let mut console = StdinConsole;
    let dir = crews_dir();
    edit_and_save(&mut console, &cfg, name, &dir)
}

// ---------------------------------------------------------------------------
// Driver (fully testable: scripted Console + tempdir)
// ---------------------------------------------------------------------------

/// Resolve which crew to edit, run the field-by-field form, preview, and (on a
/// `[Y/n]` confirm) write `<dir>/<name>.toml`. Split from `run_edit` so tests
/// drive it with a scripted `Console` and a tempdir.
pub fn edit_and_save(
    console: &mut dyn Console,
    cfg: &Config,
    name: Option<&str>,
    dir: &Path,
) -> anyhow::Result<()> {
    let Some(name) = resolve_target_name(console, cfg, name)? else {
        console.say("No crew name given — nothing to edit.");
        return Ok(());
    };
    let starting = cfg.crews.get(&name).cloned().unwrap_or_default();
    let is_new = !cfg.crews.contains_key(&name);
    let path = dir.join(format!("{name}.toml"));
    console.say(&format!(
        "{} crew '{}'  →  {}",
        if is_new { "Creating" } else { "Editing" },
        name,
        path.display()
    ));
    let loadouts: Vec<&str> = cfg.loadouts.keys().map(String::as_str).collect();
    if loadouts.is_empty() {
        console
            .say("  (no [loadouts.*] defined yet — role names won't resolve until you add some)");
    } else {
        console.say(&format!("  known loadouts: {}", loadouts.join(", ")));
    }
    console.say("  Enter keeps the [current] value; '-' clears an optional field.");

    let crew = edit_fields(console, &starting)?;

    // Preview the exact bytes that land on disk (a bare Crew — the filename is
    // the crew name), then confirm before writing.
    let preview = toml::to_string_pretty(&crew)
        .unwrap_or_else(|e| format!("# (could not render preview: {e})"));
    console.say(&format!(
        "\n# {} (crew '{name}')\n{preview}",
        path.display()
    ));
    let ans = console.ask(&format!("Write {}? [Y/n] ", path.display()))?;
    if !is_yes(&ans, true) {
        console.say("Aborted. Nothing written.");
        return Ok(());
    }
    let written = save_crew(dir, &name, &crew)?;
    console.say(&format!("Wrote {}.", written.display()));
    // Saved regardless, but flag a role that names an unknown loadout — the
    // run-time `crew.validate` would reject it, and silently letting it slip
    // through would be the kind of false-OK this codebase forbids.
    if let Err(e) = crew.validate(cfg) {
        console.say(&format!("Note: {e}"));
        console.say("      (saved anyway — add the missing loadout before `newt crew`.)");
    }
    Ok(())
}

/// Pick the crew to edit: the explicit name, else the sole existing crew, else
/// ask. `Ok(None)` means "no name supplied" (the caller bails cleanly).
fn resolve_target_name(
    console: &mut dyn Console,
    cfg: &Config,
    explicit: Option<&str>,
) -> io::Result<Option<String>> {
    if let Some(n) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(Some(n.to_string()));
    }
    let names: Vec<&str> = cfg.crews.keys().map(String::as_str).collect();
    match names.as_slice() {
        [] => {
            let ans = console.ask("New crew name: ")?;
            Ok((!ans.is_empty()).then_some(ans))
        }
        [only] => Ok(Some((*only).to_string())),
        many => {
            console.say(&format!("Crews: {}", many.join(", ")));
            let ans = console.ask("Edit which crew (or type a new name)? ")?;
            Ok((!ans.is_empty()).then_some(ans))
        }
    }
}

/// The field-by-field prompts, each defaulting to the current value.
fn edit_fields(console: &mut dyn Console, cur: &Crew) -> io::Result<Crew> {
    let planner = ask_required(console, "planner loadout", &cur.planner)?;
    let navigator = ask_optional(console, "navigator loadout", cur.navigator.as_deref())?;
    let triage = ask_optional(console, "triage loadout", cur.triage.as_deref())?;
    let loop_program = ask_optional(console, "control loop", cur.loop_program.as_deref())?;
    let test = ask_optional(console, "test command", cur.test.as_deref())?;

    let cb = cur.budgets.clone().unwrap_or_default();
    let budgets = CrewBudgets {
        max_attempts: ask_optional_u32(console, "max attempts", cb.max_attempts)?,
        max_files_touched: ask_optional_u32(console, "max files touched", cb.max_files_touched)?,
        max_lines_changed: ask_optional_u32(console, "max lines changed", cb.max_lines_changed)?,
        require_human_review_on: ask_csv(
            console,
            "review-gate topics (comma-sep)",
            &cb.require_human_review_on,
        )?,
    };
    // Collapse an all-empty budget block back to `None` so the file stays clean.
    let budgets = (budgets != CrewBudgets::default()).then_some(budgets);

    Ok(Crew {
        planner,
        navigator,
        triage,
        loop_program,
        test,
        budgets,
    })
}

// ---------------------------------------------------------------------------
// Field prompts
// ---------------------------------------------------------------------------

/// Required string: blank keeps the current value (which may itself be blank if
/// this is a brand-new crew).
fn ask_required(console: &mut dyn Console, label: &str, cur: &str) -> io::Result<String> {
    let ans = console.ask(&format!("{label} [{cur}]: "))?;
    Ok(if ans.is_empty() { cur.to_string() } else { ans })
}

/// Optional string: blank keeps the current value; `-` clears it to `None`.
fn ask_optional(
    console: &mut dyn Console,
    label: &str,
    cur: Option<&str>,
) -> io::Result<Option<String>> {
    let shown = cur.unwrap_or("none");
    let ans = console.ask(&format!("{label} (blank=keep, '-'=none) [{shown}]: "))?;
    Ok(match ans.as_str() {
        "" => cur.map(str::to_string),
        "-" => None,
        v => Some(v.to_string()),
    })
}

/// Optional unsigned integer: blank keeps, `-` clears, anything else must parse
/// (re-prompts on garbage rather than silently dropping the field).
fn ask_optional_u32(
    console: &mut dyn Console,
    label: &str,
    cur: Option<u32>,
) -> io::Result<Option<u32>> {
    let shown = cur.map_or_else(|| "none".to_string(), |n| n.to_string());
    loop {
        let ans = console.ask(&format!("{label} (blank=keep, '-'=none) [{shown}]: "))?;
        match ans.as_str() {
            "" => return Ok(cur),
            "-" => return Ok(None),
            v => match v.parse::<u32>() {
                Ok(n) => return Ok(Some(n)),
                Err(_) => console.say("  not a number — try again."),
            },
        }
    }
}

/// Comma-separated list: blank keeps, `-` clears, anything else replaces (empty
/// items trimmed out).
fn ask_csv(console: &mut dyn Console, label: &str, cur: &[String]) -> io::Result<Vec<String>> {
    let shown = if cur.is_empty() {
        "none".to_string()
    } else {
        cur.join(",")
    };
    let ans = console.ask(&format!("{label} (blank=keep, '-'=none) [{shown}]: "))?;
    Ok(match ans.as_str() {
        "" => cur.to_vec(),
        "-" => Vec::new(),
        v => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    })
}

/// Write `<dir>/<name>.toml` as a bare `Crew` (the per-file discovery format).
fn save_crew(dir: &Path, name: &str, crew: &Crew) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{name}.toml"));
    std::fs::write(&path, toml::to_string_pretty(crew)?)?;
    Ok(path)
}

/// `[Y/n]`-style yes/no with a default for the empty answer.
fn is_yes(input: &str, default: bool) -> bool {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Scripted console: pops answers in order, records everything said.
    struct ScriptedConsole {
        answers: std::collections::VecDeque<String>,
        transcript: Vec<String>,
    }

    impl ScriptedConsole {
        fn new(answers: &[&str]) -> Self {
            Self {
                answers: answers.iter().map(|s| s.to_string()).collect(),
                transcript: Vec::new(),
            }
        }
    }

    impl Console for ScriptedConsole {
        fn ask(&mut self, prompt: &str) -> io::Result<String> {
            self.transcript.push(prompt.to_string());
            // Out of scripted answers → empty (EOF), like a closed stdin.
            Ok(self.answers.pop_front().unwrap_or_default())
        }
        fn say(&mut self, line: &str) {
            self.transcript.push(line.to_string());
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

    fn read_saved(dir: &Path, name: &str) -> Crew {
        let body = std::fs::read_to_string(dir.join(format!("{name}.toml"))).unwrap();
        toml::from_str::<Crew>(&body).unwrap()
    }

    #[test]
    fn is_yes_handles_defaults_and_explicit() {
        assert!(is_yes("", true));
        assert!(!is_yes("", false));
        assert!(is_yes("y", false));
        assert!(is_yes("YES", false));
        assert!(!is_yes("n", true));
        assert!(!is_yes("garbage", false));
    }

    #[test]
    fn new_crew_is_built_field_by_field_and_saved() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(&["planner", "navigator", "triage"], &[]);
        // name resolution is via the explicit arg, so no name prompt.
        // planner, navigator, triage, loop, test, max_attempts, files, lines,
        // review topics, then the [Y/n] write confirm.
        let mut console = ScriptedConsole::new(&[
            "planner",     // planner loadout
            "navigator",   // navigator loadout
            "triage",      // triage loadout
            "",            // control loop (none)
            "just check",  // test command
            "4",           // max attempts
            "",            // max files (keep none)
            "",            // max lines (keep none)
            "auth,crypto", // review topics
            "y",           // write
        ]);
        edit_and_save(&mut console, &cfg, Some("home"), dir.path()).unwrap();

        let saved = read_saved(dir.path(), "home");
        assert_eq!(saved.planner, "planner");
        assert_eq!(saved.navigator.as_deref(), Some("navigator"));
        assert_eq!(saved.triage.as_deref(), Some("triage"));
        assert_eq!(saved.loop_program, None);
        assert_eq!(saved.test.as_deref(), Some("just check"));
        let b = saved.budgets.unwrap();
        assert_eq!(b.max_attempts, Some(4));
        assert_eq!(b.max_files_touched, None);
        assert_eq!(b.require_human_review_on, vec!["auth", "crypto"]);
    }

    #[test]
    fn editing_keeps_current_values_on_blank_and_clears_on_dash() {
        let dir = tempfile::tempdir().unwrap();
        let existing = Crew {
            planner: "planner".into(),
            navigator: Some("navigator".into()),
            triage: Some("triage".into()),
            loop_program: Some("patch-revise".into()),
            test: Some("just check".into()),
            budgets: Some(CrewBudgets {
                max_attempts: Some(3),
                ..Default::default()
            }),
        };
        let cfg = cfg_with(&["planner", "navigator", "triage"], &[("home", existing)]);
        // Blank-through everything EXCEPT clear the navigator ('-') and bump
        // attempts to 5.
        let mut console = ScriptedConsole::new(&[
            "",  // planner (keep)
            "-", // navigator (clear)
            "",  // triage (keep)
            "",  // loop (keep)
            "",  // test (keep)
            "5", // max attempts → 5
            "",  // max files (keep none)
            "",  // max lines (keep none)
            "",  // review (keep none)
            "",  // write confirm (default Y)
        ]);
        edit_and_save(&mut console, &cfg, Some("home"), dir.path()).unwrap();

        let saved = read_saved(dir.path(), "home");
        assert_eq!(saved.planner, "planner");
        assert_eq!(saved.navigator, None, "'-' cleared the navigator");
        assert_eq!(saved.triage.as_deref(), Some("triage"));
        assert_eq!(saved.loop_program.as_deref(), Some("patch-revise"));
        assert_eq!(saved.budgets.unwrap().max_attempts, Some(5));
    }

    #[test]
    fn abort_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(&["planner"], &[]);
        let mut console = ScriptedConsole::new(&[
            "planner", "", "", "", "", "", "", "", "",  // all fields
            "n", // do NOT write
        ]);
        edit_and_save(&mut console, &cfg, Some("home"), dir.path()).unwrap();
        assert!(!dir.path().join("home.toml").exists());
        assert!(console
            .transcript
            .iter()
            .any(|l| l.contains("Nothing written")));
    }

    #[test]
    fn unknown_loadout_is_flagged_but_still_saved() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(&["planner"], &[]); // only "planner" is a known loadout
        let mut console = ScriptedConsole::new(&[
            "ghost", // planner names a loadout that doesn't exist
            "", "", "", "", "", "", "", "", "y",
        ]);
        edit_and_save(&mut console, &cfg, Some("home"), dir.path()).unwrap();
        assert!(dir.path().join("home.toml").exists(), "saved anyway");
        assert!(
            console.transcript.iter().any(|l| l.starts_with("Note:")),
            "the unknown-loadout note is surfaced"
        );
    }

    #[test]
    fn invalid_number_reprompts_then_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(&["planner"], &[]);
        let mut console = ScriptedConsole::new(&[
            "planner",
            "",
            "",
            "",
            "",             // strings
            "not-a-number", // max attempts: rejected
            "7",            // re-prompt accepts
            "",
            "",
            "",
            "y",
        ]);
        edit_and_save(&mut console, &cfg, Some("home"), dir.path()).unwrap();
        let saved = read_saved(dir.path(), "home");
        assert_eq!(saved.budgets.unwrap().max_attempts, Some(7));
        assert!(console
            .transcript
            .iter()
            .any(|l| l.contains("not a number")));
    }

    #[test]
    fn no_name_with_sole_crew_edits_that_one() {
        let mut console = ScriptedConsole::new(&[]);
        let cfg = cfg_with(&[], &[("only", Crew::default())]);
        assert_eq!(
            resolve_target_name(&mut console, &cfg, None)
                .unwrap()
                .as_deref(),
            Some("only")
        );
    }

    #[test]
    fn no_name_no_crews_prompts_for_a_name() {
        let mut console = ScriptedConsole::new(&["fresh"]);
        let cfg = Config {
            crews: BTreeMap::new(),
            ..Default::default()
        };
        assert_eq!(
            resolve_target_name(&mut console, &cfg, None)
                .unwrap()
                .as_deref(),
            Some("fresh")
        );
    }
}
