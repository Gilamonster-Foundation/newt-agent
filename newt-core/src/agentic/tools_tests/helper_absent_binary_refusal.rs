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
//! | not carried, on host | exit 127, no denials, host PATH resolves | ask for `exec:<abs path>` |
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

/// NOT CARRIED, but the host has it: the refusal must name the state, and
/// name the lane that can supply it — as an ABSOLUTE PATH grant, never a
/// basename (#2274 grant shape).
#[test]
fn not_carried_but_present_on_host_names_the_grant_to_ask_for() {
    let present = present_host_binary();
    let msg = super::super::shell::absent_binary_refusal(
        &present,
        &not_found_envelope(&present),
        &empty_exec(),
    )
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
}

/// ABSENT FROM THE HOST: no grant can conjure a binary that is not installed,
/// so the refusal must say so and must NOT coach the model to ask for one.
/// (Coaching a grant that cannot help is the loop the denial journal exists
/// to detect.)
#[test]
fn absent_from_host_says_so_and_does_not_coach_a_useless_grant() {
    let msg = super::super::shell::absent_binary_refusal(
        ABSENT,
        &not_found_envelope(ABSENT),
        &empty_exec(),
    )
    .expect("a 127 with no denials must produce a named refusal");

    assert!(
        msg.contains("not installed on this host"),
        "must name the absent-from-host state, got: {msg}"
    );
    assert!(
        !msg.contains("exec:/"),
        "must NOT ask for a grant that cannot help, got: {msg}"
    );
}

/// DENIED BY A GRANT is a different state with a different remedy, and it is
/// owned by the existing structured-denial path. The absent-binary classifier
/// must decline it rather than relabel a denial as an absence.
#[test]
fn denied_by_grant_is_not_an_absence() {
    assert!(
        super::super::shell::absent_binary_refusal(
            &present_host_binary(),
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
    let not_carried = super::super::shell::absent_binary_refusal(
        &present,
        &not_found_envelope(&present),
        &empty_exec(),
    )
    .expect("not-carried must be named");

    let absent = super::super::shell::absent_binary_refusal(
        ABSENT,
        &not_found_envelope(ABSENT),
        &empty_exec(),
    )
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
        super::super::shell::absent_binary_refusal("echo hi", &ok, &empty_exec()).is_none(),
        "exit 0 must never be rewritten as a refusal"
    );
}

/// A non-zero exit that is NOT 127 is an ordinary command failure (a failing
/// compile, a failing test), not an absence.
#[test]
fn an_ordinary_failure_is_not_an_absence() {
    let failed = serde_json::json!({"exit_code": 1, "stdout": "", "stderr": "boom\n"});
    assert!(
        super::super::shell::absent_binary_refusal("false", &failed, &empty_exec()).is_none(),
        "a plain non-zero exit must not be relabelled as an absence"
    );
}
