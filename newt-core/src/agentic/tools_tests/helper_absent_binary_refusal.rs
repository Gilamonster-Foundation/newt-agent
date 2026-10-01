//! #2274 (part 1) — absence must never be ambiguous.
//!
//! A binary the carried userland does not carry used to reach the model as
//! `error: command exited 127` wrapped around brush's own
//! `command not found: X`. That is indistinguishable from a broken machine,
//! which is why the confined profile read as flakiness for six months instead
//! of as policy.
//!
//! Three DIFFERENT states hide behind that one string, and the model must be
//! able to tell them apart, because the right next move differs in each:
//!
//! | state | envelope | next move |
//! |---|---|---|
//! | denied by a grant | `denied:true`, `denials[kind=exec]`, exit 126 | ask for the grant |
//! | not carried, on host | exit 127, no denials, host PATH resolves | build authority for project validation, or direct `exec:<abs path>` |
//! | not on this host | exit 127, no denials, host PATH misses | no grant helps |
//!
//! The discriminator is STRUCTURED (exit code + the `denials` array), never a
//! stderr grep — the same rule [`super::super::shell::envelope_denied`] follows.

/// A program certain NOT to exist — no grant could ever supply it. Contains no
/// path separator, so it is resolved through `PATH` on every platform and
/// found nowhere.
const ABSENT: &str = "newt-absent-binary-xyzzy-2274";

/// A file certain to be PRESENT on the host, on every platform the suite runs
/// on: this test binary.
///
/// The obvious choice is a well-known command name, and the obvious choice is
/// wrong. `sh` is a Unix assumption; so is every other hardcoded binary name,
/// which only moves the assumption rather than removing it. A Windows runner
/// has no `sh`, the host probe correctly reports "not on this host", and the
/// not-carried case silently becomes the absent case — the test stops
/// exercising the branch it was written for.
///
/// `current_exe()` is definitionally present wherever the test runs, and it is
/// ALREADY ABSOLUTE, which is what this case needs: `host_path_lookup` takes
/// its explicit-path branch on a separator and stats the file directly, so no
/// `PATH` resolution is involved on any platform. It is also the right shape
/// for the `exec:<abs path>` grant assertion (#2274 grants are absolute paths).
///
/// The reaching-outside-the-carried-userland instinct is exactly what carrying
/// brush exists to prevent; the fixture had the same bug the production code
/// does not.
fn present_host_binary() -> String {
    let exe = std::env::current_exe().expect("the running test binary exists");
    assert!(
        exe.is_absolute(),
        "current_exe() must be absolute for the exec:<abs path> assertion: {exe:?}"
    );
    exe.display().to_string()
}

/// brush's shape for "I could not resolve this program at all": exit 127,
/// and — critically — NO `denied` flag and NO `denials` array, because the
/// interceptor was never reached.
fn not_found_envelope(prog: &str) -> serde_json::Value {
    serde_json::json!({
        "exit_code": 127,
        "stdout": "",
        "stderr": format!("error: command not found: {prog}\n"),
    })
}

/// The leash's shape for "this program exists and you may not run it":
/// exit 126 plus the structured refusal.
fn denied_envelope(prog: &str) -> serde_json::Value {
    serde_json::json!({
        "exit_code": 126,
        "stdout": "",
        "stderr": "",
        "denied": true,
        "denials": [{
            "kind": "exec",
            "target": prog,
            "reason": format!("exec of \"{prog}\" is not within the granted authority"),
        }],
    })
}

fn empty_exec() -> crate::caveats::Scope<String> {
    crate::caveats::Scope::none()
}

/// Caveats with the given read scope and an unscoped exec axis — the shape
/// under which a kernel refusal is a missing READ right (#2629).
fn read_scope(fs_read: crate::caveats::Scope<String>) -> crate::caveats::Caveats {
    crate::caveats::Caveats {
        fs_read,
        ..crate::caveats::Caveats::top()
    }
}

/// NOT CARRIED, but the host has it: the refusal must name the state, and
/// name the lane that can supply it — as an ABSOLUTE PATH grant, never a
/// basename (#2274 grant shape).
#[test]
fn not_carried_but_present_on_host_names_the_grant_to_ask_for() {
    let present = present_host_binary();
    let msg =
        super::super::shell::absent_binary_refusal(&not_found_envelope(&present), &empty_exec())
            .expect("a 127 with no denials must produce a named refusal");

    assert!(
        msg.contains("carried userland"),
        "must name the state, got: {msg}"
    );
    // The EXACT grant string, not a `/` prefix: on Windows an absolute path is
    // `C:\...`, so asserting "exec:/" would be the same platform assumption in
    // a different place.
    assert!(
        msg.contains(&format!("exec:{present}")),
        "must name the absolute-path grant to ask for ({present}), got: {msg}"
    );
    assert!(
        !msg.contains("not installed on this host"),
        "the host HAS this binary; must not claim otherwise: {msg}"
    );
    for guidance in [
        "project compiler/test validation",
        "if lifecycle is advertised",
        "prefer lifecycle action=build",
        "explicit offline build authority",
        "does not grant compiler descendants",
        "do not retry a declined grant",
    ] {
        assert!(
            msg.contains(guidance),
            "host-present absence must explain the build route ({guidance}): {msg}"
        );
    }
}

/// ABSENT FROM THE HOST: no grant can conjure a binary that is not installed,
/// so the refusal must say so and must NOT coach the model to ask for one.
/// (Coaching a grant that cannot help is the loop the denial journal exists
/// to detect.)
#[test]
fn absent_from_host_says_so_and_does_not_coach_a_useless_grant() {
    let msg =
        super::super::shell::absent_binary_refusal(&not_found_envelope(ABSENT), &empty_exec())
            .expect("a 127 with no denials must produce a named refusal");

    assert!(
        msg.contains("not installed on this host"),
        "must name the absent-from-host state, got: {msg}"
    );
    assert!(
        !msg.contains("exec:/"),
        "must NOT ask for a grant that cannot help, got: {msg}"
    );
    assert!(
        !msg.contains("lifecycle"),
        "build authority cannot supply a missing host binary: {msg}"
    );
}

/// DENIED BY A GRANT is a different state with a different remedy, and it is
/// owned by the existing structured-denial path. The absent-binary classifier
/// must decline it rather than relabel a denial as an absence.
#[test]
fn denied_by_grant_is_not_an_absence() {
    assert!(
        super::super::shell::absent_binary_refusal(
            &denied_envelope(&present_host_binary()),
            &empty_exec(),
        )
        .is_none(),
        "a structured denial is not an absence and must not be relabelled"
    );
}

/// The whole point of #2274 part 1: the three states are DISTINGUISHABLE.
/// One assertion on one message would not have caught the six-month
/// misdiagnosis — the twin is that no two of them read the same.
#[test]
fn all_three_states_produce_different_messages() {
    let present = present_host_binary();
    let not_carried =
        super::super::shell::absent_binary_refusal(&not_found_envelope(&present), &empty_exec())
            .expect("not-carried must be named");

    let absent =
        super::super::shell::absent_binary_refusal(&not_found_envelope(ABSENT), &empty_exec())
            .expect("absent-from-host must be named");

    let denied = super::super::shell::denied_run_command_result(&denied_envelope(&present), false);

    assert_ne!(not_carried, absent, "not-carried must differ from absent");
    assert_ne!(not_carried, denied, "not-carried must differ from denied");
    assert_ne!(absent, denied, "absent must differ from denied");

    // And each must be classified as a FAILURE by the ledger's prefix test,
    // or the turn records a blocked command as a success (#1969).
    for m in [&not_carried, &absent, &denied] {
        assert!(
            !super::super::tool_result_ok(m),
            "a refusal must not read as success: {m}"
        );
    }
}

/// A command that ran is not an absence — exit 0 must stay untouched, or the
/// classifier would rewrite ordinary output.
#[test]
fn a_successful_command_is_not_an_absence() {
    let ok = serde_json::json!({"exit_code": 0, "stdout": "hi\n", "stderr": ""});
    assert!(
        super::super::shell::absent_binary_refusal(&ok, &empty_exec()).is_none(),
        "exit 0 must never be rewritten as a refusal"
    );
}

/// A non-zero exit that is NOT 127 is an ordinary command failure (a failing
/// compile, a failing test), not an absence.
#[test]
fn an_ordinary_failure_is_not_an_absence() {
    let failed = serde_json::json!({"exit_code": 1, "stdout": "", "stderr": "boom\n"});
    assert!(
        super::super::shell::absent_binary_refusal(&failed, &empty_exec()).is_none(),
        "a plain non-zero exit must not be relabelled as an absence"
    );
}

/// brush's shape for "found it, could not execute it": exit 126 and NO
/// structured denial — the same envelope for a kernel refusal the interceptor
/// never saw and for a script the model forgot to `chmod +x`.
fn exit_126_envelope(stderr: String) -> serde_json::Value {
    serde_json::json!({"exit_code": 126, "stdout": "", "stderr": stderr})
}

/// brush's own wording when it could not execute `prog`, which a permitted
/// script can print verbatim before it `exit 126`s.
fn forged_permission_denied(prog: &str) -> String {
    format!("brush: failed to execute command '{prog}': Permission denied (os error 13)\n")
}

/// #2273 → #2629 round 2: a 126 with NO structured denial is an UNSTRUCTURED
/// failure, and the renderer must say so instead of deciding for the model.
/// The renderer this replaces read the program from brush's `failed to execute
/// command '<x>'` line, resolved it on the host and, when it lay outside the
/// read grant, asserted `capability denied` and named the grant to ask for.
/// A permitted script can print that exact line and `exit 126`: here it
/// names this very test binary, which exists on the host and is outside the
/// empty read grant — the shape that made the old renderer assert. Checked
/// under both exec shapes, because the old renderer chose a different grant
/// for each. RED on 534892e5 (the round-1 head): `capability denied: exec of
/// … at …` plus a `request_permissions(capability="fs_read", …)` call and
/// outcome `Denied`.
#[test]
fn a_forged_permission_denied_on_exit_126_names_no_target_and_no_grant() {
    let present = present_host_binary();
    let envelope = exit_126_envelope(forged_permission_denied(&present));
    let scoped_exec = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::none(),
        exec: crate::caveats::Scope::only(["cargo".to_string()]),
        ..crate::caveats::Caveats::top()
    };
    for caveats in [read_scope(crate::caveats::Scope::none()), scoped_exec] {
        let (text, outcome) = super::super::shell::confined_result(
            "./scripts/check.sh",
            &envelope,
            &caveats,
            false,
            |_| "RENDERED".to_owned(),
        );
        assert_eq!(outcome, crate::ExecOutcome::Failed, "{text}");
        assert!(
            text.starts_with("RENDERED\n("),
            "the child's own output stays first, unannotated: {text}"
        );
        assert!(!text.contains("capability denied"), "{text}");
        assert!(!text.contains("request_permissions"), "{text}");
        assert!(
            !text.contains(&present),
            "the note must name no target — the only path in the result is the child's: {text}"
        );
        assert!(text.contains("exit 126"), "{text}");
        assert!(text.contains("unknown"), "{text}");
    }
}

/// The positive control: the SAME exit 126 carrying the leash's structured
/// refusal still names its axis, its exact target and the one call — and the
/// target is `denials[].target`, never the stderr line, which here names
/// nothing.
#[test]
fn a_structured_exec_denial_on_exit_126_still_names_its_exact_target() {
    let present = present_host_binary();
    let (text, outcome) = super::super::shell::confined_result(
        &present,
        &denied_envelope(&present),
        &read_scope(crate::caveats::Scope::none()),
        false,
        |_| unreachable!("a structured denial is rendered by the denial path"),
    );
    assert_eq!(outcome, crate::ExecOutcome::Denied, "{text}");
    assert!(text.starts_with("capability denied:"), "{text}");
    assert_eq!(text.matches("request_permissions(").count(), 1, "{text}");
    assert!(
        text.contains(&format!(
            r#"request_permissions(capability="exec", target={}"#,
            serde_json::json!(present)
        )),
        "{text}"
    );
}

/// Grants cannot change an unstructured 126: with the read grant covering the
/// binary, with it empty, and with exec unscoped, the result is byte-identical,
/// because nothing is looked up on the host any more. (Before, the "inside the
/// grant" case fell through untouched and the "outside" case was relabelled a
/// denial — a distinction drawn from a host stat of a child-named path.)
#[test]
fn an_unstructured_126_renders_the_same_under_every_grant() {
    let present = present_host_binary();
    let envelope = exit_126_envelope(forged_permission_denied(&present));
    let dir = std::path::Path::new(&present)
        .parent()
        .expect("the test binary has a parent directory")
        .display()
        .to_string();
    let rendered: Vec<_> = [
        crate::caveats::Caveats::top(),
        read_scope(crate::caveats::Scope::none()),
        read_scope(crate::caveats::Scope::only([dir])),
    ]
    .iter()
    .map(|caveats| {
        super::super::shell::confined_result(&present, &envelope, caveats, false, |_| {
            "RENDERED".to_owned()
        })
    })
    .collect();
    assert!(rendered.iter().all(|r| r == &rendered[0]), "{rendered:?}");
    assert_eq!(rendered[0].1, crate::ExecOutcome::Failed);
}

/// Denial accounting: the model's "capability wall" claim is grounded only by
/// the OS's own words in the CHILD's output, never by newt's note. With a
/// silent child the result carries nothing the ground-truth check recognises,
/// so a bare `exit 126` grounds no denial claim; a child that printed
/// `Permission denied` does, on its own words.
#[test]
fn the_unstructured_126_note_is_not_a_denial_in_newts_own_vocabulary() {
    let caveats = crate::caveats::Caveats::top();
    let (silent, outcome) = super::super::shell::confined_result(
        "./x.sh",
        &exit_126_envelope(String::new()),
        &caveats,
        false,
        |_| String::new(),
    );
    assert_eq!(outcome, crate::ExecOutcome::Failed);
    assert!(
        !crate::agentic::run_command_result_is_denial("run_command", false, &silent),
        "{silent}"
    );
    let (spoken, _) = super::super::shell::confined_result(
        "./x.sh",
        &exit_126_envelope("sh: ./x.sh: Permission denied\n".to_owned()),
        &caveats,
        false,
        |envelope| envelope["stderr"].as_str().unwrap_or_default().to_owned(),
    );
    assert!(
        crate::agentic::run_command_result_is_denial("run_command", false, &spoken),
        "{spoken}"
    );
}

/// #2304: in `nope; ABSENT` both lookups fail and the 127 belongs to the last
/// one, so the refusal names the program brush failed on last.
#[test]
fn a_compound_127_names_the_last_program_not_found() {
    let envelope = serde_json::json!({
        "exit_code": 127,
        "stdout": "",
        "stderr": format!("error: command not found: nope\nerror: command not found: {ABSENT}\n"),
    });
    let msg = super::super::shell::absent_binary_refusal(&envelope, &empty_exec())
        .expect("a 127 with no denials must produce a named refusal");
    assert!(msg.starts_with(&format!("error: {ABSENT}:")), "{msg}");
}

/// #2315: a 127 that brush did NOT attribute to a missing program is a check
/// failing on its own terms (a test harness, a `make` recipe), not an absence.
/// Before this fix the leading token stood in for the unnamed program, so the
/// run rendered as `<leading>: not in this profile's carried userland`, the
/// repair router read a blocker no edit can clear, and a real failing check
/// never reached repair. The refusal no longer reads the command at all, so
/// nothing but brush's own naming can mint it.
#[test]
fn a_127_brush_did_not_attribute_to_a_missing_program_is_not_an_absence() {
    let present = present_host_binary();
    let recipe_failed = serde_json::json!({
        "exit_code": 127,
        "stdout": "",
        "stderr": "make: *** [test] Error 127\n",
    });
    assert!(
        super::super::shell::absent_binary_refusal(&recipe_failed, &empty_exec()).is_none(),
        "only brush naming the missing program makes a 127 an absence"
    );
    // Twin: the same command, with brush naming the program, still refuses.
    assert!(super::super::shell::absent_binary_refusal(
        &not_found_envelope(&present),
        &empty_exec(),
    )
    .is_some());
}

/// A later pipeline stage owns the exit code, so `rg … | head` exits 0 with
/// brush's `command not found: rg` on stderr. A live ornith-35b run saw only
/// that bare line three times (`rg`, `timeout`, `mkdir`). The output is kept
/// and the absence is named after it, and the result stays a success.
#[test]
fn a_piped_absence_keeps_its_output_and_names_the_program() {
    let piped = serde_json::json!({
        "exit_code": 0,
        "stdout": "partial\n",
        "stderr": format!("error: command not found: {ABSENT}\n"),
    });
    let (text, outcome) = super::super::shell::confined_result(
        &format!("{ABSENT} x | head"),
        &piped,
        &crate::caveats::Caveats::top(),
        false,
        |_| "partial".to_owned(),
    );
    assert!(text.starts_with("partial\n"), "{text}");
    assert!(
        text.contains(&format!(
            "error: {ABSENT}: {}",
            super::super::shell::ABSENT_BINARY_MARKER
        )),
        "{text}"
    );
    assert_eq!(outcome, crate::ExecOutcome::Passed);
}

/// An ordinary, unrelated failure must fall through untouched as an honest
/// failure — never relabelled into a `capability denied` sandbox denial and
/// never offered an exec-grant target. This pins the reviewer's invariant on
/// #2633: a non-zero exit carries no authoritative sandbox-refusal evidence of
/// its own, so it is rendered as itself (see shell.rs's `confined_result`).
#[test]
fn an_unrelated_failure_is_not_a_child_exec_denial() {
    let envelope = serde_json::json!({
        "exit_code": 1,
        "stdout": "",
        "stderr": "error: test assertion failed\n",
    });
    let (text, outcome) = super::super::shell::confined_result(
        "cargo test",
        &envelope,
        &crate::caveats::Caveats::top(),
        false,
        |_| "error: test assertion failed".to_owned(),
    );
    assert_eq!(text, "error: test assertion failed");
    assert_eq!(outcome, crate::ExecOutcome::Failed);
}

/// A failure whose stderr merely *names* a child but names nothing the
/// sandbox actually refused carries no resolvable, invocation-bound target,
/// so no structured denial is minted — never guess a grant target. This is
/// the guard against re-deriving a sandbox denial from child stderr: only the
/// leash's own structured refusal (#2421) could ever mint one.
#[test]
fn an_unresolvable_child_name_is_not_a_named_denial() {
    let envelope = serde_json::json!({
        "exit_code": 1,
        "stdout": "",
        "stderr": format!("fatal: cannot exec '{ABSENT}': Permission denied\n"),
    });
    let (text, outcome) = super::super::shell::confined_result(
        &format!("git {ABSENT}"),
        &envelope,
        &crate::caveats::Caveats::top(),
        false,
        |_| "fatal: cannot exec".to_owned(),
    );
    assert_eq!(text, "fatal: cannot exec");
    assert_eq!(outcome, crate::ExecOutcome::Failed);
}
