//! #2558 HANDOFF item 2 (red first): `cmd f > f` must never truncate `f`.
//!
//! Run 6 of the refactor test destroyed a file this way (`awk … f > f`) —
//! the shell truncates the redirect target before the command reads it.
//! These tests drive REAL dispatch (a tempdir file with known bytes, the
//! genuine confined shell AND the `--yolo` host shell), never a stub, so a
//! passing test proves the file survives, not just that a detector fires.

use super::super::NoMcp;
use super::disable_ocap_tests::{env_lock, EnvVar};
use super::*;
use crate::caveats::Caveats;

async fn run_command(command: &str, ws: &std::path::Path, caveats: &Caveats) -> String {
    execute_tool(
        "run_command",
        &serde_json::json!({ "command": command }),
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

const SEED: &[u8] = b"one\ntwo\nthree\n";

fn seeded_file(ws: &std::path::Path) -> std::path::PathBuf {
    let f = ws.join("f");
    std::fs::write(&f, SEED).unwrap();
    f
}

/// Every refused form, through the CONFINED lane (`Caveats::top()`, no
/// `--yolo`). `f` must be byte-identical after each refusal — nothing ran.
#[tokio::test]
async fn confined_lane_refuses_every_same_file_redirect_form_and_never_touches_the_file() {
    let _l = env_lock().await;
    let _off = EnvVar::unset("NEWT_DISABLE_OCAP");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();

    for command in [
        "awk '{print}' f > f",
        "sort f > f",
        "sed 's/a/b/' f > f",
        "cat f | tr a b > f",
        "tr a b < f > f",
    ] {
        let f = seeded_file(ws.path());
        let out = run_command(command, ws.path(), &caveats).await;
        assert!(
            out.starts_with("error: refusing to run this command"),
            "{command:?} must be refused before any exec: {out}"
        );
        assert!(
            out.contains("truncate") && out.contains("f.tmp"),
            "the refusal must say the redirect would truncate the input, \
             and name the write-to-a-new-name-then-mv fix: {out}"
        );
        assert_eq!(
            std::fs::read(&f).unwrap(),
            SEED,
            "{command:?} must leave f byte-identical — nothing ran"
        );
    }
}

/// The SAME forms, through the HOST lane (`--yolo` / `NEWT_DISABLE_OCAP=1`)
/// — the brief's "both lanes" requirement. `exec_confined_command` checks
/// the guard before branching into either dispatch, so this proves the
/// single check point actually covers the host-bypass branch too.
#[cfg(unix)]
#[tokio::test]
async fn host_lane_refuses_every_same_file_redirect_form_and_never_touches_the_file() {
    let _l = env_lock().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();

    for command in [
        "awk '{print}' f > f",
        "sort f > f",
        "sed 's/a/b/' f > f",
        "cat f | tr a b > f",
        "tr a b < f > f",
    ] {
        let f = seeded_file(ws.path());
        let out = run_command(command, ws.path(), &caveats).await;
        assert!(
            out.starts_with("error: refusing to run this command"),
            "{command:?} must be refused on the host lane too: {out}"
        );
        assert_eq!(
            std::fs::read(&f).unwrap(),
            SEED,
            "{command:?} must leave f byte-identical on the host lane"
        );
    }
}

/// The twin: forms that must keep working, unchanged — a different target,
/// the model's own safe rewrite (write-then-`mv`), and a command with no
/// read of its own target at all. Confined lane.
#[tokio::test]
async fn confined_lane_still_runs_forms_that_are_not_a_same_file_redirect() {
    let _l = env_lock().await;
    let _off = EnvVar::unset("NEWT_DISABLE_OCAP");
    // The `safe-subset` engine's restricted grammar does not actually
    // perform an output redirect (a real exec still returns `(exit 0)`, but
    // no file lands) — this test is about the GUARD, not the engine, so
    // pin `host`: a real `/bin/sh -c` inside the same L3 kernel jail, per
    // `--shell-engine`'s doc ("full grammar"), so the redirect genuinely runs.
    let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "host");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();

    seeded_file(ws.path());
    let out = run_command("awk '{print}' f > g", ws.path(), &caveats).await;
    assert!(
        !out.starts_with("error: refusing to run this command"),
        "a different target must run: {out}"
    );
    assert_eq!(std::fs::read(ws.path().join("g")).unwrap(), SEED);

    let out = run_command("awk '{print}' f > f.tmp && mv f.tmp f", ws.path(), &caveats).await;
    assert!(
        !out.starts_with("error: refusing to run this command"),
        "the model's own safe rewrite (write to a tmp name, then mv) must \
         still run untouched: {out}"
    );
    assert_eq!(
        std::fs::read(ws.path().join("f")).unwrap(),
        SEED,
        "the rewrite must actually preserve the content"
    );

    let out = run_command("echo x > f", ws.path(), &caveats).await;
    assert!(
        !out.starts_with("error: refusing to run this command"),
        "a plain write with no read of its own target must run: {out}"
    );
    assert_eq!(std::fs::read(ws.path().join("f")).unwrap(), b"x\n");
}

/// #2560 round 2, fix 1 (red first): `./f` and `f` name the same file —
/// `resolve_exec_cwd` joins but does not normalize, so this was a miss
/// before `lexically_normalize` was added to the comparison.
#[tokio::test]
async fn confined_lane_refuses_a_dot_slash_spelling_of_the_same_file() {
    let _l = env_lock().await;
    let _off = EnvVar::unset("NEWT_DISABLE_OCAP");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();

    let f = seeded_file(ws.path());
    let out = run_command("cat ./f > f", ws.path(), &caveats).await;
    assert!(
        out.starts_with("error: refusing to run this command"),
        "'./f' and 'f' must compare equal after normalization: {out}"
    );
    assert_eq!(std::fs::read(&f).unwrap(), SEED);
}

/// #2560 round 2, fix 2 (red first): `tee` opens every non-flag operand
/// with `O_TRUNC` at startup, regardless of any `>` redirect — `sort f |
/// tee f` is exactly as destructive as `sort f > f`, and `tee f < f` is
/// the same shape with the read spelled as `<` instead of a plain operand.
#[tokio::test]
async fn confined_lane_refuses_tee_writing_over_a_file_it_reads() {
    let _l = env_lock().await;
    let _off = EnvVar::unset("NEWT_DISABLE_OCAP");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();

    for command in ["sort f | tee f", "tee f < f", "sort f | tee -a f"] {
        let f = seeded_file(ws.path());
        let out = run_command(command, ws.path(), &caveats).await;
        assert!(
            out.starts_with("error: refusing to run this command"),
            "{command:?} must be refused — tee's operand is a write: {out}"
        );
        assert_eq!(
            std::fs::read(&f).unwrap(),
            SEED,
            "{command:?} must leave f byte-identical"
        );
    }
}

/// #2560 round 2, fix 3 (red first): the "smallest narrowing" — a
/// subcommand dispatcher's first operand is a verb, not a path, and a
/// non-reading command like `echo` never truncates what it "reads". Before
/// this fix all three refused on a coincidental name match, costing the
/// model's harmless first instinct (#2558 test 1) for no safety benefit.
///
/// Calls [`same_file_redirect_refusal`] directly rather than through real
/// dispatch: `git diff`/`cargo build` are ROUTED (read-only git → the
/// embedded git tool; build commands → the confined build lane), a
/// pre-existing behavior orthogonal to this guard, and driving them through
/// `execute_tool` in a test was observed to run against the actual process
/// cwd rather than the tempdir passed as `workspace` — a real but separate
/// routing quirk, not something to depend on (or risk polluting the repo
/// with) here. The guard's own decision is what this test is about.
#[test]
fn the_guard_no_longer_false_refuses_subcommand_and_non_reading_forms() {
    for command in ["cargo build > build", "git diff > diff", "echo f > f"] {
        assert!(
            same_file_redirect_refusal(command, "/tmp").is_none(),
            "{command:?} must not be refused by the redirect guard"
        );
    }
}
