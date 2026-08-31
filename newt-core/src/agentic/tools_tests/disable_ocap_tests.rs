use super::super::NoMcp;
use super::*;
use crate::caveats::{Caveats, CountBound, Scope};
use tokio::sync::{Mutex, MutexGuard};

/// Serializes every test that reads or writes `NEWT_DISABLE_OCAP` (and
/// the venv vars the bypass forwards): the process environment is shared
/// across the parallel test runner. Async-aware (tokio) so the guard may
/// be held across the `execute_tool` awaits; no poisoning — the `EnvVar`
/// guards below restore the environment even on panic.
pub(crate) static ENV_LOCK: Mutex<()> = Mutex::const_new(());

pub(crate) async fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().await
}

/// RAII env override: set/unset `key` for the test body, restore the
/// previous value on drop — including on a failed assertion, so yolo can
/// never leak into a neighboring test.
pub(crate) struct EnvVar {
    key: &'static str,
    saved: Option<String>,
}

impl EnvVar {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        let saved = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, saved }
    }

    fn unset(key: &'static str) -> Self {
        let saved = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { key, saved }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        match self.saved.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// #1600 regression: the confined (safe-subset / brush / host) shell env is an
/// ALLOWLIST — `ConfinedCommand::env_clear` + a narrow passthrough of NAMES
/// (default `HOME`/`USER`) + explicit `~/.newt/shell-env/` file imports — never
/// an ambient copy. A parent-only secret whose name is NOT allowlisted must
/// not reach the confined child's environment (`venv_env_map` is the seam that
/// builds it). Proves the "degraded/SafeSubset path" does not leak, closing
/// the deviation with executable coverage.
#[tokio::test]
async fn confined_shell_env_does_not_leak_a_parent_only_secret_1600() {
    let _lock = env_lock().await;
    // Narrow default passthrough (no operator widening).
    let _pt = EnvVar::unset("NEWT_SHELL_ENV_PASSTHROUGH");
    // A secret only in newt's OWN process env, with a non-allowlisted name.
    let _canary = EnvVar::set("NEWT_SAFESUBSET_ENV_CANARY_1600", "s3cr3t-parent-only-1600");
    let env = venv_env_map();
    assert!(
        !env.contains_key("NEWT_SAFESUBSET_ENV_CANARY_1600"),
        "the confined shell env leaked a parent-only secret NAME (#1600): {env:?}"
    );
    assert!(
        !env.values().any(|v| v.contains("s3cr3t-parent-only-1600")),
        "the confined shell env leaked a parent-only secret VALUE (#1600)"
    );
    // Sanity: the seam still builds a real env (the engine marker is set), so
    // the assertions above are not vacuously true against an empty map.
    assert_eq!(
        env.get("SHELL").map(String::as_str),
        Some(shell_engine().as_str())
    );
}

/// Workspace-fenced fs, NO exec, NO net — the shape under which the
/// confined shell denies (real build) or fails closed (stub build).
fn caveats_no_exec(ws: &std::path::Path) -> Caveats {
    Caveats {
        fs_read: Scope::only([ws.to_string_lossy().into_owned()]),
        fs_write: Scope::only([ws.to_string_lossy().into_owned()]),
        exec: Scope::none(),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

async fn run_tool(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
) -> String {
    run_tool_with_floor(name, args, ws, caveats, None).await
}

/// #307: like [`run_tool`] but with an explicit exec FLOOR (the active
/// named-permission-preset clamp). `Some(scope)` makes the `--disable-ocap`
/// bypass conditional on the floor permitting the command; `None` is the
/// pre-#307 behavior.
async fn run_tool_with_floor(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    exec_floor: Option<&Scope<String>>,
) -> String {
    execute_tool(
        name,
        &args,
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None, // memory_source
        None,
        exec_floor,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await
}

/// The switch reads fail-closed: ONLY the exact value `1` (the value the
/// CLI exports and the issue documents) asserts the bypass. This is also
/// the env-var-equivalence half of the #297 test list — the flag and the
/// env var are one mechanism (`--disable-ocap` just exports the var).
#[test]
fn ocap_disabled_requires_exactly_1() {
    let _l = ENV_LOCK.blocking_lock();
    {
        let _unset = EnvVar::unset("NEWT_DISABLE_OCAP");
        assert!(!ocap_disabled(), "absent ⇒ confinement stays on");
    }
    for (value, expected) in [
        ("1", true),
        ("0", false),
        ("", false),
        ("true", false),
        ("yes", false),
        ("YOLO", false),
    ] {
        let _set = EnvVar::set("NEWT_DISABLE_OCAP", value);
        assert_eq!(
            ocap_disabled(),
            expected,
            "NEWT_DISABLE_OCAP={value:?} must read as {expected}"
        );
    }
}

/// Same fail-closed contract for the `--full-access` preset override:
/// ONLY the exact value `1` asserts it (the flag and the env var are one
/// mechanism — `--full-access` just exports the var).
#[test]
fn full_access_requested_requires_exactly_1() {
    let _l = ENV_LOCK.blocking_lock();
    {
        let _unset = EnvVar::unset("NEWT_FULL_ACCESS");
        assert!(!full_access_requested(), "absent ⇒ configured preset rules");
    }
    for (value, expected) in [
        ("1", true),
        ("0", false),
        ("", false),
        ("true", false),
        ("yes", false),
        ("FULL", false),
    ] {
        let _set = EnvVar::set("NEWT_FULL_ACCESS", value);
        assert_eq!(
            full_access_requested(),
            expected,
            "NEWT_FULL_ACCESS={value:?} must read as {expected}"
        );
    }
}

/// #1176 shadow-OCAP recording gate. The decision table, which
/// `exec_confined_command` consults before dispatch:
/// - host-bypass (yolo) → record (unconfined host shell);
/// - full-access on the confined path → record (caveats are top()) —
///   THE PARITY FIX: before it, a bare `--full-access` run armed the
///   recorder yet the confined dispatch never wrote;
/// - a genuinely confined session (neither) → do NOT record (real leash).
#[test]
fn shadow_records_iff_the_run_is_unconfined() {
    assert!(shadow_records(true, false), "yolo host bypass records");
    assert!(
        shadow_records(false, true),
        "--full-access confined dispatch records (the #1176 parity fix)"
    );
    assert!(shadow_records(true, true), "both routes still record");
    assert!(
        !shadow_records(false, false),
        "a genuinely confined session has a real leash — nothing to shadow"
    );
}

/// FLAG OFF ⇒ the command goes to the confined dispatch, which governs it.
/// Built against the agent-bridle env-seam branch (#783), the bridle ships
/// the REAL safe-subset shell (not the old fail-closed stub), so an
/// ungranted `echo` under `exec = none` is DENIED by the L3 boundary. This
/// is the "when the real shell returns" case the prior stub-build note
/// anticipated; the "unavailable in this build" stub error is retired.
#[tokio::test]
async fn flag_off_run_command_keeps_the_confined_dispatch_verbatim() {
    let _l = env_lock().await;
    let _off = EnvVar::unset("NEWT_DISABLE_OCAP");
    // #1243 Leg 1: pin safe-subset. This test proves the disable-ocap FLOOR
    // (engine-independent, runs before the engine); it must not depend on
    // the L3-gated default (`echo` is a TRUE bash builtin under brush — it
    // never spawns, so it isn't exec-gated, which is correct but would make
    // this floor assertion box-dependent).
    let _eng = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    let out = run_tool(
        "run_command",
        serde_json::json!({"command": "echo hi"}),
        ws.path(),
        &caveats,
    )
    .await;
    assert!(
        out.contains("capability denied"),
        "flag off ⇒ the confined dispatch must govern (deny) the command, got: {out}"
    );
}

/// FLAG ON: a command the confined shell fails closed on now runs on the
/// host shell and returns its real output through the SAME envelope
/// formatter (`shell_envelope_output`).
#[cfg(unix)]
#[tokio::test]
async fn yolo_runs_the_denied_command_on_the_host_shell() {
    let _l = env_lock().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    let out = run_tool(
        "run_command",
        serde_json::json!({"command": "echo yolo-ok"}),
        ws.path(),
        &caveats,
    )
    .await;
    assert_eq!(out, "yolo-ok\n");

    // No output ⇒ the same shape the bridle path produces. Since #1969 a
    // nonzero exit renders as a failure rather than a bare `(exit N)`, which
    // classified as a success.
    let out = run_tool(
        "run_command",
        serde_json::json!({"command": "exit 3"}),
        ws.path(),
        &caveats,
    )
    .await;
    assert_eq!(out, "error: command exited 3");
}

/// #726/#945: a verbose `run_command` MUST NOT flood the model's context
/// window, but MUST still surface both ends of the output — a command's
/// summary/failure/exit status lives at the TAIL, and #726's original
/// head-only cap silently dropped exactly that (the gap #945 closed). Runs
/// through the real host shell (yolo path) so it exercises the actual
/// `shell_envelope_output` → cap composition. The global budget is the
/// default 10k in the test binary (nothing raises it above default), so
/// the assertions are upper-bounded and robust regardless of a smaller
/// racing value. This test goes through the legacy `execute_tool` path
/// (`run_tool`/`run_tool_with_floor` below), which has no spill store —
/// the no-spill-id elision marker branch, not the `spill:<id>` one.
#[cfg(unix)]
#[tokio::test]
async fn run_command_output_over_budget_is_token_capped() {
    let _l = env_lock().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    // ~350k chars of output — well over the default ~40k-char budget.
    let out = run_tool(
        "run_command",
        serde_json::json!({"command": "seq 1 60000"}),
        ws.path(),
        &caveats,
    )
    .await;
    assert!(
        out.len() < 41_500,
        "model-facing output capped near the ~40k-char budget, got {} bytes",
        out.len()
    );
    assert!(
        out.contains("chars elided (head+tail shown"),
        "carries the head+tail elision marker: {:?}",
        &out[..out.len().min(400)]
    );
    // #945: the HEAD survives — the earliest lines are still visible.
    assert!(
        out.starts_with("1\n2\n3\n"),
        "head preserved: {:?}",
        &out[..out.len().min(160)]
    );
    // #945 (the regression this test now guards): the TAIL survives too —
    // under the old head-only cap this was the first assertion to break
    // (it asserted the OPPOSITE: `!out.contains("60000")`).
    assert!(
        out.trim_end().ends_with("60000"),
        "tail preserved, not dropped by the cap: {:?}",
        &out[out.len().saturating_sub(160)..]
    );
}

// --- #307 floor property: preset clamp WINS over --disable-ocap -------

/// Unit-cover every branch of the bypass-floor predicate.
#[test]
fn exec_floor_permits_covers_each_branch() {
    use crate::caveats::Scope;
    // No floor ⇒ always permit (bit-for-bit pre-#307).
    assert!(exec_floor_permits(None, "rm -rf /"));
    // Empty command ⇒ let it through to the normal path.
    let only_echo = Scope::only(["echo".to_string()]);
    assert!(exec_floor_permits(Some(&only_echo), ""));
    // In-floor simple command ⇒ permitted.
    assert!(exec_floor_permits(Some(&only_echo), "echo hi"));
    // Out-of-floor program ⇒ refused.
    assert!(!exec_floor_permits(Some(&only_echo), "rm hi"));
    // Compound command ⇒ refused even with an allow-listed leading token.
    assert!(!exec_floor_permits(Some(&only_echo), "echo hi && rm x"));
    assert!(!exec_floor_permits(Some(&only_echo), "echo a | tee b"));
    assert!(!exec_floor_permits(Some(&only_echo), "echo $(rm x)"));
    // `Scope::All` floor permits any simple command.
    let all: Scope<String> = Scope::All;
    assert!(exec_floor_permits(Some(&all), "anything goes"));
    assert!(!exec_floor_permits(Some(&all), "anything; sneaky"));
}

/// ADVERSARIAL PROBE (review #312): exhaustively attack `exec_floor_permits`
/// with EVERY shell injection / compound form so the floor is proven against
/// more than just `&&`. An `echo`-only floor must refuse to bypass for any
/// form that could chain or substitute a second program.
#[test]
fn exec_floor_refuses_every_metacharacter_form() {
    use crate::caveats::Scope;
    let echo = Scope::only(["echo".to_string()]);
    // Each of these begins with the allow-listed `echo` but smuggles or
    // could smuggle a second program. None may bypass.
    let attacks = [
        "echo ok && rm -rf /tmp/x", // && and
        "echo ok || rm -rf /tmp/x", // || or
        "echo ok ; rm -rf /tmp/x",  // ; sequence
        "echo ok | sh",             // | pipe
        "echo ok|sh",               // | no spaces
        "echo $(rm x)",             // $() command substitution
        "echo ${IFS}rm",            // ${} parameter expansion
        "echo `rm x`",              // backtick substitution
        "echo ok & rm x",           // & background
        "echo ok > /etc/passwd",    // > redirect out
        "echo ok >> /etc/passwd",   // >> append
        "echo < /etc/shadow",       // < redirect in
        "echo ok 2> err",           // 2> fd redirect (contains >)
        "(rm x)",                   // ( subshell
        "echo ok\nrm -rf /tmp/x",   // newline-separated
        "echo ok\nrm x\n",          // trailing newline
    ];
    for a in attacks {
        assert!(
            !exec_floor_permits(Some(&echo), a),
            "metacharacter form must NOT bypass the floor: {a:?}"
        );
    }
    // Forms with NO shell metacharacter that should still be refused because
    // the LEADING TOKEN is not the allow-listed program:
    let leading_token_attacks = [
        "rm -rf /tmp/x", // plain out-of-floor program
        "FOO=bar rm x",  // env-prefix: leading token `FOO=bar` ∉ floor
        "/bin/echo ok",  // path form: `/bin/echo` ≠ `echo` (exact match)
        "  rm x",        // leading whitespace, still `rm`
        "env rm x",      // `env` wrapper, leading token `env` ∉ floor
        "bash -c rm",    // `bash` ∉ floor
    ];
    for a in leading_token_attacks {
        assert!(
            !exec_floor_permits(Some(&echo), a),
            "out-of-floor leading token must be refused: {a:?}"
        );
    }
    // Sanity: a bare in-floor command with only a benign arg DOES bypass —
    // the floor is a ceiling, not a blanket off-switch. (A dangerous arg to
    // a permitted program is the user's accepted risk: they allow-listed it.)
    assert!(exec_floor_permits(Some(&echo), "echo hello world"));
    assert!(exec_floor_permits(Some(&echo), "echo -n trailing"));
}

/// FLOOR TEST (a) — the security contract: with `--disable-ocap` asserted,
/// an exec FLOOR that denies the command must STOP the unconfined bypass.
/// `echo` is outside a readonly floor (`exec = none`), so even with yolo on
/// it does NOT run on the host shell — it falls through to the confined
/// dispatch, which (env-seam real shell) DENIES it. A deliberately
/// restricted triage mode is NOT un-clamped by `--yolo`.
#[tokio::test]
async fn floor_blocks_disable_ocap_for_a_denied_exec() {
    let _l = env_lock().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    // #1243 Leg 1: pin safe-subset — this asserts the exec FLOOR blocks the
    // bypass (engine-independent); `echo` is a brush builtin, so the default
    // engine would run it unspawned and make the floor test box-dependent.
    let _eng = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    // A readonly-triage preset clamp: exec denies everything.
    let floor = crate::NamedPermissionPreset {
        readonly: true,
        ..Default::default()
    }
    .clamp();
    let out = run_tool_with_floor(
        "run_command",
        serde_json::json!({"command": "echo should-not-run"}),
        ws.path(),
        &caveats,
        Some(&floor.exec),
    )
    .await;
    // The bypass did NOT fire: the command never reached the host shell, so
    // it fell to the confined dispatch and was denied, not `should-not-run\n`.
    assert_ne!(out, "should-not-run\n", "the floor must block the bypass");
    assert!(
        out.contains("capability denied"),
        "fell to confined dispatch and was denied, got: {out}"
    );
}

/// FLOOR TEST (a, positive) — a command INSIDE the floor still takes the
/// fast unconfined path under `--disable-ocap`. The floor is a ceiling, not
/// a blanket off-switch: an explicitly allow-listed command runs.
#[cfg(unix)]
#[tokio::test]
async fn floor_allows_disable_ocap_for_an_in_floor_exec() {
    let _l = env_lock().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    // A triage preset that allow-lists `echo`.
    let floor = crate::NamedPermissionPreset {
        readonly: true,
        exec_allow: vec!["echo".to_string()],
        ..Default::default()
    }
    .clamp();
    let out = run_tool_with_floor(
        "run_command",
        serde_json::json!({"command": "echo in-floor-ok"}),
        ws.path(),
        &caveats,
        Some(&floor.exec),
    )
    .await;
    assert_eq!(out, "in-floor-ok\n", "in-floor command runs unconfined");
}

/// FLOOR conservatism — a COMPOUND command never bypasses under an active
/// floor, even if its leading token is allow-listed: `echo ok && rm -rf /`
/// must not smuggle `rm` past an `echo` grant. It falls to the confined
/// shell (env-seam real shell ⇒ denied), which gates each spawn.
#[tokio::test]
async fn floor_refuses_bypass_for_a_compound_command() {
    let _l = env_lock().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    // #1243 Leg 1: pin safe-subset so this confined-denial assertion is
    // deterministic — shell_engine() reads NEWT_SHELL_ENGINE FIRST, so the
    // test is immune to a NEWT_FULL_ACCESS leak from a concurrent test
    // (which on Windows would select brush, whose `echo` builtin runs
    // un-gated instead of the whole compound being atomically denied).
    let _eng = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    // `echo` is allow-listed, but the `&&` chains an unlisted `rm`.
    let floor = crate::NamedPermissionPreset {
        readonly: true,
        exec_allow: vec!["echo".to_string()],
        ..Default::default()
    }
    .clamp();
    let out = run_tool_with_floor(
        "run_command",
        serde_json::json!({"command": "echo ok && rm -rf /tmp/x"}),
        ws.path(),
        &caveats,
        Some(&floor.exec),
    )
    .await;
    assert_ne!(out, "ok\n", "a compound command must not bypass the floor");
    // Compound ⇒ never bypasses; it falls to the confined shell, which
    // (env-seam real shell) denies the ungranted command under `exec = none`.
    assert!(
        out.contains("capability denied"),
        "fell to confined dispatch and was denied, got: {out}"
    );
}

/// FLOOR TEST (c) — `None` floor is bit-for-bit the pre-#307 bypass: a
/// denied-by-caveats command still runs unconfined under `--disable-ocap`,
/// proving the floor is opt-in and the no-preset case is unchanged.
#[cfg(unix)]
#[tokio::test]
async fn no_floor_keeps_disable_ocap_bit_for_bit() {
    let _l = env_lock().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    let out = run_tool_with_floor(
        "run_command",
        serde_json::json!({"command": "echo no-floor-ok"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert_eq!(out, "no-floor-ok\n", "no floor ⇒ bypass unchanged");
}

/// Envelope parity (#297): the host-shell envelope is structurally
/// identical to the bridle one — `exit_code` / `stdout` / `stderr` /
/// `sandbox_kind`, `denied`/`denials` omitted (⇒ not denied) — so the
/// existing envelope readers apply unchanged.
#[cfg(unix)]
#[tokio::test]
async fn host_shell_envelope_matches_the_bridle_shape() {
    let ws = tempfile::TempDir::new().unwrap();
    let envelope = host_shell_dispatch(
        "echo out; echo err >&2; exit 3",
        &ws.path().to_string_lossy(),
        None,
    )
    .await
    .expect("host shell runs");
    assert_eq!(envelope["exit_code"], 3);
    assert_eq!(envelope["stdout"], "out\n");
    assert_eq!(envelope["stderr"], "err\n");
    assert_eq!(envelope["sandbox_kind"], "none");
    // Omitted exactly as the bridle envelope omits them on the
    // nothing-was-denied path — `envelope_denied` reads it natively.
    assert!(envelope.get("denied").is_none(), "got: {envelope}");
    assert!(envelope.get("denials").is_none(), "got: {envelope}");
    assert!(!envelope_denied(&envelope));
    // And the shared formatter renders it like any confined result — which
    // since #1969 means a nonzero exit is marked as a failure ahead of its
    // output, so the ledger's `ok` bit stops reading a failing command as a
    // success. The streams themselves are untouched.
    assert_eq!(
        shell_envelope_output(&envelope, 20, false, false, None, None),
        "error: command exited 3\nout\nerr\n"
    );
}

#[test]
fn decode_shell_stream_preserves_valid_utf8() {
    let text = "// ── Model — test ──\n";
    assert_eq!(decode_shell_stream(text.as_bytes()), text);
}

#[test]
fn decode_shell_stream_repairs_bsd_cat_v_utf8_notation() {
    // This is what macOS/BSD `cat -v` emits for "─ —\n" in a UTF-8
    // locale: the leading e2 byte is raw, while continuation bytes are
    // rendered as M-^T/M-^@ etc. A lossy decode would display
    // "�M-^TM-^@ �M-^@M-^T".
    let cat_v = b"\xe2M-^TM-^@ \xe2M-^@M-^T\n";
    assert_eq!(decode_shell_stream(cat_v), "─ —\n");
}

#[test]
fn decode_shell_stream_repairs_two_byte_bsd_cat_v_notation() {
    // "é" is c3 a9; BSD `cat -v` leaves c3 raw and renders a9 as M-).
    let cat_v = b"caf\xc3M-)\n";
    assert_eq!(decode_shell_stream(cat_v), "café\n");
}

/// The venv/PATH prefix logic rides the HOST-BYPASS path unchanged: the
/// `export VIRTUAL_ENV=…; export PATH=…;` prefix is prepended to the
/// `--yolo` command, which runs on a real `/bin/sh` where `export` works.
/// (The confined path no longer gets the prefix — it uses the env seam;
/// see `confined_dispatch_uses_env_seam_not_export_prefix_783`.)
#[cfg(unix)]
#[tokio::test]
async fn yolo_keeps_the_venv_prefix_logic() {
    let _l = env_lock().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let _venv = EnvVar::set("NEWT_VENV", "/opt/fake-venv");
    let _virtual = EnvVar::unset("VIRTUAL_ENV");
    let _paths = EnvVar::unset("NEWT_EXEC_PATHS");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    let out = run_tool(
        "run_command",
        serde_json::json!({"command": "echo \"$VIRTUAL_ENV\""}),
        ws.path(),
        &caveats,
    )
    .await;
    assert_eq!(out, "/opt/fake-venv\n");
}

/// In --yolo mode an unrestricted fs mutation prompt must not read EOF as
/// a human decline. The flag is already an explicit interactive override,
/// so final write/delete confirms auto-accept instead of auto-skipping.
#[tokio::test]
async fn yolo_auto_confirms_unrestricted_write_and_delete_prompts() {
    let _l = env_lock().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();

    let out = run_tool(
        "write_file",
        serde_json::json!({"path": "auto.txt", "content": "ok\n"}),
        ws.path(),
        &caveats,
    )
    .await;
    assert!(out.starts_with("wrote auto.txt"), "got: {out}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("auto.txt")).unwrap(),
        "ok\n"
    );

    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "auto.txt"}),
        ws.path(),
        &caveats,
    )
    .await;
    assert!(out.starts_with("deleted auto.txt"), "got: {out}");
    assert!(
        !ws.path().join("auto.txt").exists(),
        "yolo-confirmed delete must remove the file"
    );
}

/// **The `--yolo` confirm's rendered bytes, pinned** (C0a, #1856).
///
/// A0's sweep found this form had no byte coverage at all: only the
/// alias-absence property and the fail-closed outcomes were tested, so
/// the string the operator reads was free to drift. C0a moves this
/// rendering from `Question::terminal_text` to `markup::plain::render`,
/// and changing an untested rendering path inside a byte-identity slice
/// is exactly the shape that slice must not have — so the gap closes
/// here rather than being inherited by D0.
#[test]
fn mutation_confirm_renders_its_frozen_form() {
    // D0 (#1878): built directly, no legacy `Question` in the middle. The
    // FROZEN BYTES below are unchanged, which is the whole point — this
    // test is what proves the migration moved the construction without
    // moving what the operator reads.
    let definition = mutation_confirm_definition("delete `auto.txt`?");
    assert_eq!(
        crate::markup::plain::render(&definition),
        "delete `auto.txt`?\n[y] to confirm\n[n] to skip"
    );
    // The hidden `Y`/`N` aliases parse but are never advertised
    // (BHV-PROMPT-005).
    let rendered = crate::markup::plain::render(&definition);
    assert!(!rendered.contains('Y'), "alias rendered: {rendered}");
    assert!(!rendered.contains('N'), "alias rendered: {rendered}");
}

/// Direct coverage of the fs-mutation confirm guard's parsing + fail-closed
/// contract, without the full `execute_tool` path. Proves defect 3 (uppercase
/// `Y` confirms again) and defect 2 (every non-answer outcome denies).
#[tokio::test]
async fn unrestricted_mutation_confirm_accepts_y_case_insensitively_and_fails_closed() {
    let _l = env_lock().await;
    let _off = EnvVar::unset("NEWT_DISABLE_OCAP");
    // fs_write = Scope::All → the guard actually prompts (does not bail true).
    let caveats = Caveats::top();

    struct ScriptGate(HumanQuestionOutcome);
    impl super::PermissionGate for ScriptGate {
        fn ask(&mut self, _r: &[super::PermissionRequest]) -> super::PermissionDecision {
            super::PermissionDecision::Deny
        }
        fn ask_question(&mut self, _q: &str) -> HumanQuestionOutcome {
            self.0.clone()
        }
    }

    fn confirm(caveats: &Caveats, outcome: Option<HumanQuestionOutcome>) -> bool {
        match outcome {
            Some(o) => {
                let mut gate = ScriptGate(o);
                let mut g: Option<&mut dyn super::PermissionGate> = Some(&mut gate);
                confirm_unrestricted_fs_mutation(caveats, &mut g, "overwrite ~/x?")
            }
            None => {
                let mut g: Option<&mut dyn super::PermissionGate> = None;
                confirm_unrestricted_fs_mutation(caveats, &mut g, "overwrite ~/x?")
            }
        }
    }

    // defect 3: lowercase and uppercase Y both confirm.
    assert!(confirm(
        &caveats,
        Some(HumanQuestionOutcome::Answer("y".into()))
    ));
    assert!(confirm(
        &caveats,
        Some(HumanQuestionOutcome::Answer("Y".into()))
    ));
    // any other answer denies (n, N, junk, empty).
    for a in ["n", "N", "maybe", ""] {
        assert!(
            !confirm(&caveats, Some(HumanQuestionOutcome::Answer(a.into()))),
            "answer {a:?} must not confirm the mutation"
        );
    }
    // defect 2: every non-answer outcome fails closed (mutation denied).
    for o in [
        HumanQuestionOutcome::Unavailable,
        HumanQuestionOutcome::Cancelled,
        HumanQuestionOutcome::ExitRequested,
        HumanQuestionOutcome::InputClosed,
        HumanQuestionOutcome::InputFailed,
    ] {
        assert!(
            !confirm(&caveats, Some(o.clone())),
            "outcome {o:?} must fail closed"
        );
    }
    // no gate at all → denied.
    assert!(!confirm(&caveats, None));
}

/// Non-yolo unrestricted fs mutations still ask, but through the
/// PermissionGate question seam. In the TUI that seam owns
/// PromptStdinGuard, so cbreak/VMIN=0 stdin cannot auto-answer "not y".
#[tokio::test]
async fn unrestricted_write_and_delete_confirm_through_permission_gate() {
    struct ConfirmGate {
        answer: Option<String>,
        questions: Vec<String>,
    }
    impl super::PermissionGate for ConfirmGate {
        fn ask(&mut self, _requests: &[super::PermissionRequest]) -> super::PermissionDecision {
            super::PermissionDecision::Deny
        }
        fn ask_question(&mut self, question: &str) -> HumanQuestionOutcome {
            self.questions.push(question.to_string());
            self.answer.clone().map_or(
                HumanQuestionOutcome::Unavailable,
                HumanQuestionOutcome::Answer,
            )
        }
    }

    let _l = env_lock().await;
    let _off = EnvVar::unset("NEWT_DISABLE_OCAP");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    let mut gate = ConfirmGate {
        answer: Some("y".to_string()),
        questions: Vec::new(),
    };

    let out = execute_tool(
        "write_file",
        &serde_json::json!({"path": "guarded.txt", "content": "ok\n"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None, // memory_source
        Some(&mut gate),
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert!(out.starts_with("wrote guarded.txt"), "got: {out}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("guarded.txt")).unwrap(),
        "ok\n"
    );

    let out = execute_tool(
        "delete_file",
        &serde_json::json!({"path": "guarded.txt"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None, // memory_source
        Some(&mut gate),
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert!(out.starts_with("deleted guarded.txt"), "got: {out}");
    assert!(!ws.path().join("guarded.txt").exists());
    assert_eq!(
        gate.questions,
        vec![
            "Write this file? [y/N]\n[y] to confirm\n[n] to skip".to_string(),
            "Delete this file? [y/N]\n[y] to confirm\n[n] to skip".to_string(),
        ]
    );
}

/// #783 regression (Bug A): the confined-shell dispatch carries the RAW
/// user command and the venv via agent-bridle's structured `env` seam — NOT
/// an `export …;` prefix on `cmd`. The old code built
/// `{ "cmd": cmd_with_venv, "cwd": … }` (the prefixed form), and the
/// confined safe-subset engine refuses an `export` builtin on a compound
/// command, which is the bug. Pure: builds the dispatch args only, no spawn.
#[tokio::test]
async fn confined_dispatch_uses_env_seam_not_export_prefix_783() {
    let _l = env_lock().await;
    let _venv = EnvVar::set("NEWT_VENV", "/opt/fake-venv");
    let _virtual = EnvVar::unset("VIRTUAL_ENV");
    let _paths = EnvVar::unset("NEWT_EXEC_PATHS");

    // The literal failing case from #783.
    let cmd = "hostname; sw_vers 2>/dev/null | head -1; uname -s";
    let args = confined_dispatch_args(cmd, "/work/dir");

    // The command is passed RAW — no `export …;` prefix smuggled in.
    assert_eq!(args["cmd"], cmd);
    assert!(
        !args["cmd"]
            .as_str()
            .expect("cmd is a string")
            .contains("export "),
        "confined cmd must not carry an export prefix: {args}"
    );
    assert_eq!(args["cwd"], "/work/dir");

    // The venv rides the env seam: VIRTUAL_ENV + venv bin prepended to PATH.
    assert_eq!(args["env"]["VIRTUAL_ENV"], "/opt/fake-venv");
    let path = args["env"]["PATH"].as_str().expect("PATH in the env seam");
    assert!(
        path.starts_with("/opt/fake-venv/bin"),
        "venv bin must be prepended to PATH: {path}"
    );
}

/// #783: with neither venv input set, the env seam is empty (no spurious
/// VIRTUAL_ENV / PATH keys) — the no-venv invocation is unaffected.
#[tokio::test]
async fn confined_dispatch_env_seam_without_venv_783() {
    let _l = env_lock().await;
    let _venv = EnvVar::unset("NEWT_VENV");
    let _virtual = EnvVar::unset("VIRTUAL_ENV");
    let _paths = EnvVar::unset("NEWT_EXEC_PATHS");
    let _pass = EnvVar::unset("NEWT_SHELL_ENV_PASSTHROUGH"); // ⇒ default HOME+USER
    let _home = EnvVar::set("HOME", "/home/testuser");

    let args = confined_dispatch_args("ls -la", "/work/dir");
    assert_eq!(args["cmd"], "ls -la");
    let env = &args["env"];
    // #783: without a venv, no VIRTUAL_ENV / PATH override is injected...
    assert!(
        env.get("VIRTUAL_ENV").is_none(),
        "no venv ⇒ no VIRTUAL_ENV: {args}"
    );
    assert!(
        env.get("PATH").is_none(),
        "no venv/exec-paths ⇒ no PATH override: {args}"
    );
    // ...but HOME now passes through so brush can expand `~` (the confined
    // shell had NO env before, so `~` stayed literal and left `~/…` debris),
    // and SHELL identifies the confined engine.
    assert_eq!(
        env["HOME"], "/home/testuser",
        "HOME must pass through: {args}"
    );
    assert!(
        env.get("SHELL").is_some(),
        "SHELL must identify the confined engine: {args}"
    );
}

/// fs fence under yolo (#297): the newt-native workspace fence is NOT
/// bypassed — a write/read outside the granted scope keeps the standard
/// denial bit-for-bit. Yolo is unconfined exec, never authority-off.
#[tokio::test]
async fn yolo_keeps_the_fs_workspace_fence() {
    let _l = env_lock().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    let escape = "/definitely-outside-the-fence/escape.txt";
    let out = run_tool(
        "write_file",
        serde_json::json!({"path": escape, "content": "nope"}),
        ws.path(),
        &caveats,
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_write", escape));
    assert!(!std::path::Path::new(escape).exists());

    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": escape}),
        ws.path(),
        &caveats,
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_write", escape));

    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "/etc/hostname"}),
        ws.path(),
        &caveats,
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_read", "/etc/hostname"));
}

/// Precedence (#297): with both `--disable-ocap` and a #263 gate present,
/// exec never prompts — nothing is denied, so the gate is structurally
/// unreachable for run_command. (fs prompting stays live; the fs-fence
/// test above and the #263 suite cover that axis.)
#[cfg(unix)]
#[tokio::test]
async fn yolo_never_consults_the_permission_gate_for_exec() {
    struct PanicGate;
    impl super::PermissionGate for PanicGate {
        fn ask(&mut self, requests: &[super::PermissionRequest]) -> super::PermissionDecision {
            panic!("yolo exec must never prompt, but the gate was asked: {requests:?}");
        }
        fn ask_question(&mut self, question: &str) -> HumanQuestionOutcome {
            panic!("yolo exec must never prompt, but the gate was asked: {question:?}");
        }
    }
    let _l = env_lock().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    let mut gate = PanicGate;
    let out = execute_tool(
        "run_command",
        &serde_json::json!({"command": "echo no-prompt"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None, // memory_source
        Some(&mut gate),
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_eq!(out, "no-prompt\n");
}

/// The corrective tool-name guard still answers BEFORE the bypass: yolo
/// changes where commands run, not what counts as a command.
#[tokio::test]
async fn yolo_keeps_the_tool_name_corrective_guard() {
    let _l = env_lock().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    let out = run_tool(
        "run_command",
        serde_json::json!({"command": "read_file foo.txt"}),
        ws.path(),
        &caveats,
    )
    .await;
    assert!(out.contains("is a tool, not a shell command"), "got: {out}");
}

// --- facade P4 (#780): hidden tool-call routing dispatch ---------------

/// A git stub that proves *which path served the call*: a routed
/// `git status` lands here as op `status`; a routed write op would surface
/// the unexpected-op error (so a test can assert it was NOT routed).
struct RoutingStubGit;
impl crate::agentic::GitTool for RoutingStubGit {
    fn dispatch(
        &self,
        op: &str,
        _args: &serde_json::Value,
        _caps: &crate::git_caveats::GitCaveats,
    ) -> Result<String, String> {
        match op {
            "status" => Ok("on branch main (routed via git built-in)".to_string()),
            other => Err(format!("unexpected routed git op '{other}'")),
        }
    }
}

async fn run_routed_with_git(command: &str, ws: &std::path::Path, caveats: &Caveats) -> String {
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
        None, // memory_source
        None, // permission_gate
        None, // exec_floor
        Some(&RoutingStubGit as &dyn crate::agentic::GitTool),
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await
}

/// The routing switch reads fail-closed (only the exact `1`), and it is a
/// DISTINCT mechanism from `ocap_disabled` (§7-F5): asserting `NEWT_NO_ROUTE`
/// never moves `ocap_disabled`, and asserting `NEWT_DISABLE_OCAP` never moves
/// `routing_disabled`. The two switches can never alias.
#[test]
fn routing_disabled_requires_exactly_1_and_is_independent_of_ocap() {
    let _l = ENV_LOCK.blocking_lock();
    let _no_ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
    {
        let _unset = EnvVar::unset("NEWT_NO_ROUTE");
        assert!(!routing_disabled(), "absent ⇒ routing stays on");
    }
    for (value, expected) in [("1", true), ("0", false), ("", false), ("true", false)] {
        let _set = EnvVar::set("NEWT_NO_ROUTE", value);
        assert_eq!(routing_disabled(), expected, "NEWT_NO_ROUTE={value:?}");
        // F5: turning routing off NEVER turns on the L3-off unconfine.
        assert!(
            !ocap_disabled(),
            "NEWT_NO_ROUTE must not imply --disable-ocap"
        );
    }
    // And the inverse: --disable-ocap must not imply --no-route.
    let _unset_route = EnvVar::unset("NEWT_NO_ROUTE");
    let _on_ocap = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    assert!(ocap_disabled());
    assert!(
        !routing_disabled(),
        "--disable-ocap must not imply --no-route"
    );
}

/// TDD: a routed read goes through the SAME fs floor — routing is NOT a
/// bypass. An out-of-scope `cat /etc/shadow` routes to `read_file` and is
/// denied by `fs_read` exactly as a direct `read_file` would be (the denial
/// short-circuits before any real fs access). The `fs_read` denial wording
/// also proves it reached the `read_file` arm (vs. the exec/shell path).
#[tokio::test]
async fn routed_cat_goes_through_the_fs_floor_not_a_bypass() {
    let _l = env_lock().await;
    let _route_on = EnvVar::unset("NEWT_NO_ROUTE");
    let _ocap_off = EnvVar::unset("NEWT_DISABLE_OCAP");
    // #1243 Leg 1: pin safe-subset (deterministic confined engine) so a
    // concurrent NEWT_FULL_ACCESS leak can't flip this to brush on Windows.
    let _eng = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path()); // fs_read scoped to ws only
    let out = run_tool(
        "run_command",
        serde_json::json!({ "command": "cat /etc/shadow" }),
        ws.path(),
        &caveats,
    )
    .await;
    assert!(
        out.contains("capability denied: fs_read does not permit") && out.contains("/etc/shadow"),
        "routed cat must hit the fs floor, not run unconfined; got: {out}"
    );
}

/// TDD: read-only `git status` is silently routed to the governed `git`
/// built-in (the stub proves the built-in served it). Revert the routing
/// promotion and this is red — the command would instead hit the run_command
/// corrective guard.
#[tokio::test]
async fn routed_git_status_dispatches_through_the_git_builtin() {
    let _l = env_lock().await;
    let _route_on = EnvVar::unset("NEWT_NO_ROUTE");
    let _ocap_off = EnvVar::unset("NEWT_DISABLE_OCAP");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    let out = run_routed_with_git("git status", ws.path(), &caveats).await;
    assert!(
        out.contains("routed via git built-in"),
        "git status must route to the governed git built-in; got: {out}"
    );
}

/// #1022: `run_command("rm file")` routes to the governed delete_file arm,
/// so deletion works under fs_write without requiring raw shell `rm`.
#[tokio::test]
async fn routed_rm_dispatches_through_delete_file() {
    let _l = env_lock().await;
    let _route_on = EnvVar::unset("NEWT_NO_ROUTE");
    let _ocap_off = EnvVar::unset("NEWT_DISABLE_OCAP");
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("stale.txt"), "remove me\n").unwrap();
    let caveats = caveats_no_exec(ws.path());
    let out = run_tool(
        "run_command",
        serde_json::json!({ "command": "rm stale.txt" }),
        ws.path(),
        &caveats,
    )
    .await;
    assert!(out.starts_with("deleted stale.txt"), "got: {out}");
    assert!(
        !ws.path().join("stale.txt").exists(),
        "routed rm must remove the file through delete_file"
    );
}

/// TDD: state-modifying `git add` is GATED as exec — NOT silently routed
/// (owner decision 2). It never reaches the git built-in (no unexpected-op
/// error from the stub); it falls through to the normal run_command path.
#[tokio::test]
async fn state_modifying_git_add_is_not_routed() {
    let _l = env_lock().await;
    let _route_on = EnvVar::unset("NEWT_NO_ROUTE");
    let _ocap_off = EnvVar::unset("NEWT_DISABLE_OCAP");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    let out = run_routed_with_git("git add a.txt", ws.path(), &caveats).await;
    assert!(
        !out.contains("routed"),
        "git add must NOT route to the git built-in; got: {out}"
    );
    // It falls through to the run_command path (git ∈ DIRECT_TOOL_NAMES ⇒
    // the existing corrective guard), never silently routed.
    assert!(out.contains("is a tool, not a shell command"), "got: {out}");
}

/// F5 (§7-F5): `--no-route` bypasses routing but NEVER disables L3. With
/// `NEWT_NO_ROUTE=1`, the same out-of-bounds `cat` is no longer routed to
/// `read_file` (no `fs_read` denial), yet it does NOT run unconfined — it
/// falls to the confined shell (env-seam real shell ⇒ denied), and
/// `ocap_disabled()` stays false. The boundary holds.
#[tokio::test]
async fn no_route_bypasses_routing_but_keeps_l3() {
    let _l = env_lock().await;
    let _route_off = EnvVar::set("NEWT_NO_ROUTE", "1");
    let _ocap_off = EnvVar::unset("NEWT_DISABLE_OCAP");
    // #1243 Leg 1: pin safe-subset (deterministic confined engine) so a
    // concurrent NEWT_FULL_ACCESS leak can't flip this to brush on Windows.
    let _eng = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    assert!(routing_disabled() && !ocap_disabled(), "L2 off, L3 on");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());
    let out = run_tool(
        "run_command",
        serde_json::json!({ "command": "cat /etc/shadow" }),
        ws.path(),
        &caveats,
    )
    .await;
    // Routing was OFF ⇒ NOT rewritten to read_file (no fs_read denial)…
    assert!(
        !out.contains("fs_read does not permit"),
        "--no-route must not route to read_file; got: {out}"
    );
    // …and the command did NOT run unconfined: it took the confined shell
    // (env-seam real shell ⇒ denied — the L3 boundary held).
    assert!(
        out.contains("capability denied"),
        "the L3 confined dispatch must still gate the command; got: {out}"
    );
}
