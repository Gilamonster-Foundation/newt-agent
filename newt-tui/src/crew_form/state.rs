//! **The crew form as interaction state** (D1a, #1885).
//!
//! The form used to be a straight line of `console.ask(...)` calls: the
//! sequence, the prompts, the defaults and the validation were tangled into
//! the I/O, so the only way to exercise any of it was to script a `Console`.
//!
//! Here it is a state machine over [`InteractionDefinition`]s. It performs no
//! I/O, constructs no `PromptWindow`, opens no file and prints nothing; it
//! says what to ask next and folds an answer back in. Every rule — which
//! field is required, what blank means, what `-` means, what a bad number
//! does — is testable with plain strings, and the same form reaches a
//! terminal, a pipe, and (through the protocol) any other surface without a
//! form-specific implementation of any of them.
//!
//! ## No parser and no second Console live here
//!
//! D0's deletion gate stands. The only *choice* this form asks — the final
//! write confirm — is a [`ControlKind::Choice`] resolved by
//! [`newt_interaction::binding::resolve_typed`], the one resolver. That
//! retires this flow's use of `line_console::is_yes`, which was a second
//! yes/no resolution with its own case rules.
//!
//! What validation remains is `u32::from_str` and comma splitting: turning
//! text into a value, not resolving an answer to an offered option.
//!
//! ## Blank never writes the file
//!
//! `is_yes(&ans, true)` made an empty answer mean YES, and the prompt
//! advertised that with `[Y/n]`. Two things were wrong with it. The epic's
//! acceptance criterion is that a surface never CHOOSES A DEFAULT for a
//! decision — `markup::plain` renders a toggle as `[y/n]`, never `[Y/N]`,
//! for exactly this reason. And under a pipe it was a live defect rather
//! than a style question: running out of input read as an empty answer, so
//! `newt crew edit < short-script` **wrote the crew file** on EOF.
//!
//! So the confirm resolves `y`/`n` and refuses everything else, blank
//! included. A field default is different and survives: blank keeps the
//! CURRENT value, which is the operator's own prior state, displayed in the
//! prompt. Keeping what is on screen is not the machine deciding.

use newt_core::config::{Crew, CrewBudgets};
// D1b-2 (#1903): the field builders this module grew in D1a now live in
// `newt_core::interaction_form`, because the setup wizard needs the same two
// and a second spelling of "a free-text field with a default" is how `is_yes`
// became three implementations. Same shapes, one owner.
use newt_core::interaction_form::{self, NO, YES};
use newt_interaction::InteractionDefinition;

/// Consecutive unusable answers at one step before the form gives up.
///
/// A cap, not a nicety: under a pipe an unusable answer is re-asked against
/// the same exhausted input, so an uncapped retry is a hang. Fail-closed —
/// giving up writes nothing.
const MAX_RETRIES: u8 = 3;

/// Which field the form is on.
///
/// Ordered as the operator sees them, and walked by [`Step::next`] rather
/// than by an index, so a field cannot be added and silently skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Planner,
    Navigator,
    Triage,
    LoopProgram,
    Test,
    MaxAttempts,
    MaxFilesTouched,
    MaxLinesChanged,
    ReviewGate,
    Confirm,
    Done,
}

impl Step {
    fn next(self) -> Self {
        match self {
            Self::Planner => Self::Navigator,
            Self::Navigator => Self::Triage,
            Self::Triage => Self::LoopProgram,
            Self::LoopProgram => Self::Test,
            Self::Test => Self::MaxAttempts,
            Self::MaxAttempts => Self::MaxFilesTouched,
            Self::MaxFilesTouched => Self::MaxLinesChanged,
            Self::MaxLinesChanged => Self::ReviewGate,
            Self::ReviewGate => Self::Confirm,
            Self::Confirm | Self::Done => Self::Done,
        }
    }
}

/// What folding an answer in did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fold {
    /// Accepted. Ask [`CrewForm::prompt`] again (it may now be `None`).
    Advanced,
    /// Not usable. The same step is re-asked, carrying the reason.
    Retry,
    /// Nothing is written: the operator answered `n`, or gave
    /// [`MAX_RETRIES`] unusable answers in a row.
    Declined,
}

/// The crew form, mid-flight.
#[derive(Debug, Clone)]
pub(crate) struct CrewForm {
    crew: Crew,
    budgets: CrewBudgets,
    step: Step,
    /// Context shown once, above the first field.
    header: String,
    /// What the confirm previews: the destination, and the exact bytes.
    label: String,
    /// Why the current step is being re-asked.
    retry: Option<&'static str>,
    /// Consecutive refusals at the current step.
    refusals: u8,
}

impl CrewForm {
    /// Start editing `starting`. `header` is shown once above the first
    /// field; `label` names the destination in the confirm.
    pub(crate) fn new(
        starting: &Crew,
        header: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            crew: starting.clone(),
            budgets: starting.budgets.clone().unwrap_or_default(),
            step: Step::Planner,
            header: header.into(),
            label: label.into(),
            retry: None,
            refusals: 0,
        }
    }

    /// The prompt to present now, or `None` once the form has finished.
    pub(crate) fn prompt(&self) -> Option<InteractionDefinition> {
        let field = |body: &str, hint: String| {
            let body = if self.step == Step::Planner {
                format!("{}\n\n{body}", self.header)
            } else {
                body.to_string()
            };
            Some(interaction_form::text_field(body, self.note(hint)))
        };
        match self.step {
            Step::Done => None,
            Step::Planner => field("planner loadout", keep_hint(&self.crew.planner)),
            Step::Navigator => field(
                "navigator loadout",
                clear_hint(self.crew.navigator.as_deref()),
            ),
            Step::Triage => field("triage loadout", clear_hint(self.crew.triage.as_deref())),
            Step::LoopProgram => field(
                "control loop",
                clear_hint(self.crew.loop_program.as_deref()),
            ),
            Step::Test => field("test command", clear_hint(self.crew.test.as_deref())),
            Step::MaxAttempts => field("max attempts", number_hint(self.budgets.max_attempts)),
            Step::MaxFilesTouched => field(
                "max files touched",
                number_hint(self.budgets.max_files_touched),
            ),
            Step::MaxLinesChanged => field(
                "max lines changed",
                number_hint(self.budgets.max_lines_changed),
            ),
            Step::ReviewGate => field(
                "review-gate topics",
                csv_hint(&self.budgets.require_human_review_on),
            ),
            Step::Confirm => Some(self.write_confirm()),
        }
    }

    /// Fold `answer` into the form.
    ///
    /// The answer is TRIMMED, unlike the terminal adapter's verbatim line: a
    /// loadout name, a count and a topic list have no whitespace-significant
    /// spelling, and `StdinConsole::ask` trimmed before this slice — so not
    /// trimming here would silently reject ` 4 `, which used to work.
    pub(crate) fn answer(&mut self, answer: &str) -> Fold {
        let answer = answer.trim();
        let fold = match self.step {
            Step::Done => Fold::Advanced,
            // Blank keeps the current value, which on a brand-new crew is
            // itself blank — the console form's rule, unchanged.
            Step::Planner => {
                if !answer.is_empty() {
                    self.crew.planner = answer.to_string();
                }
                self.advance()
            }
            Step::Navigator => {
                self.crew.navigator = optional(answer, self.crew.navigator.take());
                self.advance()
            }
            Step::Triage => {
                self.crew.triage = optional(answer, self.crew.triage.take());
                self.advance()
            }
            Step::LoopProgram => {
                self.crew.loop_program = optional(answer, self.crew.loop_program.take());
                self.advance()
            }
            Step::Test => {
                self.crew.test = optional(answer, self.crew.test.take());
                self.advance()
            }
            Step::MaxAttempts => self.number(answer, |b| &mut b.max_attempts),
            Step::MaxFilesTouched => self.number(answer, |b| &mut b.max_files_touched),
            Step::MaxLinesChanged => self.number(answer, |b| &mut b.max_lines_changed),
            Step::ReviewGate => {
                self.budgets.require_human_review_on =
                    csv(answer, &self.budgets.require_human_review_on);
                self.advance()
            }
            Step::Confirm => match self.resolve_confirm(answer) {
                Some(true) => {
                    self.step = Step::Done;
                    Fold::Advanced
                }
                Some(false) => Fold::Declined,
                // Unrecognised REFUSES; it does not fall back to a default.
                // The confirm writes a file, so guessing here is the one
                // thing it must not do. See the module note on blank.
                None => self.refuse("answer y or n"),
            },
        };
        // Give up rather than re-ask forever against an input that has
        // nothing left to give.
        if fold == Fold::Retry && self.refusals >= MAX_RETRIES {
            return Fold::Declined;
        }
        fold
    }

    /// The crew as edited. Meaningful once [`CrewForm::prompt`] is `None`.
    pub(crate) fn finish(&self) -> Crew {
        let mut crew = self.crew.clone();
        // Collapse an all-empty budget block back to `None` so the file
        // stays clean — the console form's rule, unchanged.
        crew.budgets = (self.budgets != CrewBudgets::default()).then(|| self.budgets.clone());
        crew
    }

    fn advance(&mut self) -> Fold {
        self.step = self.step.next();
        self.retry = None;
        self.refusals = 0;
        Fold::Advanced
    }

    fn refuse(&mut self, why: &'static str) -> Fold {
        self.retry = Some(why);
        self.refusals += 1;
        Fold::Retry
    }

    fn number(&mut self, answer: &str, field: fn(&mut CrewBudgets) -> &mut Option<u32>) -> Fold {
        match answer {
            "" => {}
            "-" => *field(&mut self.budgets) = None,
            v => match v.parse::<u32>() {
                Ok(n) => *field(&mut self.budgets) = Some(n),
                Err(_) => return self.refuse("not a number — try again"),
            },
        }
        self.advance()
    }

    /// The exact bytes the confirm previews, and that a write would emit.
    fn preview(&self) -> String {
        toml::to_string_pretty(&self.finish())
            .unwrap_or_else(|e| format!("# (could not render preview: {e})"))
    }

    /// The note line: the retry reason, if any, above the field hint.
    fn note(&self, hint: String) -> String {
        match self.retry {
            Some(why) => format!("{why}\n{hint}"),
            None => hint,
        }
    }

    /// Resolve the confirm through the ONE resolver.
    ///
    /// The mapping back to a bool reads the option's stable `id`, never its
    /// `role`: role is author-assigned (A3). This definition is built here
    /// rather than authored, but reading a role to decide what an answer
    /// MEANS is the habit that breaks where the definition is not ours.
    fn resolve_confirm(&self, answer: &str) -> Option<bool> {
        match interaction_form::resolve(&self.write_confirm(), answer)?.as_str() {
            YES => Some(true),
            NO => Some(false),
            _ => None,
        }
    }

    /// The write confirm: the destination, the exact bytes, and a real
    /// choice. Built in one place so `prompt` and `resolve_confirm` cannot
    /// offer and resolve two different option sets.
    ///
    /// **Not named `confirm_prompt`**, which is the ratchet's needle for a
    /// BESPOKE confirm builder beside the shared path (`sas_confirm` and
    /// `rich_input` each hold one). This is the opposite: it delegates to
    /// `interaction_form::confirm` and adds only this form's body. Taking a
    /// baseline row would have recorded a third bespoke builder that does
    /// not exist.
    fn write_confirm(&self) -> InteractionDefinition {
        interaction_form::confirm(
            format!("# {}\n{}\nWrite it?", self.label, self.preview()),
            self.retry.unwrap_or_default(),
            "yes, write it",
            "no, leave it alone",
        )
    }
}

fn optional(answer: &str, cur: Option<String>) -> Option<String> {
    match answer {
        "" => cur,
        "-" => None,
        v => Some(v.to_string()),
    }
}

fn csv(answer: &str, cur: &[String]) -> Vec<String> {
    match answer {
        "" => cur.to_vec(),
        "-" => Vec::new(),
        v => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    }
}

fn keep_hint(cur: &str) -> String {
    format!("[{cur}] — Enter keeps it")
}

fn clear_hint(cur: Option<&str>) -> String {
    format!(
        "[{}] — Enter keeps it, '-' clears it",
        cur.unwrap_or("none")
    )
}

fn number_hint(cur: Option<u32>) -> String {
    clear_hint(cur.map(|n| n.to_string()).as_deref())
}

fn csv_hint(cur: &[String]) -> String {
    let shown = (!cur.is_empty()).then(|| cur.join(","));
    format!(
        "[{}] — comma-separated; Enter keeps it, '-' clears it",
        shown.as_deref().unwrap_or("none")
    )
}

/// The crew-name prompt: which crew the form is about to edit.
///
/// Built here rather than in the driver so EVERY definition this flow
/// presents comes from one module.
pub(crate) fn name_prompt(body: String) -> InteractionDefinition {
    interaction_form::text_field(body, String::new())
}

// ---------------------------------------------------------------------------
// Tests — fully mocked: plain strings in, plain values out. No filesystem, no
// terminal, no clock, no `PromptWindow`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod d1a {
    use super::*;
    use newt_core::markup::plain;
    use newt_interaction::ControlKind;

    const LABEL: &str = "crews/home.toml (crew 'home')";

    fn form_for(starting: &Crew) -> CrewForm {
        CrewForm::new(starting, "Creating crew 'home'", LABEL)
    }

    fn form() -> CrewForm {
        form_for(&Crew::default())
    }

    /// Feed answers in order, asserting the form is still asking for each.
    fn walk(form: &mut CrewForm, answers: &[&str]) -> Vec<Fold> {
        answers
            .iter()
            .map(|a| {
                assert!(form.prompt().is_some(), "form finished before {a:?}");
                form.answer(a)
            })
            .collect()
    }

    /// The nine fields, then the write confirm.
    const ALL_FIELDS: &[&str] = &[
        "planner",
        "navigator",
        "triage",
        "patch-revise",
        "just check",
        "4",
        "5",
        "600",
        "auth, crypto",
    ];

    fn rendered(form: &CrewForm) -> String {
        plain::render(&form.prompt().expect("a prompt"))
    }

    #[test]
    fn every_field_lands_where_the_console_form_put_it() {
        let mut f = form();
        walk(&mut f, ALL_FIELDS);
        walk(&mut f, &["y"]);
        assert!(f.prompt().is_none(), "the confirm ends the form");

        let crew = f.finish();
        assert_eq!(crew.planner, "planner");
        assert_eq!(crew.navigator.as_deref(), Some("navigator"));
        assert_eq!(crew.triage.as_deref(), Some("triage"));
        assert_eq!(crew.loop_program.as_deref(), Some("patch-revise"));
        assert_eq!(crew.test.as_deref(), Some("just check"));
        let b = crew.budgets.expect("budgets were answered");
        assert_eq!(b.max_attempts, Some(4));
        assert_eq!(b.max_files_touched, Some(5));
        assert_eq!(b.max_lines_changed, Some(600));
        // Comma-split, trimmed, empties dropped.
        assert_eq!(b.require_human_review_on, vec!["auth", "crypto"]);
    }

    #[test]
    fn blank_keeps_the_current_value_and_dash_clears_it() {
        let existing = Crew {
            planner: "planner".into(),
            navigator: Some("navigator".into()),
            triage: Some("triage".into()),
            loop_program: Some("patch-revise".into()),
            role_timeout_secs: None,
            test: Some("just check".into()),
            budgets: Some(CrewBudgets {
                max_attempts: Some(3),
                require_human_review_on: vec!["auth".into()],
                ..Default::default()
            }),
        };
        let mut f = form_for(&existing);
        // Blank through everything except: clear the navigator, bump attempts,
        // clear the review topics.
        walk(&mut f, &["", "-", "", "", "", "5", "", "", "-"]);
        walk(&mut f, &["y"]);

        let crew = f.finish();
        assert_eq!(crew.planner, "planner", "blank kept the required field");
        assert_eq!(crew.navigator, None, "'-' cleared the navigator");
        assert_eq!(crew.triage.as_deref(), Some("triage"));
        assert_eq!(crew.loop_program.as_deref(), Some("patch-revise"));
        assert_eq!(crew.test.as_deref(), Some("just check"));
        let b = crew.budgets.expect("attempts survived");
        assert_eq!(b.max_attempts, Some(5));
        assert!(b.require_human_review_on.is_empty(), "'-' cleared the list");
    }

    #[test]
    fn an_all_empty_budget_block_collapses_to_none() {
        let mut f = form();
        walk(&mut f, &["planner", "", "", "", "", "", "", "", ""]);
        walk(&mut f, &["y"]);
        assert_eq!(
            f.finish().budgets,
            None,
            "an untouched budget block stays out of the file"
        );
    }

    #[test]
    fn a_bad_number_reasks_the_same_field_and_says_why() {
        let mut f = form();
        walk(&mut f, &["planner", "", "", "", ""]);
        let before = rendered(&f);
        assert_eq!(f.answer("not-a-number"), Fold::Retry);
        let after = rendered(&f);
        assert!(
            after.contains("max attempts"),
            "the SAME field is re-asked, not the next one: {after}"
        );
        assert!(
            after.contains("not a number"),
            "the reason travels in the definition, not in a side channel: {after}"
        );
        assert!(
            !before.contains("not a number"),
            "the first ask carried no complaint"
        );
        // ...and the retry accepts a real number and moves on.
        assert_eq!(f.answer("7"), Fold::Advanced);
        assert!(
            rendered(&f).contains("max files touched"),
            "advanced past the field it was stuck on"
        );
    }

    /// **The empty answer must not write the file.**
    ///
    /// Regression for the pre-D1a behaviour: the confirm was
    /// `is_yes(&ans, true)`, so an empty answer meant YES. Under a pipe that
    /// was not a style question — `StdinConsole::ask` returned `""` at EOF, so
    /// `newt crew edit < short-script` wrote the crew file on running out of
    /// input. It also violates the epic's rule that a surface never chooses a
    /// default for a decision.
    #[test]
    fn the_write_confirm_refuses_an_empty_answer() {
        let mut f = form();
        walk(&mut f, ALL_FIELDS);
        assert_eq!(f.answer(""), Fold::Retry, "blank decides nothing");
        assert!(
            f.prompt().is_some(),
            "the confirm is still pending — nothing was written"
        );
        assert!(
            rendered(&f).contains("answer y or n"),
            "and it says why: {}",
            rendered(&f)
        );
    }

    /// The anti-vacuous twin of the test above: the confirm is refusing
    /// blank specifically, not refusing everything. If this ever failed, the
    /// guard above would be passing for the wrong reason.
    #[test]
    fn the_write_confirm_accepts_y_and_its_aliases() {
        for yes in ["y", "Y", "yes"] {
            let mut f = form();
            walk(&mut f, ALL_FIELDS);
            assert_eq!(f.answer(yes), Fold::Advanced, "{yes:?} confirms");
            assert!(f.prompt().is_none(), "{yes:?} finished the form");
        }
        for no in ["n", "N", "no"] {
            let mut f = form();
            walk(&mut f, ALL_FIELDS);
            assert_eq!(f.answer(no), Fold::Declined, "{no:?} declines");
        }
        // And a word that is neither is refused rather than guessed.
        let mut f = form();
        walk(&mut f, ALL_FIELDS);
        assert_eq!(f.answer("maybe"), Fold::Retry);
    }

    /// `yes`/`no` resolve through `newt_interaction::binding::resolve_typed`
    /// — the one resolver D0 (#1878) consolidated onto. The proof is a
    /// property only that resolver has: an ALIAS never shadows another
    /// option's canonical key, and an unrecognised answer refuses rather than
    /// falling back. `is_yes` had neither.
    #[test]
    fn the_confirm_resolves_through_the_one_resolver() {
        let definition = form_for(&Crew::default()).write_confirm();
        let ControlKind::Choice { options } = &definition.controls[0].kind else {
            panic!("the confirm is a choice control");
        };
        use newt_interaction::binding::resolve_typed;
        assert_eq!(resolve_typed(&options[..], "y").unwrap().as_str(), "yes");
        assert_eq!(resolve_typed(&options[..], "no").unwrap().as_str(), "no");
        assert!(resolve_typed(&options[..], "").is_none());
        assert!(resolve_typed(&options[..], "sure").is_none());
    }

    /// Fail-closed against an input with nothing left to give: an unusable
    /// answer is re-asked, but not forever.
    #[test]
    fn repeated_unusable_answers_give_up_rather_than_loop() {
        let mut f = form();
        walk(&mut f, ALL_FIELDS);
        let folds: Vec<Fold> = (0..MAX_RETRIES + 1).map(|_| f.answer("")).collect();
        assert_eq!(
            folds.last(),
            Some(&Fold::Declined),
            "gave up after {MAX_RETRIES} refusals: {folds:?}"
        );
        assert!(
            folds[..usize::from(MAX_RETRIES) - 1]
                .iter()
                .all(|f| *f == Fold::Retry),
            "and re-asked before giving up: {folds:?}"
        );
    }

    /// A counter for the retry cap must not leak across fields: three bad
    /// numbers spread over three different fields is not "giving up".
    #[test]
    fn the_retry_cap_resets_when_a_field_is_answered() {
        let mut f = form();
        walk(&mut f, &["planner", "", "", "", ""]);
        for _ in 0..3 {
            assert_eq!(f.answer("nope"), Fold::Retry);
            assert_eq!(f.answer("1"), Fold::Advanced);
        }
        assert!(f.prompt().is_some(), "still asking, not given up");
    }

    #[test]
    fn the_confirm_previews_the_exact_bytes_a_write_would_emit() {
        let mut f = form();
        walk(&mut f, ALL_FIELDS);
        let shown = rendered(&f);
        let bytes = toml::to_string_pretty(&f.finish()).expect("a crew serializes");
        assert!(
            shown.contains(&bytes),
            "the confirm shows the file verbatim.\nshown:\n{shown}\nbytes:\n{bytes}"
        );
        assert!(shown.contains(LABEL), "and names the destination: {shown}");
    }

    /// The plain projection advertises no default for the decision — house
    /// style is `[y]es`/`[n]o`, never a capitalised `[Y/n]`.
    #[test]
    fn the_confirm_renders_no_default() {
        let shown = plain::render(&form_for(&Crew::default()).write_confirm());
        assert!(
            shown.contains("[y]es") && shown.contains("[n]o"),
            "both options are offered: {shown}"
        );
        assert!(
            !shown.contains("[Y/n]") && !shown.contains("[y/N]"),
            "no default is rendered: {shown}"
        );
    }

    /// The terminal adapter returns a line VERBATIM (whitespace can be
    /// meaningful at a free-text prompt). A crew field's cannot be, and
    /// `StdinConsole::ask` trimmed — so the form trims, and ` 4 ` still works.
    #[test]
    fn answers_are_trimmed() {
        let mut f = form();
        walk(&mut f, &["  planner  ", "", "", "", "", " 4 ", "", "", ""]);
        assert_eq!(f.answer("  y  "), Fold::Advanced);
        let crew = f.finish();
        assert_eq!(crew.planner, "planner");
        assert_eq!(crew.budgets.expect("attempts").max_attempts, Some(4));
    }

    /// Each field says what it is and what its current value is, in the
    /// definition — not in a string the driver pre-rendered for a terminal.
    #[test]
    fn every_field_carries_its_own_prompt_and_current_value() {
        let existing = Crew {
            planner: "pl".into(),
            navigator: Some("nav".into()),
            ..Default::default()
        };
        let mut f = form_for(&existing);
        let mut seen = Vec::new();
        for answer in ALL_FIELDS {
            seen.push(rendered(&f));
            f.answer(answer);
        }
        seen.push(rendered(&f));
        assert_eq!(seen.len(), 10, "nine fields and a confirm");
        assert!(seen[0].contains("Creating crew 'home'"), "{}", seen[0]);
        assert!(seen[0].contains("planner loadout"), "{}", seen[0]);
        assert!(
            seen[0].contains("[pl]"),
            "shows the current value: {}",
            seen[0]
        );
        assert!(seen[1].contains("[nav]"), "{}", seen[1]);
        assert!(
            seen[1].contains("'-' clears it"),
            "an optional field says so: {}",
            seen[1]
        );
        assert!(
            !seen[1].contains("Creating crew"),
            "the header is shown once, not on every field: {}",
            seen[1]
        );
        assert!(seen[8].contains("comma-separated"), "{}", seen[8]);
        assert!(seen[9].contains("Write it?"), "{}", seen[9]);
    }
}
