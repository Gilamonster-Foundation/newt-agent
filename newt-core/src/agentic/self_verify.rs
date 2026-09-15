//! Self-verify gate — before the model concludes a turn, has it actually run the
//! task's own tests / an obvious check?
//!
//! The measured #1 capability lever (2026-07-28 Terminal-Bench taxonomy, 12 of 27
//! failures): the agent **declares done on a broken solution** because it never
//! ran the verification the workspace already ships. `cobol-modernization`'s
//! `program.py` crashes on a single run; `constraints-scheduling` books a slot
//! violating a stated hard rule a checker would catch; many tasks include a
//! `test_*.py` (and say "you can run it to verify") the agent ignores.
//!
//! This module is the PURE decision core: given the workspace's top-level
//! entries, the instruction, and the shell commands the model ran this turn, it
//! detects the verifications on offer and returns a nudge naming the ones NOT run — so the
//! loop can hand the model one more round to verify instead of accepting a
//! finish. It renders no output and touches no filesystem; the loop supplies the
//! entries (one cheap scan) and the accumulated commands.
//!
//! Complements — does not duplicate — [`crate::verify_gate`] (#73), which is a
//! STATIC control: it resolves a coding turn's Python imports against the
//! authoritative surface and reverts files with fabricated imports. That gate
//! asks "does the code reference things that exist?"; this one asks "did you RUN
//! the check the workspace ships before declaring done?" — a dynamic,
//! run-the-tests signal the static gate can't provide.
//!
//! Precision over recall: a spurious "go run your tests" nudge wastes a round and
//! annoys, so detection favours HIGH-CONFIDENCE signals and treats a check as
//! satisfied on any plausible run marker.

/// A verification the workspace affords that the model could run before
/// concluding. Data only — the loop turns it into a nudge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyCheck {
    /// Model-facing description of what to run (e.g. "the test file `test_x.py`"
    /// or "`make test`").
    pub label: String,
    /// Lower-case substrings whose presence in a run_command marks this check as
    /// having been run this turn. Any match satisfies the check.
    pub run_markers: Vec<String>,
    /// #2374: the lower-case forms that actually RUN the check when a command
    /// segment starts with one. Result-aware mode takes pass evidence only from
    /// these, never from a marker substring (`cat test_x.py` names a test file
    /// and runs nothing).
    pub runners: Vec<String>,
    /// Forms that run no check: a flag anywhere in the runner's arguments
    /// (`--no-run`, `--collect-only`, also as `--flag=value`), compared
    /// case-sensitively, or a runner-relative prefix (`cargo nextest list`).
    pub non_runs: Vec<String>,
    /// The check the task names in backticks matches only that command plus
    /// flags: backticked `make` is not `make install`.
    pub flags_only: bool,
}

impl VerifyCheck {
    fn new(label: impl Into<String>, markers: &[&str]) -> Self {
        let run_markers: Vec<String> = markers.iter().map(|m| m.to_ascii_lowercase()).collect();
        Self {
            label: label.into(),
            runners: run_markers.clone(),
            run_markers,
            non_runs: Vec::new(),
            flags_only: false,
        }
    }

    /// Replace the invocation forms (the markers stay the attempted-mode rule).
    fn runs_as(mut self, runners: &[&str]) -> Self {
        self.runners = runners.iter().map(|r| r.to_ascii_lowercase()).collect();
        self
    }

    fn except(mut self, non_runs: &[&str]) -> Self {
        self.non_runs = non_runs.iter().map(|r| r.to_string()).collect();
        self
    }

    /// Was this check run by `command` (a single run_command invocation)?
    fn run_by(&self, command_lc: &str) -> bool {
        self.run_markers.iter().any(|m| command_lc.contains(m))
    }

    /// How `command` runs this check, when one of its segments starts with a
    /// runner (after env assignments and `time` / `timeout <t>` / `env`).
    fn invocation(&self, command: &str) -> Option<Invocation> {
        let segments = split_command(command);
        let (index, words, runner) =
            segments
                .iter()
                .enumerate()
                .find_map(|(index, (segment, _))| {
                    let words = strip_transparent_prefix(segment);
                    let text = words.join(" ").to_ascii_lowercase();
                    let runner = self.runners.iter().find(|r| {
                        text == **r
                            || text.starts_with(&format!("{r} "))
                            || (r.ends_with('_') && text.starts_with(r.as_str()))
                    })?;
                    Some((index, words, runner.clone()))
                })?;
        let taken = runner.split_whitespace().count();
        let args = if runner.ends_with('_') {
            &words[taken - 1..]
        } else {
            &words[taken..]
        };
        if self.flags_only && args.iter().any(|a| !a.starts_with('-')) {
            return None;
        }
        let text = words.join(" ").to_ascii_lowercase();
        let non_run = self.non_runs.iter().any(|form| {
            if form.starts_with('-') {
                args.iter().any(|a| {
                    a.strip_prefix(form.as_str())
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
                })
            } else {
                text == *form || text.starts_with(&format!("{form} "))
            }
        });
        let last = segments[index + 1..]
            .iter()
            .all(|(next, _)| next.is_empty());
        let backgrounded = segments[index].1 == "&";
        let skippable = segments[..index].iter().any(|(_, sep)| *sep == "||");
        let evidence = last && !backgrounded && !skippable && !non_run;
        let plain = evidence
            && segments[..index]
                .iter()
                .all(|(seg, sep)| *sep == "&&" && seg.split_whitespace().next() == Some("cd"))
            && !segments[index]
                .0
                .replace("2>&1", "")
                .replace("1>&2", "")
                .replace(">&2", "")
                .contains('>');
        Some(Invocation {
            evidence,
            plain,
            normalized: command.split_whitespace().collect::<Vec<_>>().join(" "),
        })
    }
}

/// How one command runs a check.
struct Invocation {
    /// The run's exit status is the check's: the runner is the last segment,
    /// not backgrounded, not skippable by an earlier `||`, and not a non-run
    /// form. Anything else that matches a runner proves nothing.
    evidence: bool,
    /// Evidence with nothing else in the command (after `cd dir &&`) and no
    /// output redirected to a file: the only run that is not itself a mutation.
    plain: bool,
    /// The whole command, whitespace-normalized: only this exact command
    /// clears a failure it produced.
    normalized: String,
}

/// `command` split into `(segment, separator after it)` at `&&`, `||`, `|`,
/// `;`, `&` and newlines, outside quotes (a `\"` inside double quotes does not
/// end them). `2>&1`, `>&2` and `&>` are redirections, not separators.
fn split_command(command: &str) -> Vec<(String, &'static str)> {
    let chars: Vec<char> = command.trim().chars().collect();
    let (mut out, mut cur, mut quote, mut i) = (Vec::new(), String::new(), None::<char>, 0);
    while i < chars.len() {
        let (c, next) = (chars[i], chars.get(i + 1).copied());
        if let Some(q) = quote {
            cur.push(c);
            if q == '"' && c == '\\' {
                if let Some(n) = next {
                    cur.push(n);
                    i += 2;
                    continue;
                }
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        let sep = match (c, next) {
            ('\'' | '"', _) => {
                quote = Some(c);
                None
            }
            ('\\', Some(n)) => {
                cur.push(c);
                cur.push(n);
                i += 2;
                continue;
            }
            ('&', Some('&')) => Some(("&&", 2)),
            ('|', Some('|')) => Some(("||", 2)),
            ('|', Some('&')) => Some(("|", 2)),
            ('|', _) => Some(("|", 1)),
            (';', _) => Some((";", 1)),
            ('\n', _) => Some(("\n", 1)),
            ('&', Some('>')) => None,
            ('&', _) if cur.ends_with('>') => None,
            ('&', _) => Some(("&", 1)),
            _ => None,
        };
        match sep {
            Some((sep, width)) => {
                out.push((cur.trim().to_string(), sep));
                cur.clear();
                i += width;
            }
            None => {
                cur.push(c);
                i += 1;
            }
        }
    }
    out.push((cur.trim().to_string(), ""));
    out
}

/// A segment's words after the prefixes that run what follows them unchanged.
fn strip_transparent_prefix(segment: &str) -> Vec<String> {
    let mut words: Vec<String> = segment.split_whitespace().map(str::to_string).collect();
    loop {
        match words.first().map(String::as_str) {
            Some(w) if w.contains('=') && !w.starts_with('-') => {
                words.remove(0);
            }
            Some("env" | "time" | "nice") => {
                words.remove(0);
            }
            Some("timeout") if words.len() > 2 => {
                words.drain(..2);
            }
            _ => return words,
        }
    }
}

/// #2374: forms of each runner that run no test. Flags match anywhere among the
/// runner's arguments; the rest are command prefixes.
const PYTEST_NON_RUNS: &[&str] = &[
    "--collect-only",
    "--co",
    "--collectonly",
    "--fixtures",
    "--markers",
    "--setup-plan",
    "--version",
    "--help",
    "-h",
];
const CARGO_NON_RUNS: &[&str] = &[
    "--no-run",
    "--list",
    "--help",
    "-h",
    "cargo nextest list",
    "cargo nextest archive",
    "cargo nextest show-config",
];
const GO_NON_RUNS: &[&str] = &["-c", "-list", "--list", "-n"];
const MAKE_NON_RUNS: &[&str] = &["-n", "--dry-run", "--just-print", "--recon"];

/// The non-run forms of the runner a command starts with.
fn non_runs_for(runner: &str) -> &'static [&'static str] {
    match runner.split_whitespace().next() {
        Some("pytest" | "py.test" | "python" | "python3") => PYTEST_NON_RUNS,
        Some("cargo") => CARGO_NON_RUNS,
        Some("go") => GO_NON_RUNS,
        Some("make") => MAKE_NON_RUNS,
        _ => &[],
    }
}

/// Detect the verifications afforded by a workspace's top-level `entries`
/// (file/dir names, not full paths) plus the task `instruction`. Pure.
///
/// High-confidence signals only:
/// - a top-level `test_*.py` / `*_test.py` (and `tests/`, `conftest.py`,
///   `pytest.ini`) ⇒ run the tests with pytest,
/// - a build-tool entrypoint that conventionally carries a `test` target
///   (`Makefile`, `justfile`, `package.json`, `Cargo.toml`, `go.mod`),
/// - the instruction naming a verify/run command in backticks.
pub fn detect_checks(entries: &[String], instruction: &str) -> Vec<VerifyCheck> {
    let mut checks = Vec::new();
    let mut seen_pytest = false;

    for e in entries {
        let el = e.to_ascii_lowercase();
        let is_py_test = (el.starts_with("test_") && el.ends_with(".py"))
            || el.ends_with("_test.py")
            || el == "tests"
            || el == "conftest.py"
            || el == "pytest.ini";
        if is_py_test && !seen_pytest {
            seen_pytest = true;
            checks.push(
                VerifyCheck::new(
                    "the Python tests (`pytest` / running the test file)",
                    &[
                        "pytest",
                        "unittest",
                        "py.test",
                        "python -m test",
                        "test_",
                        "_test.py",
                    ],
                )
                .runs_as(&[
                    "pytest",
                    "py.test",
                    "python -m pytest",
                    "python3 -m pytest",
                    "python -m unittest",
                    "python3 -m unittest",
                    "python test_",
                    "python3 test_",
                ])
                .except(PYTEST_NON_RUNS),
            );
        }
        match el.as_str() {
            "makefile" => checks.push(
                VerifyCheck::new(
                    "`make test` (the Makefile)",
                    &["make test", "make check", "make ci"],
                )
                .except(MAKE_NON_RUNS),
            ),
            "justfile" => checks.push(VerifyCheck::new(
                "`just test` (the justfile)",
                &["just test", "just check"],
            )),
            "package.json" => checks.push(VerifyCheck::new(
                "`npm test` (package.json)",
                &[
                    "npm test",
                    "npm run test",
                    "yarn test",
                    "pnpm test",
                    "npm run check",
                ],
            )),
            "cargo.toml" => {
                // `cargo check` is deliberately NOT a marker (#1942). It is a
                // TYPE-CHECK: it compiles and runs nothing, so accepting it
                // satisfies this gate on a turn that executed no test at all —
                // the "declares done on a broken solution" failure the module
                // doc calls the measured #1 capability lever. A check that its
                // own wrong evidence satisfies is worse than no check, because
                // it reports confidence.
                //
                // `cargo test` is a SUBSTRING marker, so every flag-bearing
                // form already counts (`-p foo`, `--workspace`, `--lib x`) and
                // narrowing costs none of them. `cargo nextest` is listed
                // beside it because it is a different binary running the same
                // tests: a turn that ran it has verified exactly as much, and
                // omitting it would nudge a workspace that had.
                checks.push(
                    VerifyCheck::new("`cargo test`", &["cargo test", "cargo nextest"])
                        .except(CARGO_NON_RUNS),
                );
            }
            "go.mod" => {
                checks.push(VerifyCheck::new("`go test ./...`", &["go test"]).except(GO_NON_RUNS));
            }
            _ => {}
        }
    }

    if let Some(cmd) = instruction_verify_command(instruction) {
        let marker = cmd.to_ascii_lowercase();
        // #2374: the command runs as its last segment after the prefixes that
        // run it unchanged, so `cd app && pytest` and `time make test` match
        // their own runs; it takes that runner's non-run forms.
        let runner = split_command(&cmd)
            .iter()
            .rev()
            .find(|(segment, _)| !segment.is_empty())
            .map(|(segment, _)| strip_transparent_prefix(segment).join(" "))
            .filter(|runner| !runner.is_empty())
            .unwrap_or_else(|| marker.clone());
        let mut named = VerifyCheck::new(
            format!("the command the task says to run: `{cmd}`"),
            &[marker.as_str()],
        )
        .runs_as(&[runner.as_str()])
        .except(non_runs_for(&runner.to_ascii_lowercase()));
        named.flags_only = true;
        checks.push(named);
    }
    checks
}

/// Pull a verify/run command the instruction spells out in backticks near a
/// verify cue ("run", "verify", "check", "test"). Conservative: only the first
/// backticked token-command on such a line, and only if it looks runnable.
fn instruction_verify_command(instruction: &str) -> Option<String> {
    for line in instruction.lines() {
        let ll = line.to_ascii_lowercase();
        let cues = ll.contains("run ")
            || ll.contains("verify")
            || ll.contains("you can run")
            || ll.contains("to check")
            || ll.contains("to test");
        if !cues {
            continue;
        }
        if let Some(start) = line.find('`') {
            if let Some(rel) = line[start + 1..].find('`') {
                let cmd = line[start + 1..start + 1 + rel].trim();
                // Must look like a command: has a letter, no spaces-only, not a
                // bare path/identifier we can't detect being run.
                if cmd.len() >= 3
                    && cmd.contains(char::is_alphabetic)
                    && cmd.split_whitespace().next().is_some_and(|w| {
                        w.chars().all(|c| c.is_alphanumeric() || "._-/".contains(c))
                    })
                {
                    return Some(cmd.to_string());
                }
            }
        }
    }
    None
}

/// The detected checks NOT satisfied by any of the `commands` run this turn.
pub fn unrun<'a>(checks: &'a [VerifyCheck], commands: &[String]) -> Vec<&'a VerifyCheck> {
    let lc: Vec<String> = commands.iter().map(|c| c.to_ascii_lowercase()).collect();
    checks
        .iter()
        .filter(|chk| !lc.iter().any(|cmd| chk.run_by(cmd)))
        .collect()
}

/// The nudge to hand the model when it is concluding with unrun verifications —
/// or `None` when there is nothing to verify (no checks detected) or everything
/// was already run. The loop injects the `Some` text and grants one more round.
pub fn verify_gate_nudge(checks: &[VerifyCheck], commands: &[String]) -> Option<String> {
    let pending = unrun(checks, commands);
    if pending.is_empty() {
        return None;
    }
    let named = pending
        .iter()
        .map(|c| c.label.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    Some(format!(
        "Before you finish: you have NOT run the verification this task ships — {named}. \
         Do not declare the task done on an unverified solution. Run it now with run_command, \
         read the output, and if it fails, FIX the code and run it again until it passes. \
         Only conclude once you have seen it pass (or have proven there is nothing to run)."
    ))
}

/// The `run_command` command strings the model issued this turn, read straight
/// from the requested calls in `messages` (the raw args, before the trace
/// digests them) — so the loop needs no separate accumulator. Pure over the JSON.
///
/// Both call shapes count: an assistant message's `tool_calls` (the Chat wires,
/// arguments a JSON string or an object) and a top-level Responses
/// `function_call` item (#2315). Reading only the first made every run on the
/// Responses wire look unattempted.
pub fn commands_from_messages(messages: &[serde_json::Value]) -> Vec<String> {
    let calls = messages.iter().flat_map(|m| {
        let chat = m
            .get("tool_calls")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .map(|call| &call["function"]);
        let responses = (m["type"] == "function_call").then_some(m);
        chat.chain(responses)
    });
    calls
        .filter(|f| f["name"].as_str() == Some("run_command"))
        .filter_map(|f| {
            let args = &f["arguments"];
            match args.as_str() {
                Some(s) => serde_json::from_str::<serde_json::Value>(s)
                    .ok()?
                    .get("command")?
                    .as_str()
                    .map(String::from),
                None => args.get("command")?.as_str().map(String::from),
            }
        })
        .collect()
}

/// Is the self-verify gate enabled? **ON by default** (#1943), turned off with
/// `NEWT_SELF_VERIFY=0` / `off` / `false`.
///
/// # Why the default flipped, and what it costs
///
/// It shipped opt-in as behaviour-preserving, which was the right call for a
/// gate nobody had run. The measurement that followed is the argument against
/// leaving it there: across the evidence session the gate was consulted on
/// **294 tool calls and evaluated zero times**, because nothing sets the
/// variable. A guard that is dark by default does not preserve behaviour — it
/// preserves the failure it was written to catch, while the repository reads
/// as though the failure is handled.
///
/// **This is a behaviour change for every user**, stated plainly rather than
/// buried: a turn that concludes without running the verification its
/// workspace ships now gets one more round and a nudge naming what it did not
/// run. It is capped (`SELF_VERIFY_CAP`), it steps aside on the final round so
/// it can never burn a turn's last chance, and `/nudge off` disables it along
/// with every other action nudge. The cost of a false nudge is one wasted
/// round; the cost of the miss it replaces is a task declared done on a broken
/// solution, which is this module's reason to exist.
///
/// # Off is explicit, and only explicit
///
/// Only `0`, `off` and `false` disable. An unrecognised value leaves the gate
/// ON, because the default is now on and a typo'd opt-out should not silently
/// restore the dark state this change exists to end — failing toward the gate
/// being armed is the safe direction for a check whose whole failure mode was
/// never running. `NEWT_SELF_VERIFY=1` keeps working, so the headless bench
/// lane that already sets it needs no change.
///
/// Kept an env toggle to match the session-scoped `NEWT_NUDGE` /
/// `NEWT_FULL_ACCESS` pattern.
pub fn enabled() -> bool {
    !std::env::var("NEWT_SELF_VERIFY").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false"
        )
    })
}

/// How many directory levels below the workspace root the scan descends
/// (#1945).
///
/// Three, because that is where the manifests actually are and not further.
/// A one-level scan — what this used to do — cannot see `backend/Cargo.toml`,
/// `services/api/package.json` or `tests/test_solve.py`, so [`detect_checks`]
/// registered nothing and the gate went quiet on exactly the repositories
/// that ship the most verification. Three levels reaches
/// `crates/newt-tuner/Cargo.toml` and `backend/services/api/package.json`;
/// deeper is where the ratio of manifests to directories collapses and the
/// scan starts paying for vendored trees [`SKIP_DIRS`] did not name.
const MAX_DEPTH: usize = 3;

/// Hard ceiling on directory entries examined in one scan.
///
/// The scan runs each time a turn tries to conclude, so "bounded" has to mean
/// bounded on a pathological tree as well as a typical one. A generated or
/// symlink-loopy workspace stops the scan rather than the turn: the names
/// already collected are used, which degrades to a weaker gate instead of a
/// slow one. [`SKIP_DIRS`] keeps a normal repository orders of magnitude
/// under this.
const MAX_ENTRIES: usize = 10_000;

/// The DISTINCT entry names under `root`, to [`MAX_DEPTH`] levels — the input
/// [`detect_checks`] matches on. Pure over the injected `list`.
///
/// # Names, not paths, and deduplicated
///
/// [`detect_checks`] matches bare names (`cargo.toml`, `package.json`), so
/// that is what this yields, and a name found in five places yields one entry.
/// **The dedup is load-bearing, not tidiness**: without it a monorepo with
/// four `Cargo.toml` files registers four identical `cargo test` checks and
/// the nudge names it four times. One-level scanning could not produce a
/// duplicate — one directory has one `Cargo.toml` — so recursion is what
/// introduces the possibility, and this is where it is closed. Case-insensitive,
/// because `detect_checks` lower-cases before matching and `Cargo.toml` beside
/// `cargo.toml` is one check, not two.
///
/// # Pure, with the filesystem injected
///
/// `list` returns one directory's `(name, is_dir)` pairs. Keeping the walk
/// pure is what lets the whole of it — depth bound, ignore-set, budget,
/// dedup — be tested with an in-memory tree and no `tempfile`, which is this
/// repo's unit-tier rule. [`workspace_entries`] is the thin real-fs wrapper.
fn collect_entry_names(
    root: &std::path::Path,
    max_depth: usize,
    budget: usize,
    list: &impl Fn(&std::path::Path) -> Vec<(String, bool)>,
) -> Vec<String> {
    let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut queue: std::collections::VecDeque<(std::path::PathBuf, usize)> =
        std::collections::VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));
    let mut examined = 0usize;

    while let Some((dir, depth)) = queue.pop_front() {
        for (name, is_dir) in list(&dir) {
            examined += 1;
            if examined > budget {
                // Stop scanning, keep what we have: a weaker gate beats a slow
                // turn, and beats a panic on a tree nobody anticipated.
                return seen.into_values().collect();
            }
            if is_dir {
                let skip = crate::verify_gate::SKIP_DIRS.contains(&name.as_str());
                if !skip && depth < max_depth {
                    queue.push_back((dir.join(&name), depth + 1));
                }
                // A directory name is itself a signal (`tests`), so it is
                // collected whether or not it is descended into.
            }
            seen.entry(name.to_ascii_lowercase()).or_insert(name);
        }
    }
    seen.into_values().collect()
}

/// The workspace's entry names for [`detect_checks`], scanned to
/// [`MAX_DEPTH`] levels. A thin fs wrapper over [`collect_entry_names`] (the
/// pure detection and the pure walk are tested with injected entries); an
/// unreadable directory contributes nothing, so a missing/denied dir weakens
/// the gate rather than failing the turn.
///
/// **Symlinked directories are not followed.** `file_type()` does not traverse
/// the final symlink, so a link is identified before it is entered — the same
/// hard boundary [`crate::verify_gate`] draws, and for the stronger reason
/// here that a link into `/usr/lib` would have this gate demand the model run
/// a dependency's test suite. A symlink is still reported as a NAME, because a
/// symlinked `Cargo.toml` is a real manifest.
pub fn workspace_entries(dir: &std::path::Path) -> Vec<String> {
    collect_entry_names(dir, MAX_DEPTH, MAX_ENTRIES, &|d| {
        std::fs::read_dir(d)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter_map(|e| {
                        let name = e.file_name().into_string().ok()?;
                        let ft = e.file_type().ok()?;
                        Some((name, ft.is_dir() && !ft.is_symlink()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

// ---------------------------------------------------------------------------
// #2315: the opt-in result-aware mode. The attempted-check gate above stays the
// default; this mode reads what each check actually did (PR1's `ExecOutcome`)
// and whether a pass is still about the current workspace.
// ---------------------------------------------------------------------------

use crate::ExecOutcome;
use content_addressable::{ContentAddressable, ContentId, RawContentId};

/// Is the result-aware mode requested? **OFF by default**: only `1`, `on` and
/// `true` enable it. It is a mode of the gate, so it also needs [`enabled`].
pub fn outcomes_enabled() -> bool {
    std::env::var("NEWT_VERIFY_OUTCOMES")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "on" | "true"))
}

/// Whether a turn on `kind` has a self-verify gate at all (#2374): SmartHarness
/// verifies on every wire; without it only the OpenAI Chat Completions and
/// Anthropic loops carry the ordinary gate. Headless solve always arms action
/// nudges, so this is the whole instantiation question there.
pub fn verification_gate_present(
    kind: crate::BackendKind,
    api: crate::OpenAiApi,
    smart_harness: bool,
) -> bool {
    smart_harness
        || kind == crate::BackendKind::Anthropic
        || (kind == crate::BackendKind::Openai && api == crate::OpenAiApi::ChatCompletions)
}

/// The receipt entry for this gate (#2314): the mode a turn instantiates, from
/// whether it has a gate ([`verification_gate_present`], action nudges armed)
/// and the two switches the host read, plus the repair allowance when it
/// applies. Pure: the host passes the switches. Per-decision evidence rides the
/// `verification` trace signals.
pub fn verification_receipt(
    gate_present: bool,
    self_verify: bool,
    outcomes: bool,
) -> serde_json::Value {
    let mode = if !gate_present || !self_verify {
        "off"
    } else if outcomes {
        "result_aware"
    } else {
        "attempted"
    };
    let mut receipt = serde_json::json!({ "mode": mode });
    if mode == "result_aware" {
        receipt["repair_allowance"] = serde_json::json!(VERIFY_REPAIR_ALLOWANCE);
    }
    receipt
}

/// Verification nudges (verify, re-verify and repair together) one turn may
/// spend in result-aware mode. A declared constant, not `SELF_VERIFY_CAP`:
/// each nudge buys a primary inference round, and #2313's shared run
/// allowance is meant to govern that spend once it lands — swap this then.
pub const VERIFY_REPAIR_ALLOWANCE: usize = 3;

/// Entry bound for one workspace tree state; the name scan's own bound.
const MAX_TREE_ENTRIES: usize = MAX_ENTRIES;
/// Byte bound for one workspace tree state. Past it the turn falls back to
/// the mutation chain rather than hashing a huge tree at every check.
const MAX_TREE_BYTES: u64 = 64 * 1024 * 1024;

/// One thing the per-tool-result funnel observed, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Observed {
    /// A shell execution with its class. `tree` is the workspace state when a
    /// run passed (`None` when it failed, or when the tree was out of bounds).
    Exec {
        command: String,
        outcome: ExecOutcome,
        tree: Option<ContentId>,
    },
    /// A successful workspace write through a write tool.
    Write,
}

/// The turn's ordered verification observations, fed at the per-tool-result
/// funnel of every loop. Pairs each command with what it actually did, which
/// neither the tool-event ledger (digested args, optional recorder) nor the
/// message history (requests, never results) can.
#[derive(Debug, Default, Clone)]
pub struct VerificationLedger {
    entries: Vec<Observed>,
    /// The instruction checks are detected against, so only a check's own pass
    /// pays for a tree hash.
    task: String,
    /// Result-aware mode is on for this turn ([`ChatCtx::verify_outcomes`] and
    /// the gate switch), decided once by the loop.
    ///
    /// [`ChatCtx::verify_outcomes`]: super::ChatCtx::verify_outcomes
    result_aware: bool,
    /// The workspace's detected checks, scanned (off the async worker) when a
    /// pass needs them and dropped whenever a call may have created a check.
    checks: Option<Vec<VerifyCheck>>,
}

impl VerificationLedger {
    /// A ledger for a turn whose instruction is `task`; it observes nothing
    /// unless `result_aware`.
    pub(crate) fn for_turn(task: &str, result_aware: bool) -> Self {
        Self {
            entries: Vec::new(),
            task: task.to_string(),
            result_aware,
            checks: None,
        }
    }

    /// This turn's detected checks, rescanned after any call that may have
    /// created one. A failed scan is not cached.
    async fn checks(&mut self, workspace: &str) -> &[VerifyCheck] {
        if self.checks.is_none() {
            self.checks = detect_off_worker(workspace, &self.task).await;
        }
        self.checks.as_deref().unwrap_or_default()
    }

    /// Whether this turn runs the result-aware gate.
    pub(crate) fn result_aware(&self) -> bool {
        self.result_aware
    }

    /// Record a shell execution.
    pub fn record_exec(&mut self, command: &str, outcome: ExecOutcome, tree: Option<ContentId>) {
        self.entries.push(Observed::Exec {
            command: command.to_string(),
            outcome,
            tree,
        });
    }

    /// Record a call that may have changed the workspace (and so created a
    /// check).
    pub fn record_write(&mut self) {
        self.checks = None;
        self.entries.push(Observed::Write);
    }

    /// The funnel's single call. A no-op unless result-aware mode is on, so the
    /// default path does no extra work and no workspace scan.
    pub(crate) async fn observe(
        &mut self,
        name: &str,
        args: &serde_json::Value,
        ok: bool,
        execution: Option<ExecOutcome>,
        workspace: &str,
    ) {
        if !self.result_aware {
            return;
        }
        let _ = ok;
        match execution {
            Some(outcome) => {
                let command = match args["command"].as_str() {
                    Some(command) if super::dispatched_tool_name(name) == Some("run_command") => {
                        command.to_string()
                    }
                    _ => format!("{name} {args}"),
                };
                // A command other than a read or a plain run of a known check
                // may have created a check (`cargo new`, a new test file).
                let plain = self.checks.as_ref().is_some_and(|checks| {
                    checks
                        .iter()
                        .any(|check| check.invocation(&command).is_some_and(|i| i.plain))
                });
                if !plain && !super::is_verification_read_command(&command) {
                    self.checks = None;
                }
                // Hash only pass evidence for a detected check; any other
                // command's tree is never read.
                let evidence = outcome == ExecOutcome::Passed
                    && self
                        .checks(workspace)
                        .await
                        .iter()
                        .any(|check| check.invocation(&command).is_some_and(|i| i.evidence));
                let tree = if evidence {
                    tree_state_off_worker(workspace).await
                } else {
                    None
                };
                self.record_exec(&command, outcome, tree);
            }
            // Every call that is not read-only may have changed the tree, ok or
            // not (a failed write can still leave partial bytes).
            None if super::may_change_workspace(name, args) => self.record_write(),
            None => {}
        }
    }

    /// The end reason of a round-cap exit in result-aware mode. When this turn
    /// has a gate (`gate_on`: the ordinary gate, or SmartHarness verification)
    /// and a detected check's latest evidence is a failure, the exit is a scored
    /// `RepairExhausted` and the reclassification is traced. Otherwise, and on
    /// any loop without a gate, it stays `RoundCap`.
    pub(crate) async fn cap_exit_reason(
        &self,
        workspace: &str,
        gate_on: bool,
        round: usize,
        solve_obs: Option<&mut super::observability::SolveObservation>,
    ) -> crate::TurnEndReason {
        if !(self.result_aware && gate_on) {
            return crate::TurnEndReason::RoundCap;
        }
        let scanned = detect_off_worker(workspace, &self.task).await;
        let (decision, report) = conclude(&Conclusion {
            checks: scanned.as_deref().unwrap_or_default(),
            requested: &[],
            ledger: self,
            tree_now: self.tree_now(workspace).await,
            repairs_used: VERIFY_REPAIR_ALLOWANCE,
            rounds_left: false,
        });
        let exhausted = decision == Decision::Stop(crate::TurnEndReason::RepairExhausted);
        if let Some(obs) = solve_obs.filter(|_| exhausted || scanned.is_none()) {
            obs.behavior_signals
                .push(super::observability::BehaviorSignal::Verification {
                    round,
                    decision: if exhausted {
                        "repair_exhausted"
                    } else {
                        SCAN_FAILED
                    }
                    .to_string(),
                    repairs_used: VERIFY_REPAIR_ALLOWANCE,
                    allowance: VERIFY_REPAIR_ALLOWANCE,
                    report,
                });
        }
        if exhausted {
            crate::TurnEndReason::RepairExhausted
        } else {
            crate::TurnEndReason::RoundCap
        }
    }

    /// The current tree state, computed (off the async worker) only when some
    /// recorded pass could need it.
    pub(crate) async fn tree_now(&self, workspace: &str) -> Option<ContentId> {
        let needed = self
            .entries
            .iter()
            .any(|e| matches!(e, Observed::Exec { tree: Some(_), .. }));
        if needed {
            tree_state_off_worker(workspace).await
        } else {
            None
        }
    }
}

/// The trace decision for a conclusion whose check scan failed: it decided
/// with no checks.
const SCAN_FAILED: &str = "check_scan_failed";

/// [`detect_checks`] over a fresh [`workspace_entries`] scan, on the blocking
/// pool. `None` when the scan task failed.
async fn detect_off_worker(workspace: &str, task: &str) -> Option<Vec<VerifyCheck>> {
    let (root, task) = (std::path::PathBuf::from(workspace), task.to_string());
    tokio::task::spawn_blocking(move || detect_checks(&workspace_entries(&root), &task))
        .await
        .ok()
}

/// [`workspace_tree_state`] on the blocking pool: it reads and hashes up to
/// the byte bound, which must not stall an async worker.
async fn tree_state_off_worker(workspace: &str) -> Option<ContentId> {
    let root = std::path::PathBuf::from(workspace);
    tokio::task::spawn_blocking(move || workspace_tree_state(&root))
        .await
        .ok()
        .flatten()
}

/// A concluding answer as a loop sees it: what [`conclude_turn`] needs to
/// detect the checks, read the requests, decide, and record the evidence.
pub(crate) struct Concluding<'a> {
    pub messages: &'a [serde_json::Value],
    pub workspace: &'a str,
    /// The instruction the checks are detected against.
    pub task: &'a str,
    pub rounds_left: bool,
    pub round: usize,
    pub ledger: &'a VerificationLedger,
    pub solve_obs: Option<&'a mut super::observability::SolveObservation>,
}

/// The one result-aware decision every caller uses (the two ordinary gates and
/// SmartHarness), with its evidence recorded as a solve trace signal whenever
/// the workspace affords a check or the scan failed. The scan and the tree
/// state run here, off the async worker, so only a real conclusion pays for
/// them.
pub(crate) async fn conclude_turn(turn: Concluding<'_>, repairs_used: usize) -> Decision {
    let scanned = detect_off_worker(turn.workspace, turn.task).await;
    let checks = scanned.as_deref().unwrap_or_default();
    let requested = commands_from_messages(turn.messages);
    let (decision, report) = conclude(&Conclusion {
        checks,
        requested: &requested,
        ledger: turn.ledger,
        tree_now: turn.ledger.tree_now(turn.workspace).await,
        repairs_used,
        rounds_left: turn.rounds_left,
    });
    if let Some(obs) = turn
        .solve_obs
        .filter(|_| !checks.is_empty() || scanned.is_none())
    {
        obs.behavior_signals
            .push(super::observability::BehaviorSignal::Verification {
                round: turn.round,
                decision: match &decision {
                    _ if scanned.is_none() => SCAN_FAILED.to_string(),
                    Decision::Accept => "accept".to_string(),
                    Decision::Nudge(_) => "nudge".to_string(),
                    Decision::Stop(reason) => serde_json::to_value(reason)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default(),
                },
                repairs_used,
                allowance: VERIFY_REPAIR_ALLOWANCE,
                report,
            });
    }
    decision
}

/// Everything one conclusion decision reads.
pub struct Conclusion<'a> {
    pub checks: &'a [VerifyCheck],
    /// Commands the model requested this turn ([`commands_from_messages`]).
    pub requested: &'a [String],
    pub ledger: &'a VerificationLedger,
    /// The workspace tree state now; `None` when out of bounds or not needed.
    pub tree_now: Option<ContentId>,
    /// Verification nudges already spent this turn.
    pub repairs_used: usize,
    /// Whether a round remains after this one.
    pub rounds_left: bool,
}

/// What the gate does with a concluding answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Deliver the answer.
    Accept,
    /// Hand the model this guidance and one more round.
    Nudge(String),
    /// Deliver the answer and end with this reason.
    Stop(crate::TurnEndReason),
}

/// Which evidence decided whether a pass is still current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateBasis {
    /// The bounded (path, bytes) tree id at pass time equals the one now.
    Tree,
    /// No mutation was observed after the pass (fallback when a tree state was
    /// out of bounds). Fails closed: a non-check command counts as a mutation.
    MutationChain,
}

/// Where one detected check stands at the conclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Stale,
    /// Passed, but a pipe or a later command decided the exit status.
    Unverified,
    Failed,
    TimedOut,
    Denied,
    Unavailable,
    /// Requested, but no execution was observed (a rejected batch).
    Unexecuted,
    NeverRun,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CheckReport {
    pub label: String,
    pub status: CheckStatus,
    /// The workspace state its last run saw, in the report's basis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_at_run: Option<String>,
}

/// The evidence behind one decision, for the solve trace.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VerificationReport {
    pub basis: StateBasis,
    pub checks: Vec<CheckReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_now: Option<String>,
}

/// One entry of the fallback chain: a mutation, addressed through the existing
/// [`crate::event_journal::Journal`] rather than a new hash.
#[derive(serde::Serialize)]
struct Mutation<'a> {
    command: Option<&'a str>,
    outcome: Option<ExecOutcome>,
}

/// Decide what to do with a concluding answer. Pure.
pub fn conclude(c: &Conclusion<'_>) -> (Decision, VerificationReport) {
    // The chain head before each entry, then after the last one, rebuilt from
    // the ordered observations with the checks detected NOW. A plain run of a
    // detected check and a read-only command are not mutations; every other
    // call is one, whether it succeeded or not. A mint failure poisons the
    // chain, which makes every chain-basis pass stale.
    let mut journal = crate::event_journal::Journal::new();
    let mut chain_ok = true;
    let mut heads = Vec::with_capacity(c.ledger.entries.len() + 1);
    heads.push(None);
    for entry in &c.ledger.entries {
        let mutation = match entry {
            Observed::Write => Some(Mutation {
                command: None,
                outcome: None,
            }),
            Observed::Exec {
                command, outcome, ..
            } => (!super::is_verification_read_command(command)
                && !c
                    .checks
                    .iter()
                    .any(|check| check.invocation(command).is_some_and(|i| i.plain)))
            .then_some(Mutation {
                command: Some(command),
                outcome: Some(*outcome),
            }),
        };
        if let Some(mutation) = mutation {
            chain_ok &= journal.append(mutation).is_ok();
        }
        heads.push(journal.head().map(ToString::to_string));
    }
    let chain_now = heads.last().cloned().flatten();

    // Each check's standing from its runs in order, by one rule: only pass
    // evidence moves it to Pass; a failure is cleared only by the same
    // normalized command; a denied or unavailable run, and a non-evidence run,
    // never erases a Pass or a Fail.
    enum Standing<'a> {
        None,
        Pass(usize, &'a Option<ContentId>),
        Unverified(usize),
        Fail(ExecOutcome, String),
        Blocked(ExecOutcome, usize),
    }
    let standing = |check: &VerifyCheck| {
        let mut state = Standing::None;
        for (i, entry) in c.ledger.entries.iter().enumerate() {
            let Observed::Exec {
                command,
                outcome,
                tree,
            } = entry
            else {
                continue;
            };
            let Some(run) = check.invocation(command) else {
                continue;
            };
            state = match (run.evidence, *outcome, state) {
                (false, _, kept @ (Standing::Pass(..) | Standing::Fail(..))) => kept,
                (false, _, _) => Standing::Unverified(i),
                (true, ExecOutcome::Passed, Standing::Fail(kind, failed))
                    if failed != run.normalized =>
                {
                    Standing::Fail(kind, failed)
                }
                (true, ExecOutcome::Passed, _) => Standing::Pass(i, tree),
                (true, kind @ (ExecOutcome::Failed | ExecOutcome::TimedOut), _) => {
                    Standing::Fail(kind, run.normalized)
                }
                (true, _, kept @ (Standing::Pass(..) | Standing::Fail(..))) => kept,
                (true, kind, _) => Standing::Blocked(kind, i),
            };
        }
        state
    };
    let standings: Vec<Standing> = c.checks.iter().map(standing).collect();
    let basis = if c.tree_now.is_some()
        && standings
            .iter()
            .all(|s| !matches!(s, Standing::Pass(_, None)))
    {
        StateBasis::Tree
    } else {
        StateBasis::MutationChain
    };
    let state_now = match basis {
        StateBasis::Tree => c.tree_now.map(|id| id.to_string()),
        StateBasis::MutationChain => chain_now.clone(),
    };
    let chain_at = |i: usize| heads[i + 1].clone();

    let reports: Vec<CheckReport> = c
        .checks
        .iter()
        .zip(&standings)
        .map(|(check, standing)| {
            let (status, state_at_run) = match standing {
                Standing::None if c.requested.iter().any(|r| check.invocation(r).is_some()) => {
                    (CheckStatus::Unexecuted, None)
                }
                Standing::None => (CheckStatus::NeverRun, None),
                Standing::Unverified(i) => (CheckStatus::Unverified, chain_at(*i)),
                Standing::Fail(kind, _) => (
                    if *kind == ExecOutcome::TimedOut {
                        CheckStatus::TimedOut
                    } else {
                        CheckStatus::Failed
                    },
                    None,
                ),
                Standing::Blocked(kind, i) => (
                    if *kind == ExecOutcome::Denied {
                        CheckStatus::Denied
                    } else {
                        CheckStatus::Unavailable
                    },
                    chain_at(*i),
                ),
                Standing::Pass(i, tree) => {
                    let at_run = match basis {
                        StateBasis::Tree => tree.map(|id| id.to_string()),
                        StateBasis::MutationChain => chain_at(*i),
                    };
                    let fresh = match basis {
                        StateBasis::Tree => at_run == state_now,
                        StateBasis::MutationChain => chain_ok && at_run == chain_now,
                    };
                    (
                        if fresh {
                            CheckStatus::Passed
                        } else {
                            CheckStatus::Stale
                        },
                        at_run,
                    )
                }
            };
            CheckReport {
                label: check.label.clone(),
                status,
                state_at_run,
            }
        })
        .collect();

    let decision = decide(c, &reports);
    (
        decision,
        VerificationReport {
            basis,
            checks: reports,
            state_now,
        },
    )
}

fn decide(c: &Conclusion<'_>, reports: &[CheckReport]) -> Decision {
    let with = |wanted: &[CheckStatus]| {
        reports
            .iter()
            .filter(|r| wanted.contains(&r.status))
            .map(|r| (r.label.as_str(), r.status))
            .collect::<Vec<_>>()
    };
    let broken = with(&[CheckStatus::Failed, CheckStatus::TimedOut]);
    let unverified = with(&[
        CheckStatus::NeverRun,
        CheckStatus::Stale,
        CheckStatus::Unverified,
    ]);
    if reports.iter().all(|r| r.status == CheckStatus::Passed) {
        return Decision::Accept;
    }
    let n = c.repairs_used + 1;
    let can_nudge = c.repairs_used < VERIFY_REPAIR_ALLOWANCE && c.rounds_left;
    if can_nudge && !broken.is_empty() {
        let items = broken
            .iter()
            .map(|(label, status)| match status {
                CheckStatus::TimedOut => format!(
                    "{label} timed out: it did not finish, so find what hangs or makes it slow \
                     rather than treating this as a build error"
                ),
                _ => format!("{label} ran and failed"),
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Decision::Nudge(format!(
            "Before you finish (verification repair {n}/{VERIFY_REPAIR_ALLOWANCE}): {items}. \
             Read its output above, fix the cause, and run it again. Conclude only after you \
             have seen it pass."
        ));
    }
    if can_nudge && !unverified.is_empty() {
        let items = unverified
            .iter()
            .map(|(label, status)| match status {
                CheckStatus::Stale => format!(
                    "{label} passed, but the workspace changed after it ran, so run it again on \
                     the current state"
                ),
                CheckStatus::Unverified => format!(
                    "{label} was run with its exit status hidden by a pipe or a later command, \
                     so run it again on its own without piping or masking its exit status"
                ),
                _ => format!("{label} has not been run"),
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Decision::Nudge(format!(
            "Before you finish (verification {n}/{VERIFY_REPAIR_ALLOWANCE}): {items}. Run it with \
             run_command and read the output before you conclude."
        ));
    }
    Decision::Stop(if broken.is_empty() {
        crate::TurnEndReason::VerificationIncomplete
    } else {
        crate::TurnEndReason::RepairExhausted
    })
}

/// The bounded content id of a workspace tree: a canonical map of relative
/// path to [`RawContentId`] of the file's bytes, skipping
/// [`crate::verify_gate::SKIP_DIRS`]. `None` when the tree exceeds
/// `max_entries` or `max_bytes`, or a file cannot be read: the caller then
/// falls back to the mutation chain. Pure over the injected `list`, `size` and
/// `read`; `size` is consulted first, so no file past the remaining byte budget
/// is ever read into memory.
pub(crate) fn tree_state(
    root: &std::path::Path,
    list: &impl Fn(&std::path::Path) -> Vec<(String, bool)>,
    size: &impl Fn(&std::path::Path) -> Option<u64>,
    read: &impl Fn(&std::path::Path) -> Option<Vec<u8>>,
    max_entries: usize,
    max_bytes: u64,
) -> Option<ContentId> {
    #[derive(serde::Serialize)]
    struct TreeState {
        files: std::collections::BTreeMap<String, RawContentId>,
    }
    impl ContentAddressable for TreeState {
        fn canonical_form(&self) -> Result<Vec<u8>, content_addressable::ContentError> {
            content_addressable::canonical::to_canonical_dagcbor(self)
        }
    }
    let mut files = std::collections::BTreeMap::new();
    let (mut entries, mut bytes) = (0usize, 0u64);
    let mut queue = std::collections::VecDeque::from([(root.to_path_buf(), String::new())]);
    while let Some((dir, prefix)) = queue.pop_front() {
        for (name, is_dir) in list(&dir) {
            entries += 1;
            if entries > max_entries {
                return None;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if is_dir {
                if !crate::verify_gate::SKIP_DIRS.contains(&name.as_str()) {
                    queue.push_back((dir.join(&name), rel));
                }
                continue;
            }
            let path = dir.join(&name);
            if bytes.saturating_add(size(&path)?) > max_bytes {
                return None;
            }
            let content = read(&path)?;
            bytes = bytes.saturating_add(content.len() as u64);
            if bytes > max_bytes {
                return None;
            }
            files.insert(rel, RawContentId::from_content(&content));
        }
    }
    TreeState { files }.content_id().ok()
}

/// [`tree_state`] over the real filesystem. Directories that are symlinks are
/// not entered; a symlink's state is its target path, and anything that is not
/// a regular file or a symlink (a FIFO, a socket) is refused rather than read,
/// so the turn falls back to the mutation chain instead of blocking.
pub fn workspace_tree_state(root: &std::path::Path) -> Option<ContentId> {
    tree_state(
        root,
        &|dir| {
            std::fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(Result::ok)
                        .filter_map(|e| {
                            let name = e.file_name().into_string().ok()?;
                            let ft = e.file_type().ok()?;
                            Some((name, ft.is_dir() && !ft.is_symlink()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        },
        &|path| {
            let meta = std::fs::symlink_metadata(path).ok()?;
            (meta.file_type().is_symlink() || meta.is_file()).then_some(meta.len())
        },
        &|path| {
            let meta = std::fs::symlink_metadata(path).ok()?;
            if meta.file_type().is_symlink() {
                std::fs::read_link(path)
                    .ok()
                    .map(|target| target.to_string_lossy().into_owned().into_bytes())
            } else if meta.is_file() {
                std::fs::read(path).ok()
            } else {
                None
            }
        },
        MAX_TREE_ENTRIES,
        MAX_TREE_BYTES,
    )
}

#[cfg(test)]
#[path = "self_verify_outcomes_tests.rs"]
mod outcomes_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_a_python_test_file_once() {
        let c = detect_checks(
            &entries(&["solution.py", "test_outputs.py", "README.md"]),
            "",
        );
        assert_eq!(c.len(), 1);
        assert!(c[0].label.contains("Python tests"));
        // A second test file does not add a duplicate pytest check.
        let c2 = detect_checks(&entries(&["test_a.py", "b_test.py", "tests"]), "");
        assert_eq!(c2.len(), 1, "pytest detected once, not per file");
    }

    #[test]
    fn detects_build_tool_test_entrypoints() {
        let labels: Vec<String> = detect_checks(
            &entries(&[
                "Makefile",
                "package.json",
                "Cargo.toml",
                "justfile",
                "go.mod",
            ]),
            "",
        )
        .into_iter()
        .map(|c| c.label)
        .collect();
        assert!(labels.iter().any(|l| l.contains("make test")));
        assert!(labels.iter().any(|l| l.contains("npm test")));
        assert!(labels.iter().any(|l| l.contains("cargo test")));
        assert!(labels.iter().any(|l| l.contains("just test")));
        assert!(labels.iter().any(|l| l.contains("go test")));
    }

    /// **#1942 — `cargo check` is not evidence that tests ran.** It is a
    /// type-check: it compiles and runs nothing. Accepting it satisfies the
    /// gate on a turn that never executed a single test, which is precisely
    /// the "declares done on a broken solution" failure this module's own doc
    /// calls the measured #1 capability lever.
    ///
    /// Red before the fix by construction: `cargo check` was a run marker, so
    /// this turn silenced the nudge.
    #[test]
    fn a_cargo_check_alone_does_not_satisfy_the_cargo_test_check() {
        let checks = detect_checks(&entries(&["Cargo.toml"]), "");
        for only in [
            "cargo check",
            "cargo check --workspace",
            "cargo check --all-targets",
            "cargo clippy --workspace -- -D warnings",
        ] {
            assert!(
                verify_gate_nudge(&checks, &[only.into()]).is_some(),
                "`{only}` runs no test, so the gate must still fire"
            );
        }
    }

    /// The anti-vacuous twin: the markers that DO mean tests ran still
    /// satisfy the check, so the fix above is a narrowing and not a break.
    ///
    /// Both families are here deliberately. `cargo test` is a substring
    /// marker, so every flag-bearing form of it already counts — the fix does
    /// not cost `-p`, `--workspace` or `--lib`. `cargo nextest` is a second
    /// marker because it is a different binary running the same tests, and a
    /// turn that ran it has verified exactly as much.
    #[test]
    fn real_test_runs_still_satisfy_the_cargo_check() {
        let checks = detect_checks(&entries(&["Cargo.toml"]), "");
        for ran in [
            "cargo test",
            "cargo test -p newt-core",
            "cargo test --workspace --all-targets",
            "cargo test --lib self_verify",
            "cargo nextest run",
            "cargo nextest run -p newt-core",
        ] {
            assert_eq!(
                verify_gate_nudge(&checks, &[ran.into()]),
                None,
                "`{ran}` ran the tests, so the gate must be silent"
            );
        }
    }

    #[test]
    fn detects_an_instruction_verify_command() {
        let c = detect_checks(
            &entries(&["main.py"]),
            "Implement the parser. You can run `python check.py` to verify your work.",
        );
        assert!(
            c.iter().any(|c| c.label.contains("python check.py")),
            "{c:?}"
        );
    }

    #[test]
    fn no_checks_when_nothing_verifiable() {
        assert!(detect_checks(&entries(&["notes.txt", "data.csv"]), "Write a poem.").is_empty());
    }

    #[test]
    fn nudge_fires_when_tests_present_but_never_run() {
        let checks = detect_checks(&entries(&["test_outputs.py"]), "");
        // The model only edited + catted, never ran the tests.
        let commands = vec!["cat solution.py".into(), "ls -la".into()];
        let nudge = verify_gate_nudge(&checks, &commands);
        assert!(nudge.is_some());
        assert!(nudge.unwrap().contains("Python tests"));
    }

    #[test]
    fn nudge_silent_once_the_tests_were_run() {
        let checks = detect_checks(&entries(&["test_outputs.py"]), "");
        // A pytest invocation satisfies the check → no nudge.
        let commands = vec!["python -m pytest test_outputs.py -q".into()];
        assert_eq!(verify_gate_nudge(&checks, &commands), None);
    }

    #[test]
    fn nudge_silent_when_the_instruction_command_was_run() {
        let checks = detect_checks(&entries(&["m.py"]), "Run `python check.py` to verify.");
        assert!(verify_gate_nudge(&checks, &["python check.py".into()]).is_none());
        // But if it ran something else, the nudge still fires.
        assert!(verify_gate_nudge(&checks, &["python m.py".into()]).is_some());
    }

    #[test]
    fn nudge_silent_when_no_checks_detected() {
        assert_eq!(verify_gate_nudge(&[], &["anything".into()]), None);
        assert_eq!(
            verify_gate_nudge(&detect_checks(&entries(&["a.txt"]), ""), &[]),
            None
        );
    }

    #[test]
    fn run_marker_match_is_case_insensitive() {
        let checks = detect_checks(&entries(&["Makefile"]), "");
        assert_eq!(verify_gate_nudge(&checks, &["MAKE TEST".into()]), None);
    }

    // ---------------------------------------------------------------
    // #1945 — the scanner has to be able to SEE
    // ---------------------------------------------------------------

    /// An in-memory workspace tree for the pure walk: relative dir path →
    /// `(name, is_dir)` pairs. No `tempfile`, no real fs — the unit-tier rule.
    fn tree(spec: &[(&str, &[(&str, bool)])]) -> impl Fn(&std::path::Path) -> Vec<(String, bool)> {
        let map: std::collections::BTreeMap<String, Vec<(String, bool)>> = spec
            .iter()
            .map(|(dir, kids)| {
                (
                    (*dir).to_string(),
                    kids.iter().map(|(n, d)| ((*n).to_string(), *d)).collect(),
                )
            })
            .collect();
        move |p: &std::path::Path| {
            map.get(&p.to_string_lossy().replace('\\', "/"))
                .cloned()
                .unwrap_or_default()
        }
    }

    /// A monorepo: the manifests are one and two levels down, and `target/`
    /// holds a dependency's manifest that is not the task's.
    const MONOREPO: &[(&str, &[(&str, bool)])] = &[
        (
            ".",
            &[
                ("README.md", false),
                ("backend", true),
                ("crates", true),
                ("target", true),
            ],
        ),
        ("./backend", &[("package.json", false), ("app.js", false)]),
        ("./crates", &[("tuner", true)]),
        ("./crates/tuner", &[("Cargo.toml", false), ("src", true)]),
        ("./target", &[("Cargo.toml", false), ("debug", true)]),
    ];

    /// **The bug (#1945).** At one level — what this scanned before — the root
    /// holds no manifest at all, so `detect_checks` registers NOTHING and the
    /// gate is silent on a repository that ships two test suites. This pins
    /// the old behaviour as the defect rather than describing it in prose.
    #[test]
    fn a_one_level_scan_cannot_see_the_manifests_and_detects_nothing() {
        let names = collect_entry_names(std::path::Path::new("."), 0, MAX_ENTRIES, &tree(MONOREPO));
        assert!(
            detect_checks(&names, "").is_empty(),
            "one level sees only {names:?}"
        );
    }

    /// The fix: at [`MAX_DEPTH`] both manifests are found, one and two levels
    /// down, and each registers its check.
    #[test]
    fn a_manifest_below_the_root_is_detected() {
        let names = collect_entry_names(
            std::path::Path::new("."),
            MAX_DEPTH,
            MAX_ENTRIES,
            &tree(MONOREPO),
        );
        let labels: Vec<String> = detect_checks(&names, "")
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(
            labels.iter().any(|l| l.contains("npm test")),
            "backend/package.json (one level down): {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("cargo test")),
            "crates/tuner/Cargo.toml (two levels down): {labels:?}"
        );
    }

    /// **The walk must not descend into `target/`.** Its `Cargo.toml` is a
    /// build tree's, not the task's — and `debug/` beneath it is where a scan
    /// that ignored the ignore-set would spend the rest of its life.
    ///
    /// The directory NAME is still collected (it is one of the root's
    /// entries); what must not appear is anything from INSIDE it.
    #[test]
    fn the_walk_does_not_descend_into_ignored_directories() {
        let names = collect_entry_names(
            std::path::Path::new("."),
            MAX_DEPTH,
            MAX_ENTRIES,
            &tree(&[
                (
                    ".",
                    &[("target", true), ("node_modules", true), (".git", true)],
                ),
                ("./target", &[("Cargo.toml", false)]),
                ("./node_modules", &[("package.json", false)]),
                ("./.git", &[("Makefile", false)]),
            ]),
        );
        assert!(
            detect_checks(&names, "").is_empty(),
            "nothing inside an ignored dir may register a check: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "target"),
            "the dir NAME is still an entry: {names:?}"
        );
    }

    /// A name found in five places is ONE check. Without the dedup a monorepo
    /// with four manifests nudges four times for the same command — a
    /// duplicate one-level scanning could not produce, so recursion is what
    /// introduces it. Case-insensitive, because matching is.
    #[test]
    fn a_name_found_repeatedly_registers_exactly_one_check() {
        let names = collect_entry_names(
            std::path::Path::new("."),
            MAX_DEPTH,
            MAX_ENTRIES,
            &tree(&[
                (".", &[("a", true), ("b", true), ("Cargo.toml", false)]),
                ("./a", &[("Cargo.toml", false)]),
                ("./b", &[("cargo.toml", false)]),
            ]),
        );
        assert_eq!(
            names
                .iter()
                .filter(|n| n.eq_ignore_ascii_case("cargo.toml"))
                .count(),
            1,
            "{names:?}"
        );
        assert_eq!(detect_checks(&names, "").len(), 1);
    }

    /// The budget stops a pathological tree instead of the turn, and what was
    /// already collected is still used — a weaker gate, never a slow one.
    #[test]
    fn a_pathological_tree_stops_at_the_budget_and_keeps_what_it_found() {
        let wide: Vec<(String, bool)> = (0..500).map(|i| (format!("d{i}"), true)).collect();
        let list = move |p: &std::path::Path| {
            if p.to_string_lossy().matches('/').count() > 6 {
                Vec::new()
            } else {
                wide.clone()
            }
        };
        let names = collect_entry_names(std::path::Path::new("."), 8, 1_200, &list);
        assert!(
            names.len() <= 500,
            "the walk stopped rather than enumerating the tree: {}",
            names.len()
        );
    }

    // ---------------------------------------------------------------
    // #1943 — the gate is armed by default
    // ---------------------------------------------------------------

    /// RAII restore, so an env-mutating test cannot leak into a sibling even
    /// if its body panics. Same shape as `flight_recorder`'s.
    struct EnvRestore {
        saved: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn take() -> Self {
            Self {
                saved: std::env::var_os("NEWT_SELF_VERIFY"),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match &self.saved {
                Some(v) => std::env::set_var("NEWT_SELF_VERIFY", v),
                None => std::env::remove_var("NEWT_SELF_VERIFY"),
            }
        }
    }

    /// **The flip.** With no variable set the gate evaluates — which is the
    /// whole of #1943: the evidence session consulted it on 294 tool calls and
    /// evaluated it zero times, because nothing sets the variable.
    #[serial_test::serial(newt_self_verify_env)]
    #[test]
    fn the_gate_is_armed_with_no_environment_variable_set() {
        let _restore = EnvRestore::take();
        std::env::remove_var("NEWT_SELF_VERIFY");
        assert!(
            enabled(),
            "unset must mean ON — a guard dark by default preserves the \
             failure it was written to catch"
        );
    }

    /// The anti-vacuous twin: the opt-out is real. Without this, `enabled()`
    /// could be `true` unconditionally and the test above would still pass.
    #[serial_test::serial(newt_self_verify_env)]
    #[test]
    fn an_explicit_off_still_disables_the_gate() {
        let _restore = EnvRestore::take();
        for off in ["0", "off", "false", "OFF", " 0 ", "False"] {
            std::env::set_var("NEWT_SELF_VERIFY", off);
            assert!(!enabled(), "NEWT_SELF_VERIFY={off:?} must disable");
        }
    }

    /// The existing opt-IN keeps working, so the headless bench lane that
    /// already sets `NEWT_SELF_VERIFY=1` needs no change — and an
    /// unrecognised value leaves the gate ON, because a typo'd opt-out must
    /// not silently restore the dark state this change exists to end.
    #[serial_test::serial(newt_self_verify_env)]
    #[test]
    fn the_old_opt_in_still_enables_and_a_typo_does_not_disable() {
        let _restore = EnvRestore::take();
        for on in ["1", "on", "true", "banana", ""] {
            std::env::set_var("NEWT_SELF_VERIFY", on);
            assert!(enabled(), "NEWT_SELF_VERIFY={on:?} must leave the gate on");
        }
    }

    /// **The gate is satisfied by the ATTEMPT, not by the result** — which is
    /// what stops it spinning where verification cannot succeed.
    ///
    /// It reads the commands the model ISSUED, from the assistant `tool_calls`;
    /// it never reads a tool result and has no notion of pass or fail. So in an
    /// environment where the check cannot run — no toolchain, no network, a
    /// sandbox that refuses exec — the model attempts it once, the attempt
    /// satisfies the check, and the turn concludes. The cost of an impossible
    /// verification is ONE extra round, not a nudge every round forever.
    ///
    /// The deliberate consequence is that this gate measures "did you try?",
    /// never "did it pass?". Judging the result would need the tool output and
    /// a per-tool notion of success, and a gate that guessed at that would be
    /// the false-confidence failure its own module doc is about.
    #[test]
    fn an_attempt_that_fails_still_satisfies_the_check() {
        let checks = detect_checks(&entries(&["Cargo.toml"]), "");
        let messages = vec![
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "function": {
                        "name": "run_command",
                        "arguments": "{\"command\": \"cargo test --workspace\"}"
                    }
                }]
            }),
            // The attempt blew up. The gate never looks here, and that is the
            // property being pinned.
            serde_json::json!({
                "role": "tool",
                "content": "error: no such command: `test`\ncargo: command not found"
            }),
        ];
        let cmds = commands_from_messages(&messages);
        assert_eq!(
            verify_gate_nudge(&checks, &cmds),
            None,
            "an attempted check satisfies the gate — otherwise a workspace \
             where the check CANNOT run pays a nudge every round"
        );
    }

    #[test]
    fn commands_from_messages_reads_run_command_args_both_wire_shapes() {
        let messages = vec![
            // OpenAI wire: arguments is a JSON string.
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "function": { "name": "run_command", "arguments": "{\"command\": \"pytest -q\"}" }
                }]
            }),
            // Object-args shape + a non-run_command call (ignored).
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [
                    { "function": { "name": "run_command", "arguments": { "command": "make test" } } },
                    { "function": { "name": "read_file", "arguments": { "path": "x" } } }
                ]
            }),
            serde_json::json!({ "role": "user", "content": "hi" }),
        ];
        let cmds = commands_from_messages(&messages);
        assert_eq!(cmds, vec!["pytest -q".to_string(), "make test".to_string()]);
    }

    /// #2315: the Responses wire carries a requested call as a top-level
    /// `function_call` item, not an assistant `tool_calls` entry. Reading only
    /// `tool_calls` made every run on that wire look unattempted, so the gate
    /// nudged a turn whose check had already run.
    #[test]
    fn commands_from_messages_reads_responses_function_call_items() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "fix it"}),
            serde_json::json!({"type": "function_call", "call_id": "c1", "name": "run_command",
                "arguments": "{\"command\": \"pytest -q\"}"}),
            serde_json::json!({"type": "function_call", "call_id": "c2", "name": "read_file",
                "arguments": "{\"path\": \"x\"}"}),
            serde_json::json!({"type": "function_call_output", "call_id": "c1", "output": "ok"}),
        ];
        assert_eq!(
            commands_from_messages(&messages),
            vec!["pytest -q".to_string()]
        );
    }

    #[test]
    fn end_to_end_from_messages_gate_stays_silent_after_tests_run() {
        // tests present, and the model's messages show it ran pytest → no nudge.
        let checks = detect_checks(&entries(&["test_outputs.py"]), "");
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "tool_calls": [{ "function": { "name": "run_command", "arguments": "{\"command\": \"python -m pytest\"}" }}]
        })];
        let cmds = commands_from_messages(&messages);
        assert_eq!(verify_gate_nudge(&checks, &cmds), None);
    }
}
