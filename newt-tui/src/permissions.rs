//! **Interactive permission prompting** — the `--prompt-for-permissions` (#263)
//! OCAP grant UI and its supporting state machine.
//!
//! When a session is allowed to ask, this is what turns a would-be denial into
//! an operator question: it renders the request ([`permission_prompt_text`]),
//! reads a single keystroke choice ([`parse_permission_choice`] →
//! [`PromptChoice`]), and threads the answer back through
//! [`PromptPermissionGate`] (the [`newt_core::PermissionGate`] impl the run loop
//! installs) and [`PermissionPromptState`] (session/one-shot/durable grants,
//! including the #904 permanent-deny ledger).
//!
//! **Terminal ownership is not decided here.** Stdin exclusion and raw-mode
//! line handling used to live in this file (`PromptStdinGuard`,
//! `enter_prompt_line_mode`); they moved into `newt_core::tty`, which now
//! arbitrates BOTH halves of the terminal. Every function below that can block
//! on a human takes a [`newt_core::tty::PromptWindow`] — an unforgeable
//! capability whose only constructor,
//! [`newt_core::tty::Terminal::suspend_for_prompt`], erases every live
//! ephemeral writer before it returns. A question printed onto a live spinner
//! is therefore not a bug that can be written: it does not typecheck.
//!
//! Extracted from `lib.rs` in the #1096 functional-cohesion pass. The
//! *default-deny invariant* is the load-bearing rule: a session that cannot
//! answer a TTY prompt never silently allows — see [`should_prompt_permissions`].
//!
//! The `/posture` named-preset clamp (issue #307) is a *sibling* concern and
//! deliberately stays in `lib.rs` next to the session state it floors.

use std::io;
use std::time::{Duration, Instant};

use crate::danger;
use crate::mint_operating_key;
use newt_core::agentic::{newt_line, print_newt};
use newt_core::tty::{PromptWindow, Terminal};

// ---------------------------------------------------------------------------

/// Is prompted-permission mode explicitly configured for this session? Pure in
/// its inputs so it's unit-testable. This is the EXPLICIT-ON signal only
/// (`--prompt-for-permissions` / `[tui.permissions] prompt`); whether the
/// session actually prompts is decided by [`should_prompt_permissions`], which
/// adds the #721 interactive default and the headless / explicit-off guards.
pub(crate) fn permission_prompting_configured(
    env_flag: bool,
    tui: Option<&newt_core::TuiConfig>,
) -> bool {
    env_flag || tui.is_some_and(|t| t.permissions.prompt)
}

/// #721: the named default for whether an INTERACTIVE session prompts on a
/// capability denial. Flipped ON — a human at a real terminal now gets the
/// allow/deny prompt by default, because a denial that ASKS the operator beats a
/// dead-end denial the model can't recover from. Convention-as-data: flip this
/// one knob to change the default without touching the predicate.
const INTERACTIVE_PROMPT_DEFAULT: bool = true;

/// #721: should this session prompt the human on a capability denial?
///
/// Pure predicate (unit-tested; the caller supplies the resolved TTY / tier
/// signals — never a real TTY in a test). The default flipped for INTERACTIVE
/// sessions (see [`INTERACTIVE_PROMPT_DEFAULT`]); HEADLESS / piped / eval / ACP
/// stay DEFAULT-DENY — they must NEVER block on a prompt no one can answer.
///
/// - `configured_on` — prompting explicitly enabled (`--prompt-for-permissions`
///   / `NEWT_PROMPT_FOR_PERMISSIONS` / `[tui.permissions] prompt = true`).
///   Honored, but with the new default no longer REQUIRED for interactive.
/// - `explicit_off`  — explicitly disabled (`--no-prompt-for-permissions` /
///   `NEWT_NO_PROMPT_FOR_PERMISSIONS`). Wins over both the default AND
///   `configured_on` — fail-closed honors the human's stated choice.
/// - `interactive`   — stdin AND stdout are real terminals.
/// - `headless`      — a non-interactive tier (worker / eval / ACP) that must
///   never block on a TTY prompt regardless of any ON signal.
///
/// Precedence: headless / non-TTY → never (the default-deny invariant the issue
/// requires); else explicit OFF → never; else ON (explicitly configured, or the
/// interactive default).
pub(crate) fn should_prompt_permissions(
    configured_on: bool,
    explicit_off: bool,
    interactive: bool,
    headless: bool,
) -> bool {
    // Default-deny invariant: a session that cannot answer a TTY prompt never
    // prompts, no matter what was configured. Load-bearing for headless safety.
    if headless || !interactive {
        return false;
    }
    // An explicit OFF beats the interactive default and an explicit ON.
    if explicit_off {
        return false;
    }
    // Interactive and not disabled: prompt — either explicitly on, or the #721
    // default. Both land here as ON.
    configured_on || INTERACTIVE_PROMPT_DEFAULT
}

/// One human choice at the permission prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptChoice {
    AllowOnce,
    AllowSession,
    Deny,
    DenyAlways,
    /// #904: deny PERMANENTLY — persisted to `~/.newt/permission-denials.jsonl`
    /// so the gate refuses this `(kind, target)` without re-prompting, across
    /// restarts. `[D]eny always` is the session-scoped sibling.
    DenyPermanent,
    /// #904: allow PERMANENTLY — for a NET host, durably grant it by appending
    /// it to `[tui.permissions] net` in the config (comment-preserving), so the
    /// next session reads it as an ambient allow and never prompts. Only offered
    /// for net denials; a durable widen of any axis is an explicit human keypress
    /// and is still re-minted `⊑` the user root (attenuation-only).
    AllowPermanent,
}

/// Map a typed answer to a choice. Case-significant on purpose — `[d]eny`
/// is the default, `[D]eny always` the session escalation, `[P]ermanently
/// deny` the durable deny (#904), and `[A]llow permanently` the durable net
/// grant (#904). Anything unrecognized (including empty / EOF) is the safe
/// default: deny.
pub(crate) fn parse_permission_choice(input: &str) -> PromptChoice {
    match input.trim() {
        "a" => PromptChoice::AllowOnce,
        "s" => PromptChoice::AllowSession,
        "A" => PromptChoice::AllowPermanent,
        "D" => PromptChoice::DenyAlways,
        "P" => PromptChoice::DenyPermanent,
        _ => PromptChoice::Deny,
    }
}

// ---------------------------------------------------------------------------
// Stdin ownership moved to `newt_core::tty`.
//
// It used to live here as a private `OnceLock<(Mutex<StdinOwnership>,
// Condvar)>` plus `PromptStdinGuard` / `WatcherStdinGuard` /
// `enter_prompt_line_mode`. That machinery was correct — and it was HALF the
// terminal. It modelled who may READ stdin with real rigor while nothing at all
// modelled who may WRITE the bottom line, which is exactly how a question came
// to be printed onto a live spinner and then scribbled out by it.
//
// One arbiter now owns both directions, so "I am about to block on a human"
// and "I own the bottom line" are mutually exclusive by construction. These
// re-exports keep the turn watcher's call sites unchanged.
//
// unix-gated to match its only consumer — the turn watcher's stdin interlock
// at `lib.rs` is `#[cfg(unix)]`, so on Windows this re-export would be an
// unused import and `-D warnings` fails the build.
#[cfg(unix)]
pub(crate) use newt_core::tty::try_watch_stdin;

/// #721 / facade P1b: is this request's `reason` MODEL-authored? Only the
/// proactive `request_permissions` tool lets the model write the `reason`
/// (`tools.rs` `execute_request_permissions`); a denial-driven request carries
/// harness denial text instead. Named so the untrusted-text policy (§7-F4) is
/// decided in one place.
pub(crate) fn reason_is_model_authored(req: &newt_core::PermissionRequest) -> bool {
    req.tool == "request_permissions"
}

/// Build the prompt shown for one denied capability (the #263 sketch shape),
/// hardened by facade **P1b** (§7-F3/F4):
///
/// 1. A high-danger grant (interpreter exec / broad fs root) carries a
///    **system-computed blast-radius line** ([`danger::DangerTable::blast_radius`])
///    that the model cannot author — a fact the lay operator can't be expected to
///    derive unaided (§1.1).
/// 2. A **model-authored** `reason` (from `request_permissions`) is labelled as
///    untrusted model text, never rendered as a harness fact (§7-F4). Harness
///    denial text (denial-driven requests) is shown plainly as context.
/// 3. A high-danger grant's menu **omits `[s]ession allow`** — it is not
///    session-allowable (the refusal is enforced in [`PermissionGate::ask`]);
///    `[k]ey allow` (step-up, P3, unbuilt) is noted as the future standing path.
pub(crate) fn permission_prompt_text(
    req: &newt_core::PermissionRequest,
    danger: &danger::DangerTable,
) -> String {
    use newt_core::DenialKind;
    let (verb, axis) = match req.kind {
        DenialKind::Exec => ("run", "outside the granted exec allowlist"),
        DenialKind::FsRead => ("read", "outside the granted fs_read scope"),
        DenialKind::FsWrite => ("write", "outside the granted fs_write scope"),
        DenialKind::Net => ("reach", "outside the granted net allowlist"),
        // FR-2 (#1001): a remote MCP tool the active persona does not grant.
        DenialKind::RemoteTool => ("call", "not in the active persona's tool allow-list"),
        // #1056: a local git write the session's projected git authority denies.
        DenialKind::GitWrite => (
            "commit/stage via git",
            "outside the granted git-write authority",
        ),
    };
    let tier = danger.classify(req.kind, &req.target);

    // (1) §7-F4: a system-computed blast-radius line for high-danger grants — a
    // fact derived from (capability, target), NOT from any model-supplied text.
    let blast = match danger.blast_radius(req.kind, &req.target) {
        Some(line) => format!("{line}\n"),
        None => String::new(),
    };

    // (2) §7-F4: the model's `reason` is UNTRUSTED. When it is model-authored
    // (`request_permissions`), label it as such so the operator never reads it
    // as a harness fact; a denial-driven request's reason is harness text and is
    // shown plainly as context.
    let reason = if req.reason.is_empty() {
        String::new()
    } else if reason_is_model_authored(req) {
        format!(
            "  model says (model-authored, unverified): \"{}\"\n",
            req.reason
        )
    } else {
        format!("  ({})\n", req.reason)
    };

    // (3) §7-F3/F4: a high-danger target is NOT session-allowable — omit `[s]`
    // and point at the future step-up path. Low-danger keeps the full menu.
    // #904: a low-danger NET host also gets `[A]llow permanently` — a durable
    // grant written to `[tui.permissions] net`. Not offered for other axes (no
    // per-target config allowlist) nor for high-danger (never durably widened).
    let menu = match tier {
        danger::DangerTier::High => {
            "[a]llow once   [d]eny (default)   [D]eny always   [P]ermanently deny   \
             (high-danger: [s]ession allow refused — [k]ey allow / step-up is the future path, P3) > "
                .to_string()
        }
        danger::DangerTier::Low if req.kind == DenialKind::Net => {
            "[a]llow once   [s]ession allow   [A]llow permanently (adds host to config)   \
             [d]eny (default)   [D]eny always   [P]ermanently deny > "
                .to_string()
        }
        danger::DangerTier::Low => {
            "[a]llow once   [s]ession allow   [d]eny (default)   [D]eny always   [P]ermanently deny > "
                .to_string()
        }
    };

    format!(
        "⊘ {} wants to {verb} `{}` — {axis}.\n{blast}{reason}  {menu}",
        req.tool, req.target
    )
}

/// Facade P1b: the production [`danger::DangerTable`] — the built-in
/// interpreter set plus this environment's broad fs roots (`$HOME` and the
/// current workspace dir), which a plain `[s]ession allow` must never grant
/// wholesale (§7-F3/F4). The env read happens once, here, at gate construction;
/// unit tests build a table by hand (`DangerTable::builtin().with_fs_root(...)`)
/// and never touch the real env (the no-real-fs-in-unit-tests rule).
pub(crate) fn production_danger_table() -> danger::DangerTable {
    let mut table = danger::DangerTable::builtin();
    if let Some(home) = std::env::var_os("HOME") {
        table = table.with_fs_root(std::path::PathBuf::from(home));
    }
    if let Ok(cwd) = std::env::current_dir() {
        table = table.with_fs_root(cwd);
    }
    table
}

/// The #1207 blessing judgment, exported as the ONE narrow seam `newt doctor
/// --sign-ocap` (newt-cli) needs from this subsystem: is an ocap-store target
/// high-danger by the production danger table? Passed to
/// [`newt_core::ocap_store::sign_approves`] as its `validate_approve`
/// predicate, so the blessing ceremony can never launder an interpreter or a
/// broad fs root into `approve.toml`. Fs classifies as a WRITE — the
/// conservative reading of a durable fs grant, and the table's High tier is
/// about broad roots on either axis.
pub fn ocap_high_danger_predicate() -> impl Fn(newt_core::ocap_store::CapabilityClass, &str) -> bool
{
    let table = production_danger_table();
    move |class, target| {
        let kind = match class {
            newt_core::ocap_store::CapabilityClass::Exec => newt_core::DenialKind::Exec,
            newt_core::ocap_store::CapabilityClass::Fs => newt_core::DenialKind::FsWrite,
            newt_core::ocap_store::CapabilityClass::Net => newt_core::DenialKind::Net,
        };
        table.classify(kind, target) == danger::DangerTier::High
    }
}

/// Production prompt: ask the question, read one line back. Any read error is a
/// deny (never a hang, never an allow).
///
/// The `&PromptWindow` is the whole fix. Obtaining one requires having called
/// [`Terminal::suspend_for_prompt`], which erases every live ephemeral writer
/// and holds the shared ticker still for the window's lifetime — so the
/// question lands on a clean row and stays there while the operator reads it.
/// The parameter is not advisory: without it this function does not compile,
/// which is why the same mistake cannot be reintroduced at a seventh call site.
pub(crate) fn prompt_permission_choice(w: &PromptWindow, prompt_text: &str) -> PromptChoice {
    w.ask(prompt_text).ok();
    match w.read_line() {
        Ok(answer) => parse_permission_choice(&answer),
        Err(_) => PromptChoice::Deny,
    }
}

/// #728/#783 (Bug C): pure interpreter for one line of free-text human input
/// (the `request_user_input` tool's answer). EOF (`Ok(0)`, e.g. the operator
/// pressing Ctrl-D) is treated exactly like the permission path
/// ([`prompt_permission_choice`] maps the same EOF to a valid response): it is
/// an empty, *deliberate* answer → `Some("")`, NOT "no human available". Only a
/// genuine read error → `None`, so the tool reports "no human available" only
/// when stdin truly cannot be read. Pure — unit-tested like
/// [`parse_permission_choice`].
pub(crate) fn interpret_user_line(read: io::Result<usize>, buf: &str) -> Option<String> {
    match read {
        // NOTE: when a TTY *is* present, a spontaneous EOF here implies stdin
        // contention with the chat loop's own reader; the deeper cure (a
        // separate stdin owner) is a follow-up — this is the consistency fix.
        Err(_) => None,                        // genuine read error → no human
        Ok(_) => Some(buf.trim().to_string()), // EOF (Ok(0)) → Some("")
    }
}

/// #728: production reader for `request_user_input` — print the question, read
/// one line from stdin (the same blocking shape as the permission prompt) and
/// interpret it via [`interpret_user_line`]. Closed stdin / read error → `None`
/// (no human to answer), never a hang.
pub(crate) fn prompt_user_input(w: &PromptWindow, question: &str) -> Option<String> {
    w.ask(&format!("? {question}\n> ")).ok();
    let mut answer = String::new();
    // `read_line_into`, not `read_line`: the BYTE COUNT is load-bearing here —
    // `interpret_user_line` distinguishes EOF (`Ok(0)`, a deliberate empty
    // answer from Ctrl-D) from a genuine read error (no human available).
    let read = w.read_line_into(&mut answer);
    let line = interpret_user_line(read, &answer)?;
    if is_slash_command_at_prompt(&line) {
        // A leading-slash answer is a TUI command intent, NOT an answer for the
        // model — never hand it over. A small model will try to *run* it (e.g.
        // `/exit` -> `run_command exit`), which OCAP then denies. Refuse here;
        // Ctrl-C returns to the main prompt where slash commands work.
        w.notice(
            "(slash commands aren't answers to this question - Ctrl-C to return \
             to the prompt, then use /exit, /model, ...)",
        )
        .ok();
        return None;
    }
    Some(line)
}

/// A leading-slash answer at a `request_user_input` prompt is a TUI command
/// intent, not an answer to hand to the model. Pure, so it's unit-testable.
fn is_slash_command_at_prompt(answer: &str) -> bool {
    answer.trim_start().starts_with('/')
}

#[cfg(test)]
mod slash_prompt_tests {
    use super::is_slash_command_at_prompt;

    #[test]
    fn refuses_slash_commands_as_tool_answers() {
        // Regression: `/exit` typed at a `request_user_input` prompt was sent to
        // the model, which ran it as a shell command -> OCAP denial.
        assert!(is_slash_command_at_prompt("/exit"));
        assert!(is_slash_command_at_prompt("  /quit"));
        assert!(is_slash_command_at_prompt("/model qwen2.5-coder:7b"));
        // A plain answer (even one containing a slash) is a real answer.
        assert!(!is_slash_command_at_prompt("qwen2.5-coder:7b"));
        assert!(!is_slash_command_at_prompt("use a/b testing"));
        assert!(!is_slash_command_at_prompt(""));
    }
}

/// Session-owned prompted-permission state, lent to the gate each turn (the
/// `note_nudge` ownership pattern). Session grants/denials live HERE — not
/// in the session's operating key, which is never widened — and evaporate
/// with the process: a restart starts from the configured policy again.
#[derive(Default)]
pub(crate) struct PermissionPromptState {
    /// A4/W6 (opt-in, `NEWT_WEB_DECISIONS`): when set, a permission decision is
    /// PUBLISHED to this store and answered from an attach surface (the web)
    /// rather than the TTY — the gate polls [`ConversationStore::take_permission_decision`]
    /// for the operator's verdict. `None` (the default) keeps the canonical TTY
    /// prompt bit-for-bit, so a normal session is unaffected. The simultaneous
    /// TTY-vs-web race is a follow-up (it needs a pollable single-key read on
    /// the #1312-sensitive terminal arbiter; validated on a real PTY).
    pub(crate) web_store: Option<newt_core::ConversationStore>,
    /// `(kind, target)` pairs the human allowed for the rest of the session.
    session_grants: std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    /// `(kind, target)` pairs the human denied for the rest of the session
    /// (`[D]eny always`) — auto-denied without re-prompting.
    session_denials: std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    /// #904: `(kind, target)` pairs the human denied PERMANENTLY
    /// (`[P]ermanently deny`) — loaded from `~/.newt/permission-denials.jsonl`
    /// at session start and auto-denied without re-prompting, across restarts.
    /// Deny-only, so reading it back can never widen authority.
    persistent_denials: std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    /// #1057: one-shot exec grants left by a model-driven `request_permissions`
    /// allow-once. That grant authorizes the model's UPCOMING retry (a separate
    /// tool call), but the consult that recorded it executes nothing and its
    /// widened caveats are discarded — so without carrying it the retry
    /// re-denies. Each entry is consumed by the NEXT matching `run_command`
    /// (exactly once, so allow-once semantics hold). Exec targets are stored by
    /// basename so a `python3` grant covers `/usr/bin/python3`, consistent with
    /// the enforcement leash. Grant-only, and folded through `mint` (⊑ root), so
    /// the attenuation-only invariant holds.
    pending_once_grants: std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    /// Every prompted decision this session, in prompt order — what
    /// `/permissions` lists. Also appended to the durable log as made.
    pub(crate) decisions: Vec<newt_core::PermissionRecord>,
    /// Track O (#1131): the durable OCAP policy loaded from `~/.newt/ocap/*.toml`
    /// at session start (empty when the store is absent). Consulted by the gate
    /// BEFORE prompting — a durable `deny` refuses, a durable `approve`
    /// pre-answers (danger-gated) — so the accumulation loop loosens the leash
    /// with use. Read-only this session; never widens the default-deny floor.
    pub(crate) ocap_policy: newt_core::ocap_store::PolicySet,
}

/// The exec program basename — mirrors `newt_core` `exec_allowlist_name` (splits
/// on `/` and `\`) so a grant of `python3` reconciles with a run of
/// `/usr/bin/python3`. Used ONLY for the prompt-skip memo and the one-shot
/// pending-grant match; the authority check itself (`permits_exec`) stays EXACT
/// — that method is the sole *unconfined* gate for model-authored `verify`
/// strings, and basename-matching it would be an ACE primitive (#1057 review).
pub(crate) fn exec_grant_basename(cmd: &str) -> &str {
    cmd.rsplit(['/', '\\'])
        .find(|p| !p.is_empty())
        .unwrap_or(cmd)
}

/// Does a durable session grant cover `req`? Exact for every axis; for exec the
/// set is consulted the way the enforcement leash matches — the target verbatim
/// OR its basename in the set — so a `python3` grant covers `/usr/bin/python3`,
/// while a full-path grant does NOT widen to a bare or different-path program
/// (pin-exact preserved). This mirrors agent-bridle `exec_scope_allows`, so the
/// prompt is never skipped for something the leash would then deny.
pub(crate) fn session_grant_covers(
    grants: &std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    req: &newt_core::PermissionRequest,
) -> bool {
    if grants.contains(&(req.kind, req.target.clone())) {
        return true;
    }
    req.kind == newt_core::DenialKind::Exec
        && grants.contains(&(
            newt_core::DenialKind::Exec,
            exec_grant_basename(&req.target).to_string(),
        ))
}

/// Consume a one-shot pending grant matching `req`, returning the grant key to
/// fold into the minted caveats so the retry actually runs. Exec entries are
/// basename-keyed, and the query is basename-normalized to match. One-shot:
/// removed on hit, so the next identical op re-prompts (allow-once preserved).
pub(crate) fn take_pending_once(
    pending: &mut std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    req: &newt_core::PermissionRequest,
) -> Option<(newt_core::DenialKind, String)> {
    let exact = (req.kind, req.target.clone());
    if pending.remove(&exact) {
        return Some(exact);
    }
    if req.kind == newt_core::DenialKind::Exec {
        let base = (
            newt_core::DenialKind::Exec,
            exec_grant_basename(&req.target).to_string(),
        );
        if pending.remove(&base) {
            return Some(base);
        }
    }
    None
}

impl PermissionPromptState {
    /// Load the persistent denylist from `path` into a fresh state (#904). A
    /// missing file yields an empty denylist. Called once at session start.
    pub(crate) fn with_persistent_denials(path: Option<&std::path::Path>) -> Self {
        let persistent_denials = path
            .map(|p| newt_core::load_denials(p).into_iter().collect())
            .unwrap_or_default();
        Self {
            persistent_denials,
            ..Self::default()
        }
    }
}

/// The TUI's [`newt_core::PermissionGate`]: prompts the human on a denial,
/// records each decision, and — on allow — RE-MINTS a fresh operating
/// authority from the user root as (session baseline ∪ grants), per the
/// #263 design. Attenuation-only is preserved: the session's live key and
/// enforced baseline are never touched; the minted caveats exist only for
/// the re-executed call (and are re-minted on demand for session grants).
///
/// `ask_human` is the interaction seam: production wires
/// [`prompt_permission_choice`]; tests inject a scripted closure.
///
/// It takes a [`PromptWindow`] because every blocking prompt does. The token is
/// constructed at the ONE site inside [`PromptPermissionGate::ask`] below, not
/// threaded in from callers — so this type's shape changes while all six
/// `gate.ask` call sites in `newt-core`'s tool dispatcher stay untouched.
pub(crate) struct PromptPermissionGate<'a, F: FnMut(&PromptWindow, &str) -> PromptChoice> {
    pub(crate) state: &'a mut PermissionPromptState,
    /// The session's enforced caveats at turn start — the re-mint baseline.
    /// When a `/posture` preset is active this is ALREADY the clamped (base ∩
    /// preset) value, so `widen_caveats` starts below the preset ceiling.
    pub(crate) base: newt_core::Caveats,
    /// Per-user root key path; `None` degrades the re-mint to a plain
    /// caveats value (the same degradation as `SessionCapability`).
    pub(crate) key_path: Option<std::path::PathBuf>,
    /// Conversation id the decisions are recorded under.
    pub(crate) conversation_id: String,
    /// Durable decision log (`~/.newt/permission-log.jsonl`); `None` keeps
    /// the in-session list only.
    pub(crate) log_path: Option<std::path::PathBuf>,
    /// #904: durable denylist (`~/.newt/permission-denials.jsonl`) appended on
    /// `[P]ermanently deny`. `None` degrades that choice to session-scoped (like
    /// `[D]eny always`) — the deny still holds, it just won't survive a restart.
    pub(crate) denials_path: Option<std::path::PathBuf>,
    /// #904: user config path (`~/.newt/config.toml`) that `[A]llow permanently`
    /// appends a net host to (`[tui.permissions] net`, comment-preserving).
    /// `None` degrades that choice to a session grant (durable widen unavailable).
    pub(crate) config_path: Option<std::path::PathBuf>,
    /// #307 FLOOR: the active named-permission-preset clamp, if any. The minted
    /// authority is re-`meet`-ed with this ceiling so a session-grant can NEVER
    /// re-add a target the preset denied — `widen_caveats` adds to `Only` sets,
    /// including ones the preset emptied, so the post-mint clamp is the
    /// load-bearing point that keeps the floor honest against grants. `None`
    /// (no active preset) leaves the #263 mint bit-for-bit.
    pub(crate) preset_clamp: Option<newt_core::Caveats>,
    /// Facade P1b (§7-F3/F4): the pure-DATA danger-tier table. Used to render
    /// the prompt's system-computed blast-radius line and to refuse a plain
    /// `[s]ession allow` of a high-danger target (interpreter exec / broad fs
    /// root). See [`danger`].
    pub(crate) danger: danger::DangerTable,
    pub(crate) color: bool,
    pub(crate) verbose: bool,
    /// Bound the web-decision wait below the store's five-minute TTL. Tests
    /// inject a short duration so the timeout path is exercised without a
    /// multi-minute test.
    pub(crate) web_decision_timeout: Duration,
    pub(crate) ask_human: F,
}

impl<F: FnMut(&PromptWindow, &str) -> PromptChoice> PromptPermissionGate<'_, F> {
    /// Record one decision: into the session list (for `/permissions`) and
    /// appended to the durable log. A log-write failure is reported but
    /// never blocks the decision — the record is a review artifact, not a
    /// gate.
    fn record(&mut self, req: &newt_core::PermissionRequest, decision: &str, scope: &str) {
        let rec = newt_core::PermissionRecord::new(
            &self.conversation_id,
            &req.tool,
            req.kind,
            &req.target,
            decision,
            scope,
        );
        if let Some(path) = self.log_path.as_deref() {
            if let Err(e) = rec.append_jsonl(path) {
                print_newt(
                    &format!("warning: permission log write failed: {e}"),
                    self.color,
                    self.verbose,
                );
            }
        }
        self.state.decisions.push(rec);
    }

    /// Mint the widened authority for an allow: policy = baseline ∪ every
    /// session grant ∪ the once-grants of this consult, re-rooted from the
    /// per-user key when available. The live operating key is NEVER widened
    /// — this is a fresh, narrower-than-root delegation (issue #263's
    /// "re-mint from root" rule). Without a usable key the value degrades
    /// to the plain policy, mirroring `SessionCapability::establish`.
    fn mint(&self, once_grants: &[(newt_core::DenialKind, String)]) -> newt_core::Caveats {
        let mut grants: Vec<(newt_core::DenialKind, String)> =
            self.state.session_grants.iter().cloned().collect();
        grants.extend(once_grants.iter().cloned());
        let mut policy = newt_core::widen_caveats(&self.base, &grants);
        // #307 FLOOR: re-clamp the widened policy under the active preset. This
        // is the load-bearing intersection — `widen_caveats` can re-populate an
        // `Only` set the preset emptied (e.g. add `rm` to an exec scope the
        // readonly preset pinned to none), so without this `meet` a session
        // grant would silently raise authority above the preset. With it, a
        // grant can never exceed the preset ceiling.
        if let Some(clamp) = &self.preset_clamp {
            policy = policy.meet(clamp);
        }
        match self
            .key_path
            .as_deref()
            .and_then(|p| mint_operating_key(p, &policy).ok())
        {
            Some(key) => newt_identity::enforced_caveats(&key).unwrap_or(policy),
            None => policy,
        }
    }

    /// A4/W6: publish the pending decision to the store and poll for the
    /// operator's WEB verdict (opt-in). The web NAMES a verdict; this maps it to
    /// the same `PromptChoice` a TTY key would, so every downstream arm — the
    /// high-danger `[s]ession` refusal, the mint, the re-exec — is reused
    /// unchanged. Fails closed (`Deny`) if the store can't publish/read. The
    /// danger tier is GATE-STAMPED here (the web renders it, never classifies).
    /// Blocks polling like the TTY read would (no auto-deny); it runs on the
    /// turn's own thread, so the UI never hangs.
    fn await_web_decision(
        &self,
        store: &newt_core::ConversationStore,
        w: &newt_core::tty::PromptWindow,
        req: &newt_core::PermissionRequest,
    ) -> PromptChoice {
        let requests_json = serde_json::to_string(req).unwrap_or_default();
        let tier = if self.danger.classify(req.kind, &req.target) == danger::DangerTier::High {
            "\"high\""
        } else {
            "\"low\""
        };
        let request_id =
            match store.publish_permission_request(&self.conversation_id, &requests_json, tier) {
                Ok(id) => id,
                Err(_) => return PromptChoice::Deny,
            };
        w.notice(&newt_line(
            &format!("awaiting a decision from the web for `{}`…", req.target),
            self.color,
            self.verbose,
        ))
        .ok();
        let deadline = Instant::now() + self.web_decision_timeout;
        loop {
            match store.take_permission_decision(&self.conversation_id, &request_id) {
                Ok(Some(verdict)) => return verdict_to_choice(verdict),
                Ok(None) if Instant::now() >= deadline => {
                    // Resolve through the same CAS as a TTY answer. If a web
                    // answer won the race, consume that verdict; otherwise
                    // the timeout is a fail-closed denial.
                    match store.resolve_permission_request(
                        &self.conversation_id,
                        &request_id,
                        "expired",
                    ) {
                        Ok(true) => return PromptChoice::Deny,
                        Ok(false) => match store
                            .take_permission_decision(&self.conversation_id, &request_id)
                        {
                            Ok(Some(verdict)) => return verdict_to_choice(verdict),
                            Ok(None) | Err(_) => return PromptChoice::Deny,
                        },
                        Err(_) => return PromptChoice::Deny,
                    }
                }
                Ok(None) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(remaining.min(Duration::from_millis(200)));
                }
                Err(_) => return PromptChoice::Deny,
            }
        }
    }
}

/// Map a web-named [`newt_core::Verdict`] to the [`PromptChoice`] a TTY key
/// would produce, so a web answer flows through the exact same gate arms.
fn verdict_to_choice(v: newt_core::Verdict) -> PromptChoice {
    match v {
        newt_core::Verdict::AllowOnce => PromptChoice::AllowOnce,
        newt_core::Verdict::AllowSession => PromptChoice::AllowSession,
        newt_core::Verdict::Deny => PromptChoice::Deny,
    }
}

impl<F: FnMut(&PromptWindow, &str) -> PromptChoice> newt_core::PermissionGate
    for PromptPermissionGate<'_, F>
{
    fn ask(&mut self, requests: &[newt_core::PermissionRequest]) -> newt_core::PermissionDecision {
        use newt_core::PermissionDecision::{Allow, Deny};
        if requests.is_empty() {
            return Deny;
        }
        // `[D]eny always` (session) and `[P]ermanently deny` (#904, durable)
        // both short-circuit without re-prompting and without re-recording —
        // the deny was recorded when chosen. The persistent set was loaded from
        // disk at session start, so a permanent deny survives restarts.
        // A durable OCAP `deny` (`~/.newt/ocap/deny.toml`) refuses before any
        // prompt, exactly like a session/permanent deny — same short-circuit,
        // no re-record (the policy file IS the audit record). The store applies
        // the contract precedence (deny > passkey > ask > approve), so a target
        // that is both denied and approved returns `Deny` here.
        if requests.iter().any(|r| {
            let key = (r.kind, r.target.clone());
            self.state.session_denials.contains(&key)
                || self.state.persistent_denials.contains(&key)
                || newt_core::ocap_store::evaluate_request(
                    &self.state.ocap_policy,
                    r.kind,
                    &r.target,
                ) == Some(newt_core::ocap_store::Verdict::Deny)
        }) {
            return Deny;
        }
        let mut once_grants: Vec<(newt_core::DenialKind, String)> = Vec::new();
        // A4/W6: a cheap clone of the opt-in web-decision store (None normally),
        // taken once so the per-request `self.record`/grant mutations in the
        // loop don't collide with the shared-borrow at the prompt seam below.
        let web = self.state.web_store.clone();
        for req in requests {
            // #1056 FLOOR: a readonly `/posture` denies LOCAL git writes just as it
            // denies fs writes. The git-write capability is non-axis, so `mint`'s
            // preset re-clamp can't attenuate it (it widens nothing) — enforce the
            // floor HERE: if a preset is active and projects no git commit
            // authority, refuse the grant outright (even `[a]llow once`), so a git
            // write can never pierce the readonly clamp.
            if req.kind == newt_core::DenialKind::GitWrite {
                if let Some(clamp) = &self.preset_clamp {
                    if !newt_core::git_caveats::GitCaveats::from_session(clamp).permits_commit() {
                        self.record(req, "deny", "preset-floor-git-readonly");
                        return Deny;
                    }
                }
            }
            // A session grant covers this target: no re-prompt, no new
            // record — the decision was recorded when the human made it.
            // (Exec matches by basename so a `python3` grant covers
            // `/usr/bin/python3`, consistent with the enforcement leash.)
            if session_grant_covers(&self.state.session_grants, req) {
                continue;
            }
            // Track O (#1131): a durable OCAP `approve` pre-answers the prompt —
            // the "Always allow" the accumulation loop earns. It is folded into
            // the minted authority via `once_grants` (like an allow-once), so the
            // enforcement leash actually permits the op, not merely skips the
            // ask. Danger-gated: a high-danger target (interpreter exec / broad
            // fs root) is NEVER auto-allowed from the store — it fails closed to
            // the prompt, the same P1b ceiling that refuses a plain `[s]ession
            // allow` for high-danger. (`validate_approve` keeps such targets out
            // of approve.toml in the first place; this is the belt-and-suspenders
            // enforcement.)
            if newt_core::ocap_store::evaluate_request(
                &self.state.ocap_policy,
                req.kind,
                &req.target,
            ) == Some(newt_core::ocap_store::Verdict::Approve)
                && self.danger.classify(req.kind, &req.target) != danger::DangerTier::High
            {
                self.record(req, "allow", "ocap-approve");
                once_grants.push((req.kind, req.target.clone()));
                continue;
            }
            // #1057: a one-shot pending grant left by a prior request_permissions
            // allow-once covers this retry — consume it (exactly once) and fold
            // it into the minted caveats so the command actually runs. Only an
            // actual operation (a `run_command` retry) consumes it, never another
            // request_permissions ask (which would burn the grant before the
            // retry and re-deny it).
            if req.tool != "request_permissions" {
                if let Some(key) = take_pending_once(&mut self.state.pending_once_grants, req) {
                    self.record(req, "allow", "once");
                    once_grants.push(key);
                    continue;
                }
            }
            // ---------------- THE seam ----------------
            // The ONE place in the workspace that constructs a `PromptWindow`.
            // `suspend_for_prompt()` takes stdin AND erases every registered
            // ephemeral writer before it returns, then holds the shared ticker
            // still until `w` drops — so the question lands on a clean row and
            // survives for as long as the operator is reading it.
            //
            // Constructed HERE rather than threaded in from callers: that is
            // what lets all six `gate.ask` sites in `newt-core`'s tool
            // dispatcher inherit the guarantee without a single line changing
            // at any of them, and what makes a seventh site safe by default.
            let w = Terminal::suspend_for_prompt();
            // A4/W6: when a web-decision store is configured (opt-in), publish
            // the decision and poll for the operator's web verdict instead of
            // reading the TTY. Off by default → the canonical prompt is
            // unchanged. `web` is a cheap clone taken before the loop so the
            // per-request `self.record`/grant mutations below don't conflict.
            let choice = match &web {
                Some(store) => self.await_web_decision(store, &w, req),
                None => (self.ask_human)(&w, &permission_prompt_text(req, &self.danger)),
            };
            match choice {
                PromptChoice::AllowOnce => {
                    self.record(req, "allow", "once");
                    once_grants.push((req.kind, req.target.clone()));
                    // #1057: a model-driven `request_permissions` allow-once is
                    // authorizing the model's UPCOMING retry — a separate tool
                    // call whose caveats are re-derived from scratch, so the
                    // widening above is discarded. Carry it as a ONE-SHOT pending
                    // grant (exec keyed by basename) the retry consumes exactly
                    // once. A denial-driven allow-once re-execs in place and
                    // needs no carry, so this is scoped to request_permissions.
                    if req.tool == "request_permissions" {
                        let key = if req.kind == newt_core::DenialKind::Exec {
                            (req.kind, exec_grant_basename(&req.target).to_string())
                        } else {
                            (req.kind, req.target.clone())
                        };
                        self.state.pending_once_grants.insert(key);
                    }
                }
                PromptChoice::AllowSession => {
                    // Facade P1b (§7-F3/F4): a high-danger target (interpreter
                    // exec / broad fs root) is NOT session-allowable. A standing
                    // grant of arbitrary execution or the whole tree is exactly
                    // the catastrophic over-grant P1b closes — the interpreter's
                    // children fork outside the per-spawn interceptor, and a
                    // prefix grant of `/` is a whole-tree permit. Refuse:
                    // fail-closed to a deny so the operator must `[a]llow once`
                    // per op. `[k]ey allow` (step-up, P3, unbuilt) is the
                    // intended standing path for these. The prompt menu already
                    // omits `[s]` for high-danger; this is the enforcement that
                    // makes a muscle-memory `s` safe.
                    if self.danger.classify(req.kind, &req.target) == danger::DangerTier::High {
                        self.record(req, "deny", "session-allow-refused-high-danger");
                        // Through the window, not `println!`: the refusal is
                        // printed while a question is still on screen, so it
                        // must go through the arbiter or it races the ticker.
                        w.notice(&newt_line(
                            &format!(
                                "session allow refused for high-danger `{}` — \
                                 allow once per op or deny (step-up is the future path)",
                                req.target
                            ),
                            self.color,
                            self.verbose,
                        ))
                        .ok();
                        return Deny;
                    }
                    self.record(req, "allow", "session");
                    self.state
                        .session_grants
                        .insert((req.kind, req.target.clone()));
                }
                PromptChoice::AllowPermanent => {
                    // #904: durably grant a NET host by appending it to
                    // `[tui.permissions] net` (comment-preserving). It is ALSO
                    // added to session_grants so it holds immediately this
                    // session — the durable grant only takes effect on the next
                    // config load. The grant still flows through `mint()`
                    // (re-minted ⊑ root), so the attenuation-only invariant holds.
                    //
                    // Only net is durably grantable (the only per-target config
                    // allowlist). A non-net `[A]` (not offered in the menu, but a
                    // muscle-memory keypress) degrades to a session grant, and a
                    // high-danger net host is refused like `[s]ession allow`.
                    if req.kind != newt_core::DenialKind::Net {
                        self.record(req, "allow", "session");
                        self.state
                            .session_grants
                            .insert((req.kind, req.target.clone()));
                        continue;
                    }
                    if self.danger.classify(req.kind, &req.target) == danger::DangerTier::High {
                        self.record(req, "deny", "permanent-allow-refused-high-danger");
                        w.notice(&newt_line(
                            &format!("permanent allow refused for high-danger `{}`", req.target),
                            self.color,
                            self.verbose,
                        ))
                        .ok();
                        return Deny;
                    }
                    self.record(req, "allow", "permanent");
                    match self.config_path.as_deref() {
                        Some(path) => {
                            if let Err(e) =
                                newt_core::Config::append_permission_net_host(path, &req.target)
                            {
                                w.notice(&newt_line(
                                    &format!(
                                        "warning: could not persist net grant to config: {e} \
                                         (granted for this session only)"
                                    ),
                                    self.color,
                                    self.verbose,
                                ))
                                .ok();
                            } else {
                                w.notice(&newt_line(
                                    &format!(
                                        "added `{}` to [tui.permissions] net — future sessions \
                                         will not prompt for it",
                                        req.target
                                    ),
                                    self.color,
                                    self.verbose,
                                ))
                                .ok();
                            }
                        }
                        None => {
                            w.notice(&newt_line(
                                "no config path this session — net grant is session-only",
                                self.color,
                                self.verbose,
                            ))
                            .ok();
                        }
                    }
                    self.state
                        .session_grants
                        .insert((req.kind, req.target.clone()));
                }
                PromptChoice::Deny => {
                    self.record(req, "deny", "once");
                    return Deny;
                }
                PromptChoice::DenyAlways => {
                    self.record(req, "deny", "session");
                    self.state
                        .session_denials
                        .insert((req.kind, req.target.clone()));
                    return Deny;
                }
                PromptChoice::DenyPermanent => {
                    // #904: record + persist so this (kind, target) is denied
                    // across restarts. A persist-write failure is reported but
                    // never blocks the decision, and the in-memory set is still
                    // updated so it holds for the rest of THIS session even if
                    // the disk write failed.
                    self.record(req, "deny", "permanent");
                    if let Some(path) = self.denials_path.as_deref() {
                        if let Err(e) = newt_core::append_denial(path, req.kind, &req.target) {
                            print_newt(
                                &format!("warning: permission denylist write failed: {e}"),
                                self.color,
                                self.verbose,
                            );
                        }
                    }
                    self.state
                        .persistent_denials
                        .insert((req.kind, req.target.clone()));
                    return Deny;
                }
            }
        }
        Allow(self.mint(&once_grants))
    }

    /// #728: ask the human a free-text question and read back the answer. This
    /// is the same operator-present gate the permission prompt uses, so it is
    /// only constructed for an interactive session (`prompt_permissions_enabled`);
    /// headless callers hold `None` and never reach here. A closed stdin returns
    /// `None`, which the `request_user_input` tool renders as "no human
    /// available" — never a hang.
    fn ask_question(&mut self, question: &str) -> Option<String> {
        // Same seam as `ask`: suspend first, then ask. `prompt_user_input`
        // cannot be called any other way.
        let w = Terminal::suspend_for_prompt();
        prompt_user_input(&w, question)
    }
}

#[cfg(test)]
mod permission_prompt_tests {
    use super::*;
    use crate::mcp::Mcp;
    use crate::{close_out_message, help_lines, permissions_command_lines, ActivePosture};
    use newt_core::caveats::{Caveats, CountBound, Scope};
    use newt_core::{CaveatsExt as _, DenialKind, PermissionGate as _, PermissionRequest};
    use std::cell::Cell;
    use std::rc::Rc;

    fn base_caveats(ws: &str) -> Caveats {
        Caveats {
            fs_read: Scope::only([ws.to_string()]),
            fs_write: Scope::only([ws.to_string()]),
            exec: Scope::only(["cargo".to_string()]),
            net: Scope::none(),
            max_calls: CountBound::Unlimited,
            valid_for_generation: Scope::All,
        }
    }

    fn exec_request(target: &str) -> PermissionRequest {
        PermissionRequest {
            tool: "run_command".to_string(),
            kind: DenialKind::Exec,
            target: target.to_string(),
            reason: format!("exec of \"{target}\" is not within the granted authority"),
        }
    }

    /// A4/W6 (part 2): with `web_store` set, the gate PUBLISHES the decision and
    /// consumes the operator's WEB verdict — it never reads the TTY. A concurrent
    /// answerer stands in for the web POST; allow-once → the gate returns `Allow`.
    /// This grounds the store's publish/answer/take methods against the gate's
    /// own poll loop (the map from `Verdict` to the reused `PromptChoice` arms).
    #[test]
    fn web_decisions_publish_and_consume_a_web_verdict_without_the_tty() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let store = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let conv = store.create("s", None).unwrap();

        // Stand in for the web POST: wait for the gate to publish, then answer.
        let answerer_store = store.clone();
        let answer_conv = conv.clone();
        let answerer = std::thread::spawn(move || {
            for _ in 0..500 {
                if let Ok(Some(p)) = answerer_store.pending_permission_request(&answer_conv) {
                    answerer_store
                        .answer_permission_request(
                            &answer_conv,
                            &p.request_id,
                            newt_core::Verdict::AllowOnce,
                        )
                        .unwrap();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("the gate never published a permission request");
        });

        let mut state = PermissionPromptState {
            web_store: Some(store.clone()),
            ..Default::default()
        };
        let mut gate = PromptPermissionGate {
            state: &mut state,
            base: Caveats::default(),
            key_path: None,
            conversation_id: conv.clone(),
            log_path: None,
            denials_path: None,
            config_path: None,
            preset_clamp: None,
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            web_decision_timeout: Duration::from_secs(2),
            // Proof the TTY is bypassed when web decisions are on.
            ask_human: |_w: &PromptWindow, _p: &str| {
                panic!("the TTY must not be read when web decisions are enabled")
            },
        };
        let decision = gate.ask(&[exec_request("bash")]);
        answerer.join().unwrap();
        assert!(
            matches!(decision, newt_core::PermissionDecision::Allow(_)),
            "a web allow-once verdict must produce Allow"
        );
    }

    #[test]
    fn web_decision_timeout_resolves_and_denies_without_hanging() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let store = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let conv = store.create("s", None).unwrap();
        let mut state = PermissionPromptState {
            web_store: Some(store.clone()),
            ..Default::default()
        };
        let mut gate = PromptPermissionGate {
            state: &mut state,
            base: Caveats::default(),
            key_path: None,
            conversation_id: conv.clone(),
            log_path: None,
            denials_path: None,
            config_path: None,
            preset_clamp: None,
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            web_decision_timeout: Duration::from_millis(50),
            ask_human: |_w: &PromptWindow, _p: &str| {
                panic!("the TTY must not be read when web decisions are enabled")
            },
        };
        let started = Instant::now();
        let decision = gate.ask(&[exec_request("bash")]);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(decision, newt_core::PermissionDecision::Deny));
        assert_eq!(store.pending_permission_request(&conv).unwrap(), None);
    }

    /// A gate whose "human" is a script of choices; counts every prompt.
    fn scripted_gate<'a>(
        state: &'a mut PermissionPromptState,
        base: Caveats,
        key_path: Option<std::path::PathBuf>,
        log_path: Option<std::path::PathBuf>,
        script: Vec<PromptChoice>,
        prompts: Rc<Cell<usize>>,
    ) -> PromptPermissionGate<'a, impl FnMut(&PromptWindow, &str) -> PromptChoice> {
        let mut script = script.into_iter();
        PromptPermissionGate {
            state,
            base,
            key_path,
            conversation_id: "conv-test".to_string(),
            log_path,
            denials_path: None,
            config_path: None,
            preset_clamp: None,
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            web_decision_timeout: Duration::from_secs(2),
            ask_human: move |_w: &PromptWindow, _prompt: &str| {
                prompts.set(prompts.get() + 1);
                script.next().expect("script exhausted — unexpected prompt")
            },
        }
    }

    /// #307: a scripted gate carrying an active preset clamp (the floor). Same
    /// as [`scripted_gate`] but with `preset_clamp` set, for the floor tests.
    #[allow(clippy::too_many_arguments)]
    fn scripted_gate_with_clamp<'a>(
        state: &'a mut PermissionPromptState,
        base: Caveats,
        key_path: Option<std::path::PathBuf>,
        log_path: Option<std::path::PathBuf>,
        preset_clamp: Caveats,
        script: Vec<PromptChoice>,
        prompts: Rc<Cell<usize>>,
    ) -> PromptPermissionGate<'a, impl FnMut(&PromptWindow, &str) -> PromptChoice> {
        let mut script = script.into_iter();
        PromptPermissionGate {
            state,
            base,
            key_path,
            conversation_id: "conv-test".to_string(),
            log_path,
            denials_path: None,
            config_path: None,
            preset_clamp: Some(preset_clamp),
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            web_decision_timeout: Duration::from_secs(2),
            ask_human: move |_w: &PromptWindow, _prompt: &str| {
                prompts.set(prompts.get() + 1);
                script.next().expect("script exhausted — unexpected prompt")
            },
        }
    }

    #[test]
    fn parse_choice_maps_the_sketch_keys_and_defaults_to_deny() {
        assert_eq!(parse_permission_choice("a"), PromptChoice::AllowOnce);
        assert_eq!(parse_permission_choice(" a \n"), PromptChoice::AllowOnce);
        assert_eq!(parse_permission_choice("s"), PromptChoice::AllowSession);
        assert_eq!(parse_permission_choice("d"), PromptChoice::Deny);
        assert_eq!(parse_permission_choice("D"), PromptChoice::DenyAlways);
        // #904: the durable tiers.
        assert_eq!(parse_permission_choice("P"), PromptChoice::DenyPermanent);
        assert_eq!(parse_permission_choice("A"), PromptChoice::AllowPermanent);
        // Default = deny: empty (just Enter / EOF), garbage, near-misses.
        assert_eq!(parse_permission_choice(""), PromptChoice::Deny);
        assert_eq!(parse_permission_choice("yes"), PromptChoice::Deny);
        // Case-significant: capital `S` is not a choice (session is lowercase `s`).
        assert_eq!(parse_permission_choice("S"), PromptChoice::Deny);
    }

    #[test]
    fn interpret_user_line_maps_eof_to_empty_and_errors_to_none() {
        // #783 (Bug C): EOF (Ok(0)) is now mapped to Some("") — an empty,
        // deliberate answer — consistent with prompt_permission_choice, which
        // treats the same EOF as a valid response. Only a genuine read error →
        // None ("no human available"). The Ok(0) == Some("") assertion is RED on
        // the old `Ok(0) | Err(_) => None`.
        assert_eq!(interpret_user_line(Ok(0), ""), Some(String::new()));
        assert_eq!(
            interpret_user_line(Err(io::Error::from(io::ErrorKind::Other)), ""),
            None
        );
        assert_eq!(interpret_user_line(Ok(5), "hi\n"), Some("hi".to_string()));
    }

    // NOTE: `prompt_stdin_guard_marks_prompt_ownership_and_clears_on_drop` and
    // `watcher_read_token_blocks_prompt_entry_until_the_read_finishes` moved to
    // `newt_core::tty::arbiter` with the mechanism they test. Stdin ownership
    // is no longer this module's concern — one arbiter owns both halves of the
    // terminal.

    #[test]
    fn prompt_text_names_tool_target_axis_and_choices() {
        let danger = danger::DangerTable::builtin();
        let text = permission_prompt_text(&exec_request("npm"), &danger);
        assert!(
            text.contains("run_command wants to run `npm`"),
            "got: {text}"
        );
        assert!(
            text.contains("outside the granted exec allowlist"),
            "got: {text}"
        );
        assert!(
            text.contains("not within the granted authority"),
            "reason shown: {text}"
        );
        // `npm` is a narrow command → low-danger → the full menu (incl. session).
        assert!(
            text.contains("[a]llow once   [s]ession allow   [d]eny (default)   [D]eny always"),
            "got: {text}"
        );
        // Axis wording follows the kind; an empty reason adds no parens.
        let read = permission_prompt_text(
            &PermissionRequest {
                tool: "read_file".to_string(),
                kind: DenialKind::FsRead,
                target: "/etc/hosts".to_string(),
                reason: String::new(),
            },
            &danger,
        );
        assert!(read.contains("wants to read `/etc/hosts`"), "got: {read}");
        assert!(read.contains("fs_read scope"), "got: {read}");
        assert!(!read.contains("()"), "no empty reason parens: {read}");
        let net = permission_prompt_text(
            &PermissionRequest {
                tool: "web_fetch".to_string(),
                kind: DenialKind::Net,
                target: "docs.rs".to_string(),
                reason: String::new(),
            },
            &danger,
        );
        assert!(net.contains("wants to reach `docs.rs`"), "got: {net}");
        let write = permission_prompt_text(
            &PermissionRequest {
                tool: "edit_file".to_string(),
                kind: DenialKind::FsWrite,
                target: "/ws/f".to_string(),
                reason: String::new(),
            },
            &danger,
        );
        assert!(write.contains("wants to write `/ws/f`"), "got: {write}");
    }

    /// Facade P1b (§7-F4) — the confused-deputy-through-the-operator fix. A
    /// `request_permissions{capability:"exec", target:"bash", reason:"…benign…"}`
    /// must show a SYSTEM-computed blast-radius line the model cannot forge, and
    /// the benign model `reason` must NOT suppress it (it is labelled untrusted,
    /// not rendered as harness fact). RED on today's code: the pre-P1b prompt
    /// shows only the verbatim `reason` and no danger annotation.
    #[test]
    fn high_danger_prompt_shows_blast_radius_and_labels_reason_untrusted() {
        let danger = danger::DangerTable::builtin();
        // The exact attack from §7-F4: a catastrophic grant under a benign cover.
        let req = PermissionRequest {
            tool: "request_permissions".to_string(),
            kind: DenialKind::Exec,
            target: "bash".to_string(),
            reason: "list the files in this directory".to_string(),
        };
        let text = permission_prompt_text(&req, &danger);

        // (1) the system blast-radius line is present — a fact, not model text.
        assert!(
            text.contains("⚠") && text.contains("interpreter"),
            "expected a blast-radius warning, got: {text}"
        );
        assert!(
            text.contains("arbitrary command execution"),
            "expected the exec blast radius, got: {text}"
        );
        // The benign reason did NOT suppress the warning — both are present.
        assert!(
            text.contains("list the files in this directory"),
            "the model reason is still shown (as context), got: {text}"
        );
        // (2) the model `reason` is labelled UNTRUSTED, never as harness fact.
        assert!(
            text.contains("model-authored, unverified"),
            "the reason must be labelled untrusted model text, got: {text}"
        );
        // (3) the menu OMITS a plain `[s]ession allow` and notes the refusal.
        assert!(
            !text.contains("[a]llow once   [s]ession allow   [d]eny"),
            "a high-danger grant must NOT offer the plain session-allow menu, got: {text}"
        );
        assert!(
            text.contains("[s]ession allow refused"),
            "the prompt must explain the session-allow refusal, got: {text}"
        );

        // A broad fs root gets the same treatment (root `/`, FsWrite).
        let fs_req = PermissionRequest {
            tool: "request_permissions".to_string(),
            kind: DenialKind::FsWrite,
            target: "/".to_string(),
            reason: "just save one small file".to_string(),
        };
        let fs_text = permission_prompt_text(&fs_req, &danger);
        assert!(
            fs_text.contains("filesystem root") && fs_text.contains("write access to everything"),
            "expected the fs-root blast radius, got: {fs_text}"
        );
        assert!(
            !fs_text.contains("[s]ession allow   [d]eny"),
            "fs-root grant must not offer plain session-allow, got: {fs_text}"
        );
    }

    /// Facade P1b (§7-F3/F4) — a high-danger target is NOT session-allowable.
    /// Choosing `[s]ession allow` for an interpreter is refused (fail-closed to a
    /// deny, no session grant remembered); `[a]llow once` still works. RED on
    /// today's code: the pre-P1b `AllowSession` arm inserts ANY target into
    /// `session_grants` unconditionally — a standing arbitrary-code permit.
    #[test]
    fn high_danger_target_is_not_session_allowable_but_allow_once_works() {
        let base = base_caveats("/ws");

        // `[s]ession allow` of `bash` (an interpreter) is REFUSED.
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        {
            let mut gate = scripted_gate(
                &mut state,
                base.clone(),
                None,
                None,
                vec![PromptChoice::AllowSession],
                prompts.clone(),
            );
            assert!(
                matches!(
                    gate.ask(&[exec_request("bash")]),
                    newt_core::PermissionDecision::Deny
                ),
                "session-allow of an interpreter must be refused (deny)"
            );
        }
        assert!(
            !state
                .session_grants
                .contains(&(DenialKind::Exec, "bash".to_string())),
            "a refused session-allow must leave NO standing grant"
        );
        // The refusal is recorded as a deny, not an allow.
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.decisions[0].decision, "deny");
        assert!(
            state.decisions[0].scope.contains("refused"),
            "the record must mark the high-danger refusal, got: {}",
            state.decisions[0].scope
        );

        // `[a]llow once` of the SAME high-danger target still works (per-op).
        let mut once_state = PermissionPromptState::default();
        let once_prompts = Rc::new(Cell::new(0));
        let mut once_gate = scripted_gate(
            &mut once_state,
            base,
            None,
            None,
            vec![PromptChoice::AllowOnce],
            once_prompts,
        );
        match once_gate.ask(&[exec_request("bash")]) {
            newt_core::PermissionDecision::Allow(c) => {
                assert!(
                    c.permits_exec("bash"),
                    "allow-once grants the target for this op"
                );
            }
            newt_core::PermissionDecision::Deny => {
                panic!("allow-once of a high-danger target must still be permitted")
            }
        }
        drop(once_gate);
        // Allow-once leaves no standing grant — the per-op nature is preserved.
        assert!(once_state.session_grants.is_empty());
    }

    /// Track O (#1131): build a `PolicySet` for the given verdict + inline TOML,
    /// the way the store loads `~/.newt/ocap/<verdict>.toml`.
    fn ocap(
        verdict: newt_core::ocap_store::Verdict,
        toml: &str,
    ) -> newt_core::ocap_store::PolicySet {
        newt_core::ocap_store::build_store(&[(verdict, Some(toml.to_string()))]).0
    }

    /// Track O (#1131): a durable OCAP `approve` pre-answers the prompt — no ask,
    /// and the target is folded into the minted authority so the op actually
    /// runs (not merely prompt-skipped). This is the "Always allow" the
    /// accumulation loop earns.
    #[test]
    fn durable_ocap_approve_allows_without_prompting_and_grants_authority() {
        // `git` is not in base exec (only `cargo`) — normally this prompts.
        let mut state = PermissionPromptState {
            ocap_policy: ocap(
                newt_core::ocap_store::Verdict::Approve,
                "[[exec]]\ntarget = \"git\"\n",
            ),
            ..Default::default()
        };
        let prompts = Rc::new(Cell::new(0));
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            None,
            vec![], // any prompt would panic (script exhausted)
            prompts.clone(),
        );
        match gate.ask(&[exec_request("git")]) {
            newt_core::PermissionDecision::Allow(c) => assert!(
                c.permits_exec("git"),
                "a durable approve must fold `git` into the minted authority"
            ),
            newt_core::PermissionDecision::Deny => panic!("durable approve must allow"),
        }
        assert_eq!(prompts.get(), 0, "durable approve must NOT prompt");
        drop(gate);
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.decisions[0].decision, "allow");
        assert_eq!(state.decisions[0].scope, "ocap-approve");
        // Durable, not session: the store answers again next time; no standing
        // session grant is minted.
        assert!(state.session_grants.is_empty());
    }

    /// Track O (#1131): a durable OCAP `deny` refuses before any prompt — the
    /// durable sibling of `[D]eny always`, sourced from `deny.toml`.
    #[test]
    fn durable_ocap_deny_refuses_without_prompting() {
        let mut state = PermissionPromptState {
            ocap_policy: ocap(
                newt_core::ocap_store::Verdict::Deny,
                "[[exec]]\ntarget = \"git\"\n",
            ),
            ..Default::default()
        };
        let prompts = Rc::new(Cell::new(0));
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            None,
            vec![],
            prompts.clone(),
        );
        assert!(
            matches!(
                gate.ask(&[exec_request("git")]),
                newt_core::PermissionDecision::Deny
            ),
            "a durable deny must refuse"
        );
        assert_eq!(prompts.get(), 0, "durable deny must NOT prompt");
    }

    /// Track O (#1131) danger-gate: a durable `approve` of a HIGH-danger target
    /// (an interpreter) is NOT honored as auto-allow — it fails closed to the
    /// prompt, the same P1b ceiling that refuses a plain `[s]ession allow` for
    /// high-danger. `validate_approve` keeps such targets out of approve.toml;
    /// this is the belt-and-suspenders enforcement at the gate.
    #[test]
    fn durable_ocap_approve_of_high_danger_still_prompts() {
        let mut state = PermissionPromptState {
            ocap_policy: ocap(
                newt_core::ocap_store::Verdict::Approve,
                "[[exec]]\ntarget = \"bash\"\n",
            ),
            ..Default::default()
        };
        let prompts = Rc::new(Cell::new(0));
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            None,
            vec![PromptChoice::Deny], // the human still gets to decide
            prompts.clone(),
        );
        assert!(
            matches!(
                gate.ask(&[exec_request("bash")]),
                newt_core::PermissionDecision::Deny
            ),
            "a durable approve must not bypass the danger prompt for an interpreter"
        );
        assert_eq!(
            prompts.get(),
            1,
            "high-danger falls through to the human even with a durable approve"
        );
    }

    /// #904: `[P]ermanently deny` persists the `(kind, target)` to disk and, in a
    /// FRESH session that reloads it, auto-denies the same target WITHOUT ever
    /// prompting — the durable sibling of `[D]eny always`. Exercised on a net
    /// host (the motivating axis), but the mechanism is axis-agnostic.
    #[test]
    fn permanently_deny_persists_and_reloads_without_reprompting() {
        let dir = tempfile::TempDir::new().unwrap();
        let denials = dir.path().join("permission-denials.jsonl");
        let base = base_caveats("/ws");
        let net_req = newt_core::PermissionRequest {
            tool: "web_fetch".to_string(),
            kind: DenialKind::Net,
            target: "evil.example.com".to_string(),
            reason: "net does not permit 'evil.example.com'".to_string(),
        };

        // Session 1 — the human picks [P]ermanently deny.
        let mut state = PermissionPromptState::default();
        {
            let mut script = vec![PromptChoice::DenyPermanent].into_iter();
            let mut gate = PromptPermissionGate {
                state: &mut state,
                base: base.clone(),
                key_path: None,
                conversation_id: "conv-904".to_string(),
                log_path: None,
                denials_path: Some(denials.clone()),
                config_path: None,
                preset_clamp: None,
                danger: danger::DangerTable::builtin(),
                color: false,
                verbose: false,
                web_decision_timeout: Duration::from_secs(2),
                ask_human: move |_w: &PromptWindow, _p: &str| {
                    script.next().expect("script exhausted")
                },
            };
            assert!(matches!(
                gate.ask(std::slice::from_ref(&net_req)),
                newt_core::PermissionDecision::Deny
            ));
        }
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.decisions[0].decision, "deny");
        assert_eq!(state.decisions[0].scope, "permanent");
        assert_eq!(
            newt_core::load_denials(&denials),
            vec![(DenialKind::Net, "evil.example.com".to_string())],
            "the permanent deny was written to disk"
        );

        // Session 2 (fresh) — the denylist is loaded, so the SAME target is
        // denied WITHOUT prompting (the scripted human panics if consulted).
        let mut fresh = PermissionPromptState::with_persistent_denials(Some(&denials));
        {
            let mut gate = PromptPermissionGate {
                state: &mut fresh,
                base,
                key_path: None,
                conversation_id: "conv-904b".to_string(),
                log_path: None,
                denials_path: Some(denials.clone()),
                config_path: None,
                preset_clamp: None,
                danger: danger::DangerTable::builtin(),
                color: false,
                verbose: false,
                web_decision_timeout: Duration::from_secs(2),
                ask_human: |_w: &PromptWindow, _p: &str| {
                    panic!("must NOT prompt: target was permanently denied")
                },
            };
            assert!(matches!(
                gate.ask(std::slice::from_ref(&net_req)),
                newt_core::PermissionDecision::Deny
            ));
        }
        // No prompt ⇒ no new decision recorded in the fresh session.
        assert!(fresh.decisions.is_empty());
    }

    /// #904: the choice parser maps `P` to the permanent deny and leaves the
    /// existing keys (incl. the session `D`) intact; unknown/empty stays deny.
    #[test]
    fn parse_permission_choice_maps_permanent_deny() {
        assert_eq!(parse_permission_choice("P"), PromptChoice::DenyPermanent);
        assert_eq!(parse_permission_choice("D"), PromptChoice::DenyAlways);
        assert_eq!(parse_permission_choice("a"), PromptChoice::AllowOnce);
        assert_eq!(parse_permission_choice("s"), PromptChoice::AllowSession);
        // Case-significant + safe default: lowercase p / unknown / empty → deny.
        assert_eq!(parse_permission_choice("p"), PromptChoice::Deny);
        assert_eq!(parse_permission_choice(""), PromptChoice::Deny);
    }

    /// #904: the `[A]llow permanently` option is offered ONLY for net denials
    /// (the only axis with a per-target config allowlist); every axis still
    /// offers `[P]ermanently deny`.
    #[test]
    fn permanent_allow_offered_for_net_only() {
        let danger = danger::DangerTable::builtin();
        let net = permission_prompt_text(
            &PermissionRequest {
                tool: "web_fetch".to_string(),
                kind: DenialKind::Net,
                target: "github.com".to_string(),
                reason: String::new(),
            },
            &danger,
        );
        let exec = permission_prompt_text(&exec_request("npm"), &danger);
        assert!(
            net.contains("[A]llow permanently"),
            "net must offer it: {net}"
        );
        assert!(
            !exec.contains("[A]llow permanently"),
            "exec must NOT: {exec}"
        );
        assert!(net.contains("[P]ermanently deny") && exec.contains("[P]ermanently deny"));
    }

    /// #904: `[A]llow permanently` for a net host grants it this session AND
    /// durably appends it to `[tui.permissions] net` in the config, so a fresh
    /// session reads it as an ambient allow (net scope permits it → no denial →
    /// no prompt). The written config is valid TOML that round-trips.
    #[test]
    fn allow_permanently_grants_now_and_persists_host_to_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        // Start from a hand-authored config with a comment (proves preservation).
        std::fs::write(&config, "# my config\n[tui.permissions]\nnet = []\n").unwrap();
        let base = base_caveats("/ws");
        let net_req = newt_core::PermissionRequest {
            tool: "web_fetch".to_string(),
            kind: DenialKind::Net,
            target: "github.com".to_string(),
            reason: "net does not permit 'github.com'".to_string(),
        };

        let mut state = PermissionPromptState::default();
        {
            let mut script = vec![PromptChoice::AllowPermanent].into_iter();
            let mut gate = PromptPermissionGate {
                state: &mut state,
                base,
                key_path: None,
                conversation_id: "conv-904a".to_string(),
                log_path: None,
                denials_path: None,
                config_path: Some(config.clone()),
                preset_clamp: None,
                danger: danger::DangerTable::builtin(),
                color: false,
                verbose: false,
                web_decision_timeout: Duration::from_secs(2),
                ask_human: move |_w: &PromptWindow, _p: &str| {
                    script.next().expect("script exhausted")
                },
            };
            match gate.ask(std::slice::from_ref(&net_req)) {
                newt_core::PermissionDecision::Allow(c) => {
                    assert!(c.permits_net("github.com"), "granted this session");
                }
                newt_core::PermissionDecision::Deny => {
                    panic!("permanent-allow of a net host must be granted")
                }
            }
        }
        // Holds this session.
        assert!(state
            .session_grants
            .contains(&(DenialKind::Net, "github.com".to_string())));
        assert_eq!(state.decisions[0].scope, "permanent");
        // Durably written: the config parses back and its net scope permits it,
        // and the hand-authored comment survived (comment-preserving write).
        let written = std::fs::read_to_string(&config).unwrap();
        assert!(written.contains("# my config"), "comment lost: {written}");
        assert!(
            written.contains("github.com"),
            "host not persisted: {written}"
        );
        let reloaded = newt_core::Config::load(&config).unwrap();
        assert!(
            reloaded
                .tui
                .unwrap()
                .permissions
                .net
                .contains(&"github.com".to_string()),
            "a fresh session reads the durable net grant"
        );
    }

    /// Allow-once: the minted caveats cover the target for THIS consult, but
    /// nothing is remembered — the next identical ask prompts again.
    #[test]
    fn allow_once_grants_one_call_and_reprompts_next_time() {
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        let base = base_caveats("/ws");
        let mut gate = scripted_gate(
            &mut state,
            base.clone(),
            None,
            None,
            vec![PromptChoice::AllowOnce, PromptChoice::AllowOnce],
            prompts.clone(),
        );
        let req = [exec_request("npm")];
        match gate.ask(&req) {
            newt_core::PermissionDecision::Allow(c) => {
                assert!(c.permits_exec("npm"), "the grant covers the target");
                assert!(c.permits_exec("cargo"), "baseline grants kept");
                assert!(!c.permits_exec("rm"), "nothing else widened");
            }
            newt_core::PermissionDecision::Deny => panic!("expected allow"),
        }
        assert_eq!(prompts.get(), 1);
        // The same request again: allow-once left no session grant behind.
        assert!(matches!(
            gate.ask(&req),
            newt_core::PermissionDecision::Allow(_)
        ));
        assert_eq!(prompts.get(), 2, "allow-once re-prompts on the next call");
        drop(gate);
        assert!(state.session_grants.is_empty());
        assert_eq!(state.decisions.len(), 2);
        assert_eq!(state.decisions[0].decision, "allow");
        assert_eq!(state.decisions[0].scope, "once");
    }

    /// #1057: a model-driven `request_permissions` allow-once must cover the
    /// model's SEPARATE `run_command` retry — its widened caveats are otherwise
    /// discarded by `execute_request_permissions`, so the retry re-denied and
    /// wasted a round (the live Ornith repro: grant `python3`, then
    /// `/usr/bin/python3` re-prompts). It must also be ONE-SHOT.
    #[test]
    fn request_permissions_allow_once_carries_to_the_run_command_retry() {
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        let base = base_caveats("/ws");
        let mut gate = scripted_gate(
            &mut state,
            base,
            None,
            None,
            vec![PromptChoice::AllowOnce, PromptChoice::AllowOnce],
            prompts.clone(),
        );
        // The model pre-asks via request_permissions for `python3` (bare name).
        let ask = PermissionRequest {
            tool: "request_permissions".to_string(),
            kind: DenialKind::Exec,
            target: "python3".to_string(),
            reason: "need to run the tests".to_string(),
        };
        assert!(matches!(
            gate.ask(&[ask]),
            newt_core::PermissionDecision::Allow(_)
        ));
        assert_eq!(prompts.get(), 1);
        // The retry arrives as a run_command denial, path-resolved to /usr/bin/python3.
        match gate.ask(&[exec_request("/usr/bin/python3")]) {
            newt_core::PermissionDecision::Allow(c) => {
                assert!(
                    c.permits_exec("python3"),
                    "the carried grant widened the caveats so the retry runs"
                );
            }
            newt_core::PermissionDecision::Deny => panic!("carried grant should cover the retry"),
        }
        assert_eq!(
            prompts.get(),
            1,
            "no second prompt — the pending grant covered the /usr/bin/python3 retry"
        );
        // One-shot: another python3 op must re-prompt (allow-once preserved).
        assert!(matches!(
            gate.ask(&[exec_request("/usr/bin/python3")]),
            newt_core::PermissionDecision::Allow(_)
        ));
        assert_eq!(
            prompts.get(),
            2,
            "the one-shot pending grant was consumed; the next op re-prompts"
        );
    }

    /// #1057: a durable exec session grant matches by basename, so granting a
    /// bare `mytool` covers a later path-resolved `/opt/bin/mytool` — but a
    /// different program is still not covered (no over-grant).
    #[test]
    fn session_grant_exec_matches_by_basename() {
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            None,
            vec![PromptChoice::AllowSession, PromptChoice::AllowSession],
            prompts.clone(),
        );
        assert!(matches!(
            gate.ask(&[exec_request("mytool")]),
            newt_core::PermissionDecision::Allow(_)
        ));
        assert_eq!(prompts.get(), 1);
        // Same program, path-resolved: covered — no re-prompt.
        assert!(matches!(
            gate.ask(&[exec_request("/opt/bin/mytool")]),
            newt_core::PermissionDecision::Allow(_)
        ));
        assert_eq!(prompts.get(), 1, "basename covers the resolved path");
        // A DIFFERENT program still prompts.
        assert!(matches!(
            gate.ask(&[exec_request("othertool")]),
            newt_core::PermissionDecision::Allow(_)
        ));
        assert_eq!(prompts.get(), 2, "a different program is not covered");
    }

    /// #1057 pin-exact: granting a FULL PATH must NOT widen to a bare name (which
    /// could resolve to a different binary) — the basename relaxation is one-way,
    /// bare-grant → resolved-run, never full-grant → bare-run.
    #[test]
    fn full_path_session_grant_does_not_cover_a_bare_name() {
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            None,
            vec![PromptChoice::AllowSession, PromptChoice::AllowSession],
            prompts.clone(),
        );
        assert!(matches!(
            gate.ask(&[exec_request("/opt/bin/mytool")]),
            newt_core::PermissionDecision::Allow(_)
        ));
        assert_eq!(prompts.get(), 1);
        // A bare `mytool` is NOT covered by the full-path grant — re-prompts.
        assert!(matches!(
            gate.ask(&[exec_request("mytool")]),
            newt_core::PermissionDecision::Allow(_)
        ));
        assert_eq!(
            prompts.get(),
            2,
            "full-path grant must not widen to a bare name (pin-exact)"
        );
    }

    /// #1056 FLOOR: a readonly `/posture` projects no git-commit authority, so the
    /// gate REFUSES a git-write grant outright — even `[a]llow once` cannot pierce
    /// it, and it does not even prompt (the git-write capability is non-axis, so
    /// `mint`'s re-clamp can't attenuate it — this explicit check is the floor).
    #[test]
    fn git_write_grant_refused_under_readonly_preset() {
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        let clamp = newt_core::NamedPermissionPreset {
            readonly: true,
            ..Default::default()
        }
        .clamp();
        let base = base_caveats("/ws").meet(&clamp);
        let mut gate = scripted_gate_with_clamp(
            &mut state,
            base,
            None,
            None,
            clamp,
            vec![PromptChoice::AllowOnce],
            prompts.clone(),
        );
        let req = PermissionRequest {
            tool: "git".to_string(),
            kind: DenialKind::GitWrite,
            target: "commit".to_string(),
            reason: "commit the work".to_string(),
        };
        assert!(
            matches!(gate.ask(&[req]), newt_core::PermissionDecision::Deny),
            "a readonly preset must refuse a git-write grant"
        );
        assert_eq!(prompts.get(), 0, "the floor refuses WITHOUT prompting");
    }

    /// #1056: with NO preset active the floor doesn't bite — a git-write grant is
    /// allowed (allow-once), so a coder can commit in a normal session.
    #[test]
    fn git_write_grant_allowed_without_a_preset() {
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            None,
            vec![PromptChoice::AllowOnce],
            prompts.clone(),
        );
        let req = PermissionRequest {
            tool: "git".to_string(),
            kind: DenialKind::GitWrite,
            target: "commit".to_string(),
            reason: "commit the work".to_string(),
        };
        assert!(matches!(
            gate.ask(&[req]),
            newt_core::PermissionDecision::Allow(_)
        ));
        assert_eq!(prompts.get(), 1);
    }

    /// #307 FLOOR TEST (b) — the security contract: a session-grant CANNOT
    /// grant authority the active preset denies. The human answers "allow
    /// once" (and "allow session") for `rm`, but the readonly-triage preset
    /// clamps exec to none — so the minted caveats must NOT permit `rm`. The
    /// re-mint is re-`meet`-ed under the preset clamp (the load-bearing point
    /// in `mint`), so `widen_caveats` re-adding `rm` to the exec set is undone.
    #[test]
    fn session_grant_cannot_pierce_the_preset_floor() {
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        // A readonly-triage preset: exec denied entirely.
        let clamp = newt_core::NamedPermissionPreset {
            readonly: true,
            ..Default::default()
        }
        .clamp();
        // The effective (already-clamped) base the gate runs against.
        let base = base_caveats("/ws").meet(&clamp);
        assert!(
            !base.permits_exec("cargo"),
            "the preset clamped exec to none"
        );

        // Allow-once for `rm`: the human says yes, the preset says no.
        let mut gate = scripted_gate_with_clamp(
            &mut state,
            base.clone(),
            None,
            None,
            clamp.clone(),
            vec![PromptChoice::AllowOnce, PromptChoice::AllowSession],
            prompts.clone(),
        );
        match gate.ask(&[exec_request("rm")]) {
            newt_core::PermissionDecision::Allow(c) => {
                assert!(
                    !c.permits_exec("rm"),
                    "a once-grant must not pierce the preset floor: {c:?}"
                );
                assert!(!c.permits_exec("cargo"), "floor keeps exec denied");
            }
            newt_core::PermissionDecision::Deny => panic!("the gate allowed-once"),
        }
        // Now "allow session" for `rm`: the grant is remembered, but the next
        // re-mint is STILL re-clamped — the floor wins across the session.
        match gate.ask(&[exec_request("rm")]) {
            newt_core::PermissionDecision::Allow(c) => {
                assert!(
                    !c.permits_exec("rm"),
                    "a SESSION grant must not pierce the floor either: {c:?}"
                );
            }
            newt_core::PermissionDecision::Deny => panic!("the gate allowed-session"),
        }
        drop(gate);
        // The grant WAS remembered (the human's choice is recorded), but it is
        // powerless against the clamp — proving the floor, not the prompt, is
        // the authority ceiling.
        assert!(state
            .session_grants
            .contains(&(DenialKind::Exec, "rm".to_string())));
    }

    /// Session allow: one prompt, then every later ask for the same target
    /// is allowed silently — and a FRESH state (a new session) prompts anew.
    #[test]
    fn allow_session_never_reprompts_until_restart() {
        let prompts = Rc::new(Cell::new(0));
        let base = base_caveats("/ws");
        let mut state = PermissionPromptState::default();
        {
            let mut gate = scripted_gate(
                &mut state,
                base.clone(),
                None,
                None,
                vec![PromptChoice::AllowSession],
                prompts.clone(),
            );
            let req = [exec_request("npm")];
            assert!(matches!(
                gate.ask(&req),
                newt_core::PermissionDecision::Allow(_)
            ));
            assert_eq!(prompts.get(), 1);
            // Again within the same turn: no further prompt.
            assert!(matches!(
                gate.ask(&req),
                newt_core::PermissionDecision::Allow(_)
            ));
        }
        {
            // A NEW gate over the same session state (a later turn).
            let mut gate = scripted_gate(
                &mut state,
                base.clone(),
                None,
                None,
                vec![],
                prompts.clone(),
            );
            match gate.ask(&[exec_request("npm")]) {
                newt_core::PermissionDecision::Allow(c) => assert!(c.permits_exec("npm")),
                newt_core::PermissionDecision::Deny => panic!("session grant must hold"),
            }
        }
        assert_eq!(prompts.get(), 1, "exactly one prompt for the whole session");
        assert_eq!(state.decisions.len(), 1, "re-uses are not re-recorded");
        // "Restart": session state is gone — a fresh state prompts again.
        let mut fresh = PermissionPromptState::default();
        let mut gate = scripted_gate(
            &mut fresh,
            base,
            None,
            None,
            vec![PromptChoice::Deny],
            prompts.clone(),
        );
        assert!(matches!(
            gate.ask(&[exec_request("npm")]),
            newt_core::PermissionDecision::Deny
        ));
        assert_eq!(prompts.get(), 2, "the grant did not survive the restart");
    }

    /// Deny-always auto-denies later asks without prompting or re-recording.
    #[test]
    fn deny_always_short_circuits_later_asks() {
        let prompts = Rc::new(Cell::new(0));
        let mut state = PermissionPromptState::default();
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            None,
            vec![PromptChoice::DenyAlways],
            prompts.clone(),
        );
        let req = [exec_request("rm")];
        assert!(matches!(
            gate.ask(&req),
            newt_core::PermissionDecision::Deny
        ));
        assert!(matches!(
            gate.ask(&req),
            newt_core::PermissionDecision::Deny
        ));
        assert_eq!(prompts.get(), 1, "second ask auto-denied without a prompt");
        drop(gate);
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.decisions[0].decision, "deny");
        assert_eq!(state.decisions[0].scope, "session");
    }

    /// A batch (compound command): the first deny aborts — the whole call
    /// keeps the standard denial; an empty batch is a deny by construction.
    #[test]
    fn batch_deny_and_empty_requests_deny() {
        let prompts = Rc::new(Cell::new(0));
        let mut state = PermissionPromptState::default();
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            None,
            vec![PromptChoice::AllowOnce, PromptChoice::Deny],
            prompts.clone(),
        );
        let reqs = [exec_request("npm"), exec_request("rm")];
        assert!(matches!(
            gate.ask(&reqs),
            newt_core::PermissionDecision::Deny
        ));
        assert_eq!(prompts.get(), 2, "asked per target until the deny");
        assert!(matches!(gate.ask(&[]), newt_core::PermissionDecision::Deny));
        assert_eq!(prompts.get(), 2, "empty batch never prompts");
    }

    /// Every prompted decision lands in the JSONL record, keyed by the
    /// conversation id, with the issue's `(ts_claim, tool, kind, target,
    /// decision, scope)` shape.
    #[serial_test::serial(real_fs)]
    #[test]
    fn decisions_are_recorded_to_the_session_log() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = dir.path().join("permission-log.jsonl");
        let prompts = Rc::new(Cell::new(0));
        let mut state = PermissionPromptState::default();
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            Some(log.clone()),
            vec![
                PromptChoice::AllowOnce,
                PromptChoice::AllowSession,
                PromptChoice::Deny,
            ],
            prompts.clone(),
        );
        let _ = gate.ask(&[exec_request("npm")]);
        let _ = gate.ask(&[PermissionRequest {
            tool: "web_fetch".to_string(),
            kind: DenialKind::Net,
            target: "docs.rs".to_string(),
            reason: String::new(),
        }]);
        let _ = gate.ask(&[exec_request("rm")]);
        let body = std::fs::read_to_string(&log).unwrap();
        let records: Vec<newt_core::PermissionRecord> = body
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(records.len(), 3);
        assert!(records.iter().all(|r| r.conversation_id == "conv-test"));
        assert_eq!(
            (
                records[0].tool.as_str(),
                records[0].kind.as_str(),
                records[0].target.as_str()
            ),
            ("run_command", "exec", "npm")
        );
        assert_eq!(
            (records[0].decision.as_str(), records[0].scope.as_str()),
            ("allow", "once")
        );
        assert_eq!(
            (records[1].kind.as_str(), records[1].scope.as_str()),
            ("net", "session")
        );
        assert_eq!(
            (records[2].decision.as_str(), records[2].scope.as_str()),
            ("deny", "once")
        );
        // The in-session list (the `/permissions` view) matches the file.
        assert_eq!(state.decisions, records);
    }

    /// With a per-user key available, an allow re-mints THROUGH the signed
    /// identity path: the returned caveats are the enforced caveats of a
    /// fresh key rooted in the user key — and the baseline value the session
    /// enforces is untouched (attenuation-only, #263).
    #[serial_test::serial(real_fs)]
    #[test]
    fn allow_remints_from_the_user_root_and_never_widens_the_baseline() {
        let dir = tempfile::TempDir::new().unwrap();
        let key_path = dir.path().join("identity.pem");
        let prompts = Rc::new(Cell::new(0));
        let base = base_caveats("/ws");
        let mut state = PermissionPromptState::default();
        let mut gate = scripted_gate(
            &mut state,
            base.clone(),
            Some(key_path.clone()),
            None,
            vec![PromptChoice::AllowSession],
            prompts.clone(),
        );
        let minted = match gate.ask(&[exec_request("npm")]) {
            newt_core::PermissionDecision::Allow(c) => c,
            newt_core::PermissionDecision::Deny => panic!("expected allow"),
        };
        assert!(
            key_path.exists(),
            "the user root key was used for the re-mint"
        );
        assert!(minted.permits_exec("npm"));
        assert!(minted.permits_exec("cargo"));
        assert!(!minted.permits_exec("rm"));
        // The gate's baseline (the session's enforced caveats) is unchanged:
        // the grant lives in the minted value + session state only.
        drop(gate);
        assert_eq!(base, base_caveats("/ws"));
        // And the mint provably round-trips the identity layer: minting the
        // same policy from the same root yields the same enforced caveats.
        let policy = newt_core::widen_caveats(&base, &[(DenialKind::Exec, "npm".to_string())]);
        let key = mint_operating_key(&key_path, &policy).unwrap();
        assert_eq!(newt_identity::enforced_caveats(&key).unwrap(), minted);
    }

    /// The full TUI seam: execute_tool consults the gate on an fs_read
    /// denial. Allow-once reads the file exactly once and the next identical
    /// call re-prompts; the decisions are in the session state.
    #[serial_test::serial(real_fs)]
    #[tokio::test]
    async fn execute_tool_with_tui_gate_allow_once_then_reprompt() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("outside.txt"), "gated contents").unwrap();
        // fs_read scoped to a different root → reading in `ws` is denied.
        let caveats = base_caveats("/elsewhere");
        let prompts = Rc::new(Cell::new(0));
        let mut state = PermissionPromptState::default();
        let mut gate = scripted_gate(
            &mut state,
            caveats.clone(),
            None,
            None,
            vec![PromptChoice::AllowOnce, PromptChoice::Deny],
            prompts.clone(),
        );
        let args = serde_json::json!({"path": "outside.txt"});
        let out = newt_core::agentic::execute_tool(
            "read_file",
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut Mcp::empty(),
            None,
            None,
            None,
            None, // memory_source
            Some(&mut gate),
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None,
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert_eq!(out, "gated contents", "allow-once executed the real read");
        assert_eq!(prompts.get(), 1);
        // The identical call again: no session grant — prompts again, and
        // this scripted human now denies → the standard denial.
        let out = newt_core::agentic::execute_tool(
            "read_file",
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut Mcp::empty(),
            None,
            None,
            None,
            None, // memory_source
            Some(&mut gate),
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None,
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert!(
            out.starts_with("capability denied: fs_read does not permit 'outside.txt'"),
            "got: {out}"
        );
        // #721: the denial now also carries the model-actionable recovery path.
        assert!(out.contains("request_permissions"), "got: {out}");
        assert_eq!(prompts.get(), 2, "allow-once does not stick");
        drop(gate);
        assert_eq!(state.decisions.len(), 2);
    }

    /// The full TUI seam, session scope: one prompt, then the same denial
    /// auto-allows for the rest of the session (across gate rebuilds, i.e.
    /// turns).
    #[serial_test::serial(real_fs)]
    #[tokio::test]
    async fn execute_tool_with_tui_gate_session_allow_holds_across_turns() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("outside.txt"), "gated contents").unwrap();
        let caveats = base_caveats("/elsewhere");
        let prompts = Rc::new(Cell::new(0));
        let mut state = PermissionPromptState::default();
        let args = serde_json::json!({"path": "outside.txt"});
        for _turn in 0..2 {
            // A fresh gate per turn over the SAME session state — exactly
            // how the TUI loop builds it.
            let mut gate = scripted_gate(
                &mut state,
                caveats.clone(),
                None,
                None,
                vec![PromptChoice::AllowSession],
                prompts.clone(),
            );
            let out = newt_core::agentic::execute_tool(
                "read_file",
                &args,
                &ws.path().to_string_lossy(),
                false,
                20,
                &caveats,
                &mut Mcp::empty(),
                None,
                None,
                None,
                None, // memory_source
                Some(&mut gate),
                None,
                None, // git_tool
                None, // crew_runner
                None, // scratchpad_store
                None,
                None, // code_search
                None, // experience_store
                None, // step_ledger
            )
            .await;
            assert_eq!(out, "gated contents");
        }
        assert_eq!(prompts.get(), 1, "one prompt for the whole session");
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.decisions[0].scope, "session");
    }

    #[test]
    fn prompting_configured_from_flag_or_config_off_by_default() {
        // Neither flag nor config: OFF — zero behavior change.
        assert!(!permission_prompting_configured(false, None));
        let mut tui = newt_core::TuiConfig::default();
        assert!(!permission_prompting_configured(false, Some(&tui)));
        // CLI flag (env) alone, config alone, or both.
        assert!(permission_prompting_configured(true, None));
        tui.permissions.prompt = true;
        assert!(permission_prompting_configured(false, Some(&tui)));
        assert!(permission_prompting_configured(true, Some(&tui)));
    }

    #[test]
    fn should_prompt_permissions_defaults_on_interactive_and_off_headless() {
        // #721: the new default — an interactive human prompts even with NOTHING
        // configured (the dead-end denial used to be the only outcome).
        assert!(should_prompt_permissions(false, false, true, false));
        // Explicitly configured ON, interactive: still ON.
        assert!(should_prompt_permissions(true, false, true, false));

        // Headless / eval / ACP NEVER prompt — the default-deny invariant —
        // even when explicitly configured on. (A prompt no one can answer hangs.)
        assert!(!should_prompt_permissions(true, false, true, true));
        // Non-TTY (piped / captured) is likewise default-deny.
        assert!(!should_prompt_permissions(true, false, false, false));
        assert!(!should_prompt_permissions(false, false, false, false));

        // Explicit OFF beats the interactive default AND an explicit ON.
        assert!(!should_prompt_permissions(false, true, true, false));
        assert!(!should_prompt_permissions(true, true, true, false));
    }

    /// §6.10 — **the default-deny invariant is not weakened by the arbiter.**
    ///
    /// A session that cannot answer a TTY prompt must reach a denial without
    /// ever asking. Now that asking means constructing a `PromptWindow` — which
    /// takes stdin and suspends every ephemeral writer — "without ever asking"
    /// has a mechanical witness: the process-wide construction counter must not
    /// move.
    ///
    /// The guard is `if headless || !interactive { return false; }`, and it must
    /// short-circuit BEFORE anything consults the terminal. That it does is
    /// visible in the predicate's purity (it takes four booleans and probes
    /// nothing), and pinned here by exhausting the whole product: no combination
    /// of the two ON signals can make a headless or non-interactive session
    /// prompt.
    #[serial_test::serial(prompt_stdin)]
    #[test]
    fn headless_and_piped_sessions_never_construct_a_prompt_window() {
        let before = newt_core::tty::prompt_windows_constructed();

        for configured_on in [false, true] {
            for explicit_off in [false, true] {
                // HEADLESS: never prompts, whatever else is set.
                for interactive in [false, true] {
                    assert!(
                        !should_prompt_permissions(configured_on, explicit_off, interactive, true),
                        "headless prompted (configured_on={configured_on} \
                         explicit_off={explicit_off} interactive={interactive})"
                    );
                }
                // NON-INTERACTIVE (piped / captured): likewise never prompts.
                assert!(
                    !should_prompt_permissions(configured_on, explicit_off, false, false),
                    "a non-interactive session prompted (configured_on={configured_on} \
                     explicit_off={explicit_off})"
                );
            }
        }

        assert_eq!(
            newt_core::tty::prompt_windows_constructed(),
            before,
            "a default-denied session must reach its denial without the terminal \
             ever being suspended for a question"
        );
    }

    #[test]
    fn permissions_command_lists_decisions_and_log_location() {
        let mut state = PermissionPromptState::default();
        // Disabled + empty: says how to enable, says there's nothing yet.
        // No active posture ⇒ no preset line; behavior is the pre-#307 listing.
        let lines = permissions_command_lines(&state, false, None, None);
        assert!(lines[0].contains("OFF"), "got: {lines:?}");
        assert!(lines
            .iter()
            .any(|l| l.contains("no prompted permission decisions")));
        // With decisions + a log path: one row per decision, log named,
        // promotion stays a human config edit.
        state.decisions.push(newt_core::PermissionRecord::new(
            "conv-1",
            "run_command",
            DenialKind::Exec,
            "npm",
            "allow",
            "session",
        ));
        let log = std::path::PathBuf::from("/home/u/.newt/permission-log.jsonl");
        let lines = permissions_command_lines(&state, true, Some(&log), None);
        assert!(lines
            .iter()
            .any(|l| l.contains("exec:npm") && l.contains("run_command")));
        assert!(lines.iter().any(|l| l.contains("permission-log.jsonl")));
        assert!(lines.iter().any(|l| l.contains("never authority")));
        assert!(!lines[0].contains("OFF"));
    }

    /// #307: an active posture is reflected at the top of `/permissions`, even
    /// with prompting OFF — the clamp in force is always visible.
    #[test]
    fn permissions_command_reflects_the_active_posture() {
        let state = PermissionPromptState::default();
        let preset = newt_core::NamedPermissionPreset {
            // fs_read: None preserves pre-#755 behavior (reads unrestricted).
            fs_read: None,
            readonly: true,
            exec_allow: vec!["git".to_string()],
            deny: vec!["*".to_string()],
            max_calls: Some(40),
        };
        let posture = ActivePosture {
            name: "triage".to_string(),
            preset_name: "readonly-triage".to_string(),
            clamp: preset.clamp(),
            clamp_summary: preset.summary(),
            skill_body: None,
            framing: None,
        };
        let lines = permissions_command_lines(&state, false, None, Some(&posture));
        assert!(
            lines[0].contains("active permission posture: triage")
                && lines[0].contains("readonly-triage")
                && lines[0].contains("readonly"),
            "got: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("WINS over --disable-ocap")),
            "the floor property is surfaced: {lines:?}"
        );
    }

    #[test]
    fn help_lists_the_permissions_command() {
        assert!(help_lines().iter().any(|l| l.contains("/permissions")));
    }

    #[test]
    fn help_lists_the_mode_and_posture_commands() {
        assert!(help_lines().iter().any(|l| l.contains("/mode")));
        assert!(help_lines().iter().any(|l| l.contains("/posture")));
    }

    #[test]
    fn help_lists_the_start_and_rename_commands() {
        // #1030 lifecycle verbs must be discoverable in /help.
        assert!(help_lines().iter().any(|l| l.contains("/start")));
        assert!(help_lines().iter().any(|l| l.contains("/rename")));
    }

    #[test]
    fn close_out_message_reflects_the_rotation_kind() {
        // Persisted outgoing: /new is bare; /start says stays-open; the finalizers
        // point at /resume (no more "won't resume next launch").
        assert_eq!(close_out_message("new", "NEW", true), "NEW");
        assert!(close_out_message("start", "NEW", true).contains("stays open"));
        assert!(close_out_message("start", "NEW", true).contains("/resume"));
        // #1165: /end LEADS with the ending, never "Started a new conversation".
        let end = close_out_message("end", "NEW", true);
        assert!(end.starts_with("Conversation ended"), "{end}");
        assert!(end.contains("/resume to reopen"), "{end}");
        assert!(
            !end.starts_with("NEW"),
            "end must not headline the new conversation: {end}"
        );
        assert!(close_out_message("restart", "NEW", true).contains("/resume to reopen"));
        // Nothing persisted (empty conversation or ephemeral session): no
        // resume promise — the plain new-conversation line for start/new/
        // restart, but /end STILL leads with the ending (#1170 UAT gap).
        assert_eq!(close_out_message("start", "NEW", false), "NEW");
        let end_empty = close_out_message("end", "NEW", false);
        assert!(end_empty.starts_with("Conversation ended"), "{end_empty}");
        assert!(
            !end_empty.contains("/resume"),
            "nothing to reopen: {end_empty}"
        );
    }
}
