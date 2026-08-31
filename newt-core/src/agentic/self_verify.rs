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
}

impl VerifyCheck {
    fn new(label: impl Into<String>, markers: &[&str]) -> Self {
        Self {
            label: label.into(),
            run_markers: markers.iter().map(|m| m.to_ascii_lowercase()).collect(),
        }
    }

    /// Was this check run by `command` (a single run_command invocation)?
    fn run_by(&self, command_lc: &str) -> bool {
        self.run_markers.iter().any(|m| command_lc.contains(m))
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
            checks.push(VerifyCheck::new(
                "the Python tests (`pytest` / running the test file)",
                &[
                    "pytest",
                    "unittest",
                    "py.test",
                    "python -m test",
                    "test_",
                    "_test.py",
                ],
            ));
        }
        match el.as_str() {
            "makefile" => checks.push(VerifyCheck::new(
                "`make test` (the Makefile)",
                &["make test", "make check", "make ci"],
            )),
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
                checks.push(VerifyCheck::new(
                    "`cargo test`",
                    &["cargo test", "cargo nextest"],
                ));
            }
            "go.mod" => checks.push(VerifyCheck::new("`go test ./...`", &["go test"])),
            _ => {}
        }
    }

    if let Some(cmd) = instruction_verify_command(instruction) {
        let marker = cmd.to_ascii_lowercase();
        checks.push(VerifyCheck::new(
            format!("the command the task says to run: `{cmd}`"),
            &[marker.as_str()],
        ));
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
/// from the assistant `tool_calls` in `messages` (the raw args, before the trace
/// digests them) — so the loop needs no separate accumulator. Pure over the JSON.
pub fn commands_from_messages(messages: &[serde_json::Value]) -> Vec<String> {
    let mut out = Vec::new();
    for m in messages {
        let Some(calls) = m.get("tool_calls").and_then(|c| c.as_array()) else {
            continue;
        };
        for call in calls {
            let f = &call["function"];
            if f["name"].as_str() != Some("run_command") {
                continue;
            }
            // arguments is a JSON *string* (OpenAI wire) or an object.
            let args = &f["arguments"];
            let cmd = if let Some(s) = args.as_str() {
                serde_json::from_str::<serde_json::Value>(s)
                    .ok()
                    .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(String::from))
            } else {
                args.get("command")
                    .and_then(|c| c.as_str())
                    .map(String::from)
            };
            if let Some(c) = cmd {
                out.push(c);
            }
        }
    }
    out
}

/// Is the self-verify gate enabled? OFF by default (behaviour-preserving — the
/// gate changes when a turn is allowed to conclude, so it must be opt-in), turned
/// on with `NEWT_SELF_VERIFY=1` (the headless bench lane sets it). Kept an env
/// toggle to match the session-scoped `NEWT_NUDGE` / `NEWT_FULL_ACCESS` pattern.
pub fn enabled() -> bool {
    std::env::var("NEWT_SELF_VERIFY")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "on" | "true"))
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
