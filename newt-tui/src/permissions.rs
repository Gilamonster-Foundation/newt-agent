//! Interactive OCAP decisions. One typed [`newt_core::Question`] supplies both
//! terminal and web rendering, parsing, and the set of actions that may pass.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::danger;
use crate::mint_operating_key;
use newt_core::agentic::{newt_line, print_newt};
use newt_core::interaction_adapter::{definition_to_question, role_of, DECISION_CONTROL};
use newt_core::tty::{
    modal_prompt_controls, read_prompt_window_line, ControlReader, PromptLine as ModalLine,
    PromptWindow, Terminal, MODAL_CONTROL_HINT, MODAL_INPUT_GLYPH,
};
pub(crate) use newt_core::PermissionAction as PromptChoice;
use newt_core::{HumanQuestionOutcome, Question};
use newt_interaction::{
    Audience, ChoiceOption, Control, ControlId, ControlKind, InteractionDefinition,
    InteractionKind, OptionId, Requirement,
};

// ---------------------------------------------------------------------------

/// Whether prompted permissions were explicitly enabled for this session.
pub(crate) fn permission_prompting_configured(
    env_flag: bool,
    tui: Option<&newt_core::TuiConfig>,
) -> bool {
    env_flag || tui.is_some_and(|t| t.permissions.prompt)
}

const INTERACTIVE_PROMPT_DEFAULT: bool = true;

/// Headless/non-TTY and explicit-off are fail-closed; interactive defaults on.
pub(crate) fn should_prompt_permissions(
    configured_on: bool,
    explicit_off: bool,
    interactive: bool,
    headless: bool,
) -> bool {
    if headless || !interactive {
        return false;
    }
    if explicit_off {
        return false;
    }
    configured_on || INTERACTIVE_PROMPT_DEFAULT
}
#[cfg(unix)]
pub(crate) use newt_core::tty::try_watch_stdin;

pub(crate) fn reason_is_model_authored(req: &newt_core::PermissionRequest) -> bool {
    req.tool == "request_permissions"
}

/// How long the gate waits for a web decision before failing closed.
///
/// **Load bearing, and until B0b-1 untested** (#1842): this must be
/// SHORTER than `ConversationStore::PERMISSION_REQUEST_TTL_NANOS`, or the
/// gate could still be waiting on an offer the store has already aged out
/// — and a decision could land against a row nothing will honour. The two
/// numbers used to live in different crates with nothing relating them
/// (a literal here, a constant there);
/// `b0b::the_gate_timeout_is_shorter_than_the_store_ttl` now asserts the
/// relationship instead of leaving it to coincidence.
pub(crate) const WEB_DECISION_TIMEOUT: Duration = Duration::from_secs(4 * 60);

/// One action this policy offers, already decided.
///
/// The POLICY half hands these to the MARSHALLING half. The split is what
/// lets tier and audience be consulted TOGETHER — they decide the action
/// list *and* the note text in the same branches, exactly as before — while
/// the construction of protocol records stays mechanical.
struct OfferedAction {
    action: PromptChoice,
    key: &'static str,
    label: &'static str,
}

/// **The POLICY half** (B0a, #1841): which actions this `(tier, audience)`
/// pair offers, and the note that accompanies them.
///
/// This is a RESTRUCTURING of the old `permission_question_for`, not a
/// relocation: the high-tier arms produce genuinely different per-surface
/// text, and the durable grants are terminal-only, so tier and audience
/// must still be known simultaneously.
fn permission_policy(
    req: &newt_core::PermissionRequest,
    danger: &danger::DangerTable,
    audience: Audience,
) -> (Vec<OfferedAction>, Option<String>) {
    use newt_core::DenialKind;
    let tier = danger.classify(req.kind, &req.target);
    // ONE audience test, bound once. The A0 shape asked it twice; the two
    // asks were always the same question, so the ratchet baseline goes
    // DOWN by one branch rather than up.
    let terminal = matches!(audience, Audience::Terminal);

    let mut actions = vec![OfferedAction {
        action: PromptChoice::AllowOnce,
        key: "a",
        label: "allow once",
    }];
    if tier == danger::DangerTier::Low {
        actions.push(OfferedAction {
            action: PromptChoice::AllowSession,
            key: "s",
            label: "session allow",
        });
        if req.kind == DenialKind::Net && terminal {
            actions.push(OfferedAction {
                action: PromptChoice::AllowPermanent,
                key: "A",
                label: "Allow permanently (adds host to config)",
            });
        }
    }
    actions.push(OfferedAction {
        action: PromptChoice::Deny,
        key: "d",
        label: "deny (default)",
    });
    if terminal {
        actions.push(OfferedAction {
            action: PromptChoice::DenyAlways,
            key: "D",
            label: "Deny always",
        });
        actions.push(OfferedAction {
            action: PromptChoice::DenyPermanent,
            key: "P",
            label: "Permanently deny",
        });
    }

    // `Audience` is `#[non_exhaustive]`, so this crate cannot match it
    // exhaustively and the wildcard is forced by the language rather than
    // chosen. The tripwire that a third variant needs thinking about lives
    // where the enum is DEFINED —
    // `newt_interaction::binding::model_fidelity` (#1837).
    let note = match (tier, audience) {
        (danger::DangerTier::High, Audience::Terminal) => Some(format!(
            "high-danger: session allow refused; key allow / step-up is the future path, P3\n{MODAL_CONTROL_HINT}"
        )),
        (danger::DangerTier::High, Audience::Web) => Some(format!(
            "High danger: session authorization is unavailable.\n{MODAL_CONTROL_HINT}"
        )),
        _ => Some(MODAL_CONTROL_HINT.into()),
    };
    (actions, note)
}

/// **The MARSHALLING half** (B0a, #1841): the policy's decisions as ONE
/// [`InteractionDefinition`].
///
/// This is the single form both surfaces build. The prompt text itself is
/// audience-independent; only the action list and the note are not.
pub(crate) fn permission_definition(
    req: &newt_core::PermissionRequest,
    danger: &danger::DangerTable,
    audience: Audience,
) -> InteractionDefinition {
    use newt_core::DenialKind;
    let (verb, axis) = match req.kind {
        DenialKind::Exec => ("run", "outside the granted exec allowlist"),
        DenialKind::FsRead => ("read", "outside the granted fs_read scope"),
        DenialKind::FsWrite => ("write", "outside the granted fs_write scope"),
        DenialKind::Net => ("reach", "outside the granted net allowlist"),
        DenialKind::RemoteTool => ("call", "not in the active persona's tool allow-list"),
        DenialKind::GitWrite => (
            "commit/stage via git",
            "outside the granted git-write authority",
        ),
    };

    let blast = match danger.blast_radius(req.kind, &req.target) {
        Some(line) => format!("{line}\n"),
        None => String::new(),
    };
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

    let (offered, note) = permission_policy(req, danger, audience);
    let options = offered
        .into_iter()
        .map(|offer| ChoiceOption {
            // The WIRE name is the option's identity; the key is a
            // presentation affordance. Both survive, which is what makes
            // the adapter round trip field-identical.
            id: OptionId::new(offer.action.as_str()).expect(WIRE_NAMES_ARE_OPTION_IDS),
            role: role_of(offer.action),
            label: offer.label.to_string(),
            key: offer.key.to_string(),
            aliases: Vec::new(),
        })
        .collect();

    let mut definition = InteractionDefinition::new(
        InteractionKind::Choice,
        format!(
            "\u{2298} {} wants to {verb} `{}` \u{2014} {axis}.\n{blast}{reason}",
            req.tool, req.target
        )
        .trim_end(),
        vec![Control {
            id: ControlId::new(DECISION_CONTROL).expect(WIRE_NAMES_ARE_OPTION_IDS),
            kind: ControlKind::Choice { options },
            label: String::new(),
            // A permission prompt must be answered: an unanswered one
            // denies by default, which is a decision, not an absence.
            requirement: Requirement::Required,
        }],
    );
    definition.note = note;
    definition
}

/// Why the `expect`s above cannot fire.
///
/// Every `PermissionAction` wire name, and `DECISION_CONTROL`, is drawn
/// from `[A-Za-z0-9_-]`, and that set is frozen.
/// `b0a::every_offered_action_is_a_valid_option_id` walks every
/// `(DenialKind, tier, audience)` combination this policy can produce and
/// asserts the definition builds, so this branch is unreachable rather
/// than merely unlikely.
const WIRE_NAMES_ARE_OPTION_IDS: &str =
    "every PermissionAction wire name is a valid option id (frozen set; see \
     b0a::every_offered_action_is_a_valid_option_id)";

/// Render the definition as the legacy typed form, **through the A2.2
/// adapter**.
///
/// `terminal_text()` lives on [`Question`] and has no counterpart in
/// `newt-interaction`, which is pure data — extracting rendering is C0's
/// slice, not this one. So the adapter is a PERMANENT production
/// dependency from here on, and byte preservation holds by construction:
/// `adapter::a_question_round_trips_through_the_definition_byte_for_byte`
/// proves the mapping is field-identical, and `terminal_text()` is a pure
/// function of those fields.
///
/// Hand-writing a second renderer over `InteractionDefinition` to avoid
/// this call would be a new duplicate string builder tracked by no
/// baseline — the exact sprawl the epic exists to delete.
fn question_for(
    req: &newt_core::PermissionRequest,
    danger: &danger::DangerTable,
    audience: Audience,
) -> Question<PromptChoice> {
    definition_to_question(&permission_definition(req, danger, audience))
        .expect(WIRE_NAMES_ARE_OPTION_IDS)
}

/// Build the one typed form consumed by terminal and HTMX renderers.
pub(crate) fn permission_question(
    req: &newt_core::PermissionRequest,
    danger: &danger::DangerTable,
) -> Question<PromptChoice> {
    question_for(req, danger, Audience::Terminal)
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

/// Read a terminal form. Unknown input and I/O failure fail closed.
pub(crate) fn prompt_permission_choice(
    w: &PromptWindow,
    question: &Question<PromptChoice>,
) -> PromptChoice {
    let prompt = format!("{}\n{MODAL_INPUT_GLYPH}", question.terminal_text());
    match read_prompt_window_line(w, &prompt) {
        Ok(ModalLine::Line(answer)) => question.parse(&answer).unwrap_or(PromptChoice::Deny),
        Ok(ModalLine::Back) => PromptChoice::Back,
        Ok(ModalLine::Exit) => PromptChoice::Exit,
        Ok(ModalLine::Eof) | Err(_) => PromptChoice::Deny,
    }
}

fn decision_scope(choice: PromptChoice) -> &'static str {
    match choice {
        PromptChoice::AllowOnce => "once",
        PromptChoice::AllowSession => "session",
        PromptChoice::AllowPermanent => "permanent",
        PromptChoice::Deny => "once",
        PromptChoice::DenyAlways => "session",
        PromptChoice::DenyPermanent => "permanent",
        PromptChoice::Back | PromptChoice::Exit => "control",
    }
}

fn verdict_scope(verdict: newt_core::store::Verdict) -> &'static str {
    match verdict {
        newt_core::store::Verdict::AllowOnce => "once",
        newt_core::store::Verdict::AllowSession => "session",
        newt_core::store::Verdict::Deny => "once",
    }
}

pub(crate) fn prompt_user_input(w: &PromptWindow, question: &str) -> io::Result<ModalLine> {
    let form = Question::<PromptChoice> {
        markdown: format!("? {question}"),
        actions: Vec::new(),
        note: Some(MODAL_CONTROL_HINT.into()),
    };
    let result =
        read_prompt_window_line(w, &format!("{}\n{MODAL_INPUT_GLYPH}", form.terminal_text()))?;
    let ModalLine::Line(line) = &result else {
        return Ok(result);
    };
    // The answer is returned VERBATIM — leading/trailing whitespace can be
    // meaningful (an indented code line, a spacing-sensitive value, an
    // intentionally blank-but-submitted answer). Slash-command detection reads a
    // trim_start VIEW below without mutating the answer.
    if is_slash_command_at_prompt(line) {
        w.notice(
            "(slash commands aren't answers; press Esc, then use the command at the chat prompt)",
        )
        .ok();
        return Ok(ModalLine::Back);
    }
    Ok(result)
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
        // A whitespace-padded non-slash answer is NOT a command, so
        // `prompt_user_input` returns it verbatim (whitespace preserved) rather
        // than backing out. Detection reads a trim_start view; it never trims the
        // answer itself.
        assert!(!is_slash_command_at_prompt("  indented answer  "));
        assert!(!is_slash_command_at_prompt("   "));
    }
}

/// Session decisions remain separate from the never-widened operating key.
#[derive(Default)]
pub(crate) struct PermissionPromptState {
    /// Opt-in attach-surface decision channel; `None` uses the terminal.
    pub(crate) web_store: Option<newt_core::ConversationStore>,
    /// The workspace fence this session is confined to (B0b-1, #1842).
    ///
    /// Supplied by the CALLER and compared against the fence the offer
    /// carries, so `Refusal::WorkspaceMismatch` is a check that can fail
    /// rather than a tautology. Empty in tests that do not exercise the
    /// fence, which is why `authorize` compares two independently
    /// supplied values instead of one value with itself.
    pub(crate) workspace_key: String,
    session_grants: std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    session_denials: std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    /// Durable deny-only entries loaded at session start.
    persistent_denials: std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    /// One-shot grants carried from `request_permissions` to its retry.
    pending_once_grants: std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    pub(crate) decisions: Vec<newt_core::PermissionRecord>,
    /// Durable approve/deny policy, read-only during the session.
    pub(crate) ocap_policy: newt_core::ocap_store::PolicySet,
}

/// Normalize only prompt memos; the authority check itself stays exact.
pub(crate) fn exec_grant_basename(cmd: &str) -> &str {
    cmd.rsplit(['/', '\\'])
        .find(|p| !p.is_empty())
        .unwrap_or(cmd)
}

/// Match exact grants, plus a basename-normalized exec request.
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

/// Consume an exact or basename-normalized one-shot grant.
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

/// Prompts, records, and re-mints from the user root without widening the live key.
pub(crate) struct PromptPermissionGate<
    'a,
    F: FnMut(&PromptWindow, &Question<PromptChoice>) -> PromptChoice,
> {
    pub(crate) state: &'a mut PermissionPromptState,
    /// Enforced caveats at turn start.
    pub(crate) base: newt_core::Caveats,
    pub(crate) key_path: Option<std::path::PathBuf>,
    pub(crate) conversation_id: String,
    pub(crate) log_path: Option<std::path::PathBuf>,
    pub(crate) denials_path: Option<std::path::PathBuf>,
    pub(crate) config_path: Option<std::path::PathBuf>,
    /// Re-applied after widening so grants cannot pierce a named preset.
    pub(crate) preset_clamp: Option<newt_core::Caveats>,
    pub(crate) danger: danger::DangerTable,
    pub(crate) color: bool,
    pub(crate) verbose: bool,
    /// Whether interactive AUTHORIZATION prompting is enabled this session. This
    /// is ORTHOGONAL to whether a human is present: the gate is built whenever
    /// the session has a usable TTY, so `ask_question` (the human-question seam)
    /// stays available even when this is false. When false, [`Self::ask`] denies
    /// every request immediately WITHOUT opening a prompt — disabling permission
    /// prompts must never erase the operator from `request_user_input`.
    pub(crate) authorization_prompts_enabled: bool,
    pub(crate) web_decision_timeout: Duration,
    /// Nested-form controls feed the same turn cancellation path as the watcher.
    pub(crate) cancel: Option<&'a AtomicBool>,
    pub(crate) exit: Option<&'a AtomicBool>,
    pub(crate) ask_human: F,
}

impl<F: FnMut(&PromptWindow, &Question<PromptChoice>) -> PromptChoice> PromptPermissionGate<'_, F> {
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

    /// Re-mint baseline plus grants from the root; never widen the live key.
    fn mint(&self, once_grants: &[(newt_core::DenialKind, String)]) -> newt_core::Caveats {
        let mut grants: Vec<(newt_core::DenialKind, String)> =
            self.state.session_grants.iter().cloned().collect();
        grants.extend(once_grants.iter().cloned());
        let mut policy = newt_core::widen_caveats(&self.base, &grants);
        // Re-clamping is load-bearing: widening may repopulate an emptied scope.
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

    /// Publish the same typed form the web renders and poll for its answer.
    fn await_web_decision(
        &self,
        store: &newt_core::ConversationStore,
        w: &newt_core::tty::PromptWindow,
        req: &newt_core::PermissionRequest,
    ) -> (PromptChoice, &'static str) {
        let question = question_for(req, &self.danger, Audience::Web);
        let tier = if self.danger.classify(req.kind, &req.target) == danger::DangerTier::High {
            "\"high\""
        } else {
            "\"low\""
        };
        let request_id =
            match store.publish_permission_question(&self.conversation_id, &question, tier) {
                Ok(id) => id,
                Err(_) => return (PromptChoice::Deny, "web-unavailable"),
            };
        let note = question
            .note
            .as_deref()
            .map_or(MODAL_CONTROL_HINT.to_string(), |note| {
                if note.contains(MODAL_CONTROL_HINT) {
                    note.to_string()
                } else {
                    format!("{note}\n{MODAL_CONTROL_HINT}")
                }
            });
        w.notice(&newt_line(
            &format!(
                "awaiting a decision from the web for `{}`…\n{note}",
                req.target
            ),
            self.color,
            self.verbose,
        ))
        .ok();
        // Delegate the wait loop to the injectable core so reader-recovery is
        // unit-testable with a scripted reader, a fake clock, and a no-op sleep.
        self.run_web_wait(
            store,
            &request_id,
            w,
            move || modal_prompt_controls(w).map(|r| Box::new(r) as Box<dyn ControlReader + '_>),
            Instant::now,
            std::thread::sleep,
        )
    }

    /// The web-decision wait loop with an INJECTABLE control-reader lifecycle.
    ///
    /// A transient terminal read error must never permanently disable local
    /// controls (the defect this replaces set `controls = None` forever after a
    /// single error). Here the reader runs a small state machine:
    /// `Live` → poll it; an [`io::ErrorKind::Interrupted`] error is transient and
    /// retries the SAME reader; any other error drops it and moves to `Retrying`
    /// (emitting at most one concise warning), which re-arms via `reacquire` at a
    /// bounded `reacquire_backoff` cadence — never busy-spinning. The gate is
    /// only built for an interactive session, so ANY acquisition failure —
    /// including `Unsupported` (a terminal-loss race between session setup and
    /// the first prompt) — also enters paced `Retrying`, never a permanent dead
    /// end; there is no "genuinely headless" path here to latch off.
    ///
    /// Through every reader state the web store poll and the fail-closed
    /// `deadline` keep running, and a recovered `Back`/`Exit` still resolves
    /// through [`Self::web_abort_choice`]'s exactly-once CAS — so a local abort
    /// and a web verdict can never both win. `reacquire`/`now`/`sleep` are
    /// injected so tests drive the loop deterministically without a real
    /// terminal or wall clock.
    fn run_web_wait<'w>(
        &self,
        store: &newt_core::ConversationStore,
        request_id: &str,
        w: &'w newt_core::tty::PromptWindow,
        mut reacquire: impl FnMut() -> io::Result<Box<dyn ControlReader + 'w>>,
        now: impl Fn() -> Instant,
        mut sleep: impl FnMut(Duration),
    ) -> (PromptChoice, &'static str) {
        enum ReaderState<'a> {
            Live(Box<dyn ControlReader + 'a>),
            Retrying { next_attempt: Instant },
        }

        let control_poll_timeout = Duration::from_millis(200);
        let reacquire_backoff = Duration::from_millis(200);
        let deadline = now() + self.web_decision_timeout;

        let mut warned = false;
        let mut state: ReaderState<'w> = match reacquire() {
            Ok(reader) => ReaderState::Live(reader),
            // ANY acquisition failure — including `Unsupported`, which here is a
            // terminal-loss race between the interactive session's setup and the
            // first prompt, NOT a genuinely headless session — enters paced retry
            // rather than permanently disabling the local controls. Warn once.
            Err(_) => {
                warned = true;
                self.notice_control_warning(w);
                ReaderState::Retrying {
                    next_attempt: now() + reacquire_backoff,
                }
            }
        };

        loop {
            // Advance the reader. `blocked` = we spent ~one poll timeout blocking,
            // so we should NOT also sleep this tick.
            let poll_result = match &mut state {
                ReaderState::Live(reader) => Some(reader.poll(control_poll_timeout)),
                _ => None,
            };
            let mut blocked = false;
            match poll_result {
                Some(Ok(Some(ModalLine::Back))) => {
                    return self
                        .web_abort_choice(
                            store,
                            &self.conversation_id,
                            request_id,
                            PromptChoice::Back,
                        )
                        .unwrap_or((PromptChoice::Back, "web-aborted"));
                }
                Some(Ok(Some(ModalLine::Exit))) => {
                    return self
                        .web_abort_choice(
                            store,
                            &self.conversation_id,
                            request_id,
                            PromptChoice::Exit,
                        )
                        .unwrap_or((PromptChoice::Exit, "web-aborted"));
                }
                // A typed line or EOF at a web prompt is ignored; we polled.
                Some(Ok(_)) => blocked = true,
                Some(Err(e)) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        // Transient (EINTR): keep the SAME reader and retry. This
                        // error returns IMMEDIATELY (it does not consume the poll
                        // timeout), so leave `blocked` false and let the paced
                        // sleep below run — otherwise repeated EINTR busy-spins.
                    } else {
                        // Broken reader: drop it, re-arm at a bounded cadence.
                        if !warned {
                            warned = true;
                            self.notice_control_warning(w);
                        }
                        state = ReaderState::Retrying {
                            next_attempt: now() + reacquire_backoff,
                        };
                    }
                }
                None => {
                    // Not live: try to re-arm when the backoff elapses. This gate
                    // was built for an interactive session, so a reader that once
                    // worked may return again (detach/reattach, PTY swap) — an
                    // `Unsupported` reacquire here is NOT terminal; keep retrying
                    // (bounded) until the deadline. There is no permanent
                    // "no terminal" state to latch: one reader-state transition
                    // must never permanently remove the escape hatch.
                    if let ReaderState::Retrying { next_attempt } = &mut state {
                        if now() >= *next_attempt {
                            match reacquire() {
                                Ok(reader) => state = ReaderState::Live(reader),
                                Err(_) => *next_attempt = now() + reacquire_backoff,
                            }
                        }
                    }
                }
            }

            match store.take_permission_decision(&self.conversation_id, request_id) {
                Ok(Some(verdict)) => return (verdict.into(), verdict_scope(verdict)),
                Ok(None) if now() >= deadline => {
                    // Resolve through the same CAS as a TTY answer. If a web
                    // answer won the race, consume that verdict; otherwise
                    // the timeout is a fail-closed denial.
                    return match store.resolve_permission_request(
                        &self.conversation_id,
                        request_id,
                        "expired",
                    ) {
                        Ok(true) => (PromptChoice::Deny, "web-timeout"),
                        Ok(false) => match store
                            .take_permission_decision(&self.conversation_id, request_id)
                        {
                            Ok(Some(verdict)) => (verdict.into(), verdict_scope(verdict)),
                            Ok(None) | Err(_) => (PromptChoice::Deny, "web-timeout"),
                        },
                        Err(_) => (PromptChoice::Deny, "web-timeout"),
                    };
                }
                Ok(None) => {}
                Err(_) => return (PromptChoice::Deny, "web-store-error"),
            }

            if !blocked {
                let remaining = deadline.saturating_duration_since(now());
                sleep(remaining.min(Duration::from_millis(200)));
            }
        }
    }

    /// Emit the single, concise "controls interrupted, retrying" warning used by
    /// [`Self::run_web_wait`] when a live control reader breaks mid-wait.
    fn notice_control_warning(&self, w: &newt_core::tty::PromptWindow) {
        w.notice(&newt_line(
            "terminal controls temporarily unavailable — retrying; the web decision and timeout still apply",
            self.color,
            self.verbose,
        ))
        .ok();
    }

    /// **The accept/deny decision** (B0b-1, #1842).
    ///
    /// `Question::parse` has already DECODED the operator's keystroke into
    /// a candidate action — aliases, ambiguity denial, case-distinct keys.
    /// It no longer authorizes. This does, through
    /// `newt_interaction::validate_response`, against the definition the
    /// form was rendered from and a registry of executable actions the
    /// definition cannot influence.
    ///
    /// Fails CLOSED: any refusal, and any failure to mint the offer, is a
    /// deny. There is no second opinion to consult.
    fn authorize(
        &self,
        definition: &InteractionDefinition,
        audience: Audience,
        decoded: PromptChoice,
    ) -> Result<PromptChoice, newt_interaction::Refusal> {
        // Back and Exit are local CONTROLS, not decisions: they are not
        // options of the form, they authorize nothing, and routing them
        // through the authorizer would refuse them as unknown options and
        // silently turn a cancellation into a denial.
        if matches!(decoded, PromptChoice::Back | PromptChoice::Exit) {
            return Ok(decoded);
        }
        let Ok((instance, lifecycle)) = newt_core::interaction_gate::mint_offer(
            definition,
            &self.state.workspace_key,
            &self.conversation_id,
            audience.clone(),
            newt_core::interaction_gate::now_tick(),
        ) else {
            return Err(newt_interaction::Refusal::MissingRequiredControl {
                control: "decision".to_string(),
            });
        };
        newt_core::interaction_gate::authorize_action(
            definition,
            &instance,
            &lifecycle,
            &self.state.workspace_key,
            &newt_core::interaction_gate::permission_registry(audience.clone()),
            decoded,
            audience,
        )
    }

    /// Record an authorizer refusal, keeping the operator-facing message
    /// the fixed-enum code produced for the cases that have one.
    ///
    /// The DECISION is now `validate_response`'s — a high-danger session
    /// allow is refused because the form does not offer that option, not
    /// because a downstream `match` arm re-checked the tier. But the
    /// operator still gets the same sentence, and the audit record still
    /// carries the same specific reason, because a refusal that reads as
    /// a generic deny is a worse refusal.
    fn note_refusal(
        &mut self,
        w: &PromptWindow,
        req: &newt_core::PermissionRequest,
        decoded: PromptChoice,
        refusal: &newt_interaction::Refusal,
    ) {
        let high = self.danger.classify(req.kind, &req.target) == danger::DangerTier::High;
        let (scope, message) = match decoded {
            PromptChoice::AllowSession if high => (
                "session-allow-refused-high-danger",
                Some(format!(
                    "session allow refused for high-danger `{}` — \
                     allow once per op or deny (step-up is the future path)",
                    req.target
                )),
            ),
            PromptChoice::AllowPermanent if high => (
                "permanent-allow-refused-high-danger",
                Some(format!(
                    "permanent allow refused for high-danger `{}`",
                    req.target
                )),
            ),
            _ => ("unauthorized", None),
        };
        let _ = refusal;
        self.record(req, "deny", scope);
        if let Some(message) = message {
            w.notice(&newt_line(&message, self.color, self.verbose))
                .ok();
        }
    }

    fn web_abort_choice(
        &self,
        store: &newt_core::ConversationStore,
        conversation_id: &str,
        request_id: &str,
        fallback: PromptChoice,
    ) -> Option<(PromptChoice, &'static str)> {
        if store
            .resolve_permission_request(conversation_id, request_id, "tty")
            .ok()?
        {
            return Some((fallback, decision_scope(fallback)));
        }
        store
            .take_permission_decision(conversation_id, request_id)
            .ok()
            .and_then(|verdict| verdict.map(|v| (v.into(), verdict_scope(v))))
    }

    fn apply_control(&self, action: PromptChoice) {
        if let Some(cancel) = self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        if action == PromptChoice::Exit {
            if let Some(exit) = self.exit {
                exit.store(true, Ordering::Relaxed);
            }
        }
    }
}

impl<F: FnMut(&PromptWindow, &Question<PromptChoice>) -> PromptChoice> newt_core::PermissionGate
    for PromptPermissionGate<'_, F>
{
    fn ask(&mut self, requests: &[newt_core::PermissionRequest]) -> newt_core::PermissionDecision {
        use newt_core::PermissionDecision::{Allow, Deny};
        if requests.is_empty() {
            return Deny;
        }
        // Authorization prompting disabled (`--no-prompt-for-permissions`): fail
        // closed WITHOUT opening a prompt. This gate still exists (the session is
        // interactive) so `ask_question` keeps working — disabling permission
        // prompts must not turn a present operator into "headless".
        if !self.authorization_prompts_enabled {
            for req in requests {
                self.record(req, "deny", "authorization-prompts-disabled");
            }
            return Deny;
        }
        // Previously recorded session, permanent, and OCAP denials short-circuit.
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
        let web = self.state.web_store.clone();
        for req in requests {
            // GitWrite is non-axis, so enforce the readonly preset before minting.
            if req.kind == newt_core::DenialKind::GitWrite {
                if let Some(clamp) = &self.preset_clamp {
                    if !newt_core::git_caveats::GitCaveats::from_session(clamp).permits_commit() {
                        self.record(req, "deny", "preset-floor-git-readonly");
                        return Deny;
                    }
                }
            }
            if session_grant_covers(&self.state.session_grants, req) {
                continue;
            }
            // Durable approval pre-answers only a low-danger request.
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
            // Only the eventual operation consumes a request_permissions grant.
            if req.tool != "request_permissions" {
                if let Some(key) = take_pending_once(&mut self.state.pending_once_grants, req) {
                    self.record(req, "allow", "once");
                    once_grants.push(key);
                    continue;
                }
            }
            // The sole prompt seam suspends every competing terminal writer.
            let w = Terminal::suspend_for_prompt();
            let (choice, scope) = match &web {
                Some(store) => self.await_web_decision(store, &w, req),
                None => {
                    // ONE definition: rendered to the operator, and the
                    // authority the answer is checked against. No store is
                    // touched — see `b0b::the_default_terminal_path_
                    // performs_no_store_write`.
                    let definition = permission_definition(req, &self.danger, Audience::Terminal);
                    let form =
                        definition_to_question(&definition).expect(WIRE_NAMES_ARE_OPTION_IDS);
                    let decoded = (self.ask_human)(&w, &form);
                    match self.authorize(&definition, Audience::Terminal, decoded) {
                        Ok(choice) => (choice, decision_scope(choice)),
                        Err(refusal) => {
                            self.note_refusal(&w, req, decoded, &refusal);
                            return Deny;
                        }
                    }
                }
            };
            match choice {
                PromptChoice::AllowOnce => {
                    self.record(req, "allow", scope);
                    once_grants.push((req.kind, req.target.clone()));
                    // Carry proactive allow-once to the model's separate retry.
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
                    // The form omits this action for high danger; enforce it too.
                    if self.danger.classify(req.kind, &req.target) == danger::DangerTier::High {
                        self.record(req, "deny", "session-allow-refused-high-danger");
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
                    self.record(req, "allow", scope);
                    self.state
                        .session_grants
                        .insert((req.kind, req.target.clone()));
                }
                PromptChoice::AllowPermanent => {
                    // Only net has a durable per-target config allowlist.
                    if req.kind != newt_core::DenialKind::Net {
                        self.record(req, "allow", scope);
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
                    let persistent_scope = match self.config_path.as_deref() {
                        Some(path) => {
                            match newt_core::Config::append_permission_net_host(path, &req.target) {
                                Ok(()) => "permanent",
                                Err(e) => {
                                    w.notice(&newt_line(
                                        &format!(
                                            "warning: could not persist net grant to config: {e} \
                                             (granted for this session only)"
                                        ),
                                        self.color,
                                        self.verbose,
                                    ))
                                    .ok();
                                    "permanent-persist-failed"
                                }
                            }
                        }
                        None => {
                            w.notice(&newt_line(
                                "no config path this session — net grant is session-only",
                                self.color,
                                self.verbose,
                            ))
                            .ok();
                            "session"
                        }
                    };
                    self.record(req, "allow", persistent_scope);
                    if persistent_scope == "permanent" {
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
                    self.state
                        .session_grants
                        .insert((req.kind, req.target.clone()));
                }
                PromptChoice::Deny => {
                    self.record(req, "deny", scope);
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
                control @ (PromptChoice::Back | PromptChoice::Exit) => {
                    self.apply_control(control);
                    return Deny;
                }
            }
        }
        Allow(self.mint(&once_grants))
    }

    fn ask_question(&mut self, question: &str) -> HumanQuestionOutcome {
        // Blocking on the operator's decision surfaces to the cockpit through
        // the lifecycle seam: `Terminal::suspend_for_prompt` routes through the
        // tty arbiter, which emits Blocked/Unblocked. No surface-specific wiring.
        let w = Terminal::suspend_for_prompt();
        match prompt_user_input(&w, question) {
            // A submitted line (including an explicitly empty one) is an answer.
            Ok(ModalLine::Line(answer)) => HumanQuestionOutcome::Answer(answer),
            // EOF is the input stream closing — NOT an empty human answer.
            Ok(ModalLine::Eof) => HumanQuestionOutcome::InputClosed,
            // Esc / slash-command back-out: cancel the turn, report Cancelled
            // (never "headless"). `prompt_user_input` rewrites a slash command
            // into `Back`, so a typed `/cmd` also lands here and backs out.
            Ok(ModalLine::Back) => {
                self.apply_control(PromptChoice::Back);
                HumanQuestionOutcome::Cancelled
            }
            // Ctrl-C / Ctrl-D: cancel the turn AND request exit.
            Ok(ModalLine::Exit) => {
                self.apply_control(PromptChoice::Exit);
                HumanQuestionOutcome::ExitRequested
            }
            // A read error is distinct from a missing operator.
            Err(_) => HumanQuestionOutcome::InputFailed,
        }
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
                        .answer_permission_action(
                            &answer_conv,
                            &p.request_id,
                            PromptChoice::AllowOnce,
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
            authorization_prompts_enabled: true,
            web_decision_timeout: Duration::from_secs(2),
            cancel: None,
            exit: None,
            // Proof the TTY is bypassed when web decisions are on.
            ask_human: |_w: &PromptWindow, _q: &Question<PromptChoice>| {
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
            authorization_prompts_enabled: true,
            web_decision_timeout: Duration::from_millis(50),
            cancel: None,
            exit: None,
            ask_human: |_w: &PromptWindow, _q: &Question<PromptChoice>| {
                panic!("the TTY must not be read when web decisions are enabled")
            },
        };
        let started = Instant::now();
        let decision = gate.ask(&[exec_request("bash")]);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(decision, newt_core::PermissionDecision::Deny));
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.decisions[0].scope, "web-timeout");
        assert_eq!(store.pending_permission_request(&conv).unwrap(), None);
    }

    #[test]
    fn web_publish_failure_records_web_unavailable_scope() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let store = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let mut state = PermissionPromptState {
            web_store: Some(store),
            ..Default::default()
        };
        let mut gate = PromptPermissionGate {
            state: &mut state,
            base: Caveats::default(),
            key_path: None,
            conversation_id: "does-not-exist".to_string(),
            log_path: None,
            denials_path: None,
            config_path: None,
            preset_clamp: None,
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            authorization_prompts_enabled: true,
            web_decision_timeout: Duration::from_millis(50),
            cancel: None,
            exit: None,
            ask_human: |_w: &PromptWindow, _q: &Question<PromptChoice>| {
                panic!("the TTY must not be read when web decisions are enabled");
            },
        };
        let decision = gate.ask(&[exec_request("bash")]);
        assert!(matches!(decision, newt_core::PermissionDecision::Deny));
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.decisions[0].scope, "web-unavailable");
    }

    // ---- defect 1: recoverable web-wait control reader --------------------
    //
    // These drive `run_web_wait` directly with a SCRIPTED control reader, a fake
    // stepping clock, and a no-op sleep, so the recovery behaviour is fully
    // mocked (no real terminal or wall clock). They ground the invariant that a
    // transient reader error never permanently strands the operator, while
    // preserving the exactly-once TTY-vs-web CAS and the fail-closed deadline.

    use std::collections::VecDeque;
    use std::io;

    /// A control reader that replays a scripted sequence of poll results, then
    /// idles (`Ok(None)`). `io::Result` lets a test inject transient/broken errors.
    struct ScriptedReader(VecDeque<io::Result<Option<ModalLine>>>);
    impl newt_core::tty::ControlReader for ScriptedReader {
        fn poll(&mut self, _timeout: Duration) -> io::Result<Option<ModalLine>> {
            self.0.pop_front().unwrap_or(Ok(None))
        }
    }

    fn broken() -> io::Error {
        io::Error::other("reader broke")
    }

    /// A clock that advances a fixed `step` on each call — deterministic time
    /// without sleeping, so the deadline path terminates in bounded iterations.
    fn stepping_clock(step: Duration) -> impl Fn() -> Instant {
        let base = Instant::now();
        let n = std::cell::Cell::new(0u32);
        move || {
            let t = base + step * n.get();
            n.set(n.get().saturating_add(1));
            t
        }
    }

    /// Publish a low-danger exec question and return its `request_id`.
    pub(super) fn publish_low_danger(store: &newt_core::ConversationStore, conv: &str) -> String {
        let req = exec_request("bash");
        let question = question_for(&req, &danger::DangerTable::builtin(), Audience::Web);
        store
            .publish_permission_question(conv, &question, "\"low\"")
            .unwrap()
    }

    pub(super) fn store_and_conv() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        newt_core::ConversationStore,
        String,
    ) {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let store = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let conv = store.create("s", None).unwrap();
        (root, ws, store, conv)
    }

    /// Build a web gate with an explicit timeout and optional cancel/exit flags.
    macro_rules! web_gate {
        ($state:expr, $conv:expr, $timeout:expr, $cancel:expr, $exit:expr) => {
            PromptPermissionGate {
                state: $state,
                base: Caveats::default(),
                key_path: None,
                conversation_id: $conv,
                log_path: None,
                denials_path: None,
                config_path: None,
                preset_clamp: None,
                danger: danger::DangerTable::builtin(),
                color: false,
                verbose: false,
                authorization_prompts_enabled: true,
                web_decision_timeout: $timeout,
                cancel: $cancel,
                exit: $exit,
                ask_human: |_w: &PromptWindow, _q: &Question<PromptChoice>| {
                    panic!("run_web_wait must not read the TTY answer path")
                },
            }
        };
    }

    #[test]
    fn transient_reader_error_recovers_and_esc_resolves_through_the_tty_path() {
        // Reader #1 errors (non-Interrupted); after re-arm, reader #2 yields Esc.
        // The local abort wins the CAS and the gate returns Back.
        let (_r, _w, store, conv) = store_and_conv();
        let request_id = publish_low_danger(&store, &conv);
        let mut readers: VecDeque<ScriptedReader> = VecDeque::from([
            ScriptedReader(VecDeque::from([Err(broken())])),
            ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Back))])),
        ]);
        let mut state = PermissionPromptState {
            web_store: Some(store.clone()),
            ..Default::default()
        };
        let gate = web_gate!(
            &mut state,
            conv.clone(),
            Duration::from_secs(3600),
            None,
            None
        );
        let win = Terminal::suspend_for_prompt();
        let (choice, scope) = gate.run_web_wait(
            &store,
            &request_id,
            &win,
            || {
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            stepping_clock(Duration::from_millis(50)),
            |_d| {},
        );
        assert_eq!(choice, PromptChoice::Back);
        assert_eq!(scope, "control");
        // The local abort resolved the request (nothing left pending).
        assert!(store.pending_permission_request(&conv).unwrap().is_none());
    }

    #[test]
    fn transient_reader_error_does_not_deny_when_a_web_verdict_arrives() {
        // Reader errors, but a web ALLOW is already recorded: the temporary reader
        // failure must not force a denial — the web verdict is honored.
        let (_r, _w, store, conv) = store_and_conv();
        let request_id = publish_low_danger(&store, &conv);
        store
            .answer_permission_action(&conv, &request_id, PromptChoice::AllowOnce)
            .unwrap();
        let mut readers: VecDeque<ScriptedReader> =
            VecDeque::from([ScriptedReader(VecDeque::from([Err(broken())]))]);
        let mut state = PermissionPromptState {
            web_store: Some(store.clone()),
            ..Default::default()
        };
        let gate = web_gate!(
            &mut state,
            conv.clone(),
            Duration::from_secs(3600),
            None,
            None
        );
        let win = Terminal::suspend_for_prompt();
        let (choice, _scope) = gate.run_web_wait(
            &store,
            &request_id,
            &win,
            || {
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            stepping_clock(Duration::from_millis(50)),
            |_d| {},
        );
        assert_eq!(
            choice,
            PromptChoice::AllowOnce,
            "web allow must survive a reader error"
        );
    }

    #[test]
    fn reader_failing_until_deadline_denies_without_busy_spin() {
        // The reader can never be re-armed; the loop must NOT busy-spin (it paces
        // via sleep) and must resolve as the fail-closed timeout denial.
        let (_r, _w, store, conv) = store_and_conv();
        let request_id = publish_low_danger(&store, &conv);
        let mut state = PermissionPromptState {
            web_store: Some(store.clone()),
            ..Default::default()
        };
        let gate = web_gate!(
            &mut state,
            conv.clone(),
            Duration::from_millis(500),
            None,
            None
        );
        let win = Terminal::suspend_for_prompt();
        let sleeps = std::cell::Cell::new(0u32);
        let (choice, scope) = gate.run_web_wait(
            &store,
            &request_id,
            &win,
            || Err::<Box<dyn newt_core::tty::ControlReader>, _>(broken()),
            stepping_clock(Duration::from_millis(20)),
            |_d| sleeps.set(sleeps.get() + 1),
        );
        assert_eq!(choice, PromptChoice::Deny);
        assert_eq!(scope, "web-timeout");
        // Paced (slept at least once) and bounded (nowhere near a busy-spin).
        assert!(sleeps.get() >= 1, "must pace via sleep, not spin");
        assert!(
            sleeps.get() < 10_000,
            "bounded iterations: {}",
            sleeps.get()
        );
    }

    #[test]
    fn web_verdict_and_local_control_resolve_exactly_once() {
        // (a) Web verdict already recorded → a concurrent local Back consumes THAT
        //     verdict instead of overwriting it.
        let (_r1, _w1, store, conv) = store_and_conv();
        let request_id = publish_low_danger(&store, &conv);
        store
            .answer_permission_action(&conv, &request_id, PromptChoice::AllowOnce)
            .unwrap();
        let mut readers: VecDeque<ScriptedReader> =
            VecDeque::from([ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Back))]))]);
        let mut state = PermissionPromptState {
            web_store: Some(store.clone()),
            ..Default::default()
        };
        let gate = web_gate!(
            &mut state,
            conv.clone(),
            Duration::from_secs(3600),
            None,
            None
        );
        let win = Terminal::suspend_for_prompt();
        let (choice, _s) = gate.run_web_wait(
            &store,
            &request_id,
            &win,
            || {
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            stepping_clock(Duration::from_millis(50)),
            |_d| {},
        );
        assert_eq!(
            choice,
            PromptChoice::AllowOnce,
            "web verdict already won; local consumes it"
        );

        // (b) Local Back wins first → a later web answer cannot authorize.
        let (_r2, _w2, store2, conv2) = store_and_conv();
        let request_id2 = publish_low_danger(&store2, &conv2);
        let mut readers2: VecDeque<ScriptedReader> =
            VecDeque::from([ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Back))]))]);
        let mut state2 = PermissionPromptState {
            web_store: Some(store2.clone()),
            ..Default::default()
        };
        let gate2 = web_gate!(
            &mut state2,
            conv2.clone(),
            Duration::from_secs(3600),
            None,
            None
        );
        let win2 = Terminal::suspend_for_prompt();
        let (choice2, _s2) = gate2.run_web_wait(
            &store2,
            &request_id2,
            &win2,
            || {
                readers2
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            stepping_clock(Duration::from_millis(50)),
            |_d| {},
        );
        assert_eq!(choice2, PromptChoice::Back, "local abort won the race");
        // The request is resolved: a later web POST finds nothing to answer.
        assert!(store2.pending_permission_request(&conv2).unwrap().is_none());
        let late = store2
            .answer_permission_action(&conv2, &request_id2, PromptChoice::AllowOnce)
            .unwrap();
        assert!(
            !matches!(late, newt_core::store::AnswerOutcome::Answered),
            "a late web answer must not authorize an already-resolved request: {late:?}"
        );
    }

    #[test]
    fn ctrl_c_after_a_recoverable_reader_error_sets_cancel_and_exit() {
        // A reader error, then re-arm, then Ctrl-C/Ctrl-D → run_web_wait returns
        // Exit and (as ask() then applies) both the cancel AND exit flags set.
        let (_r, _w, store, conv) = store_and_conv();
        let request_id = publish_low_danger(&store, &conv);
        let mut readers: VecDeque<ScriptedReader> = VecDeque::from([
            ScriptedReader(VecDeque::from([Err(broken())])),
            ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Exit))])),
        ]);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let exit = std::sync::atomic::AtomicBool::new(false);
        let mut state = PermissionPromptState {
            web_store: Some(store.clone()),
            ..Default::default()
        };
        let gate = web_gate!(
            &mut state,
            conv.clone(),
            Duration::from_secs(3600),
            Some(&cancel),
            Some(&exit)
        );
        let win = Terminal::suspend_for_prompt();
        let (choice, _s) = gate.run_web_wait(
            &store,
            &request_id,
            &win,
            || {
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            stepping_clock(Duration::from_millis(50)),
            |_d| {},
        );
        assert_eq!(choice, PromptChoice::Exit);
        // ask() applies the control on Back|Exit; Exit sets both signals.
        gate.apply_control(choice);
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "cancel must be set"
        );
        assert!(
            exit.load(std::sync::atomic::Ordering::Relaxed),
            "exit must be set"
        );
    }

    #[test]
    fn repeated_interrupted_keeps_the_reader_and_paces_without_spinning() {
        // EINTR returns immediately; the SAME reader is retried (not dropped) and
        // the loop paces via sleep rather than busy-spinning. After several
        // Interrupted errors the same reader yields Esc, which still resolves.
        let (_r, _w, store, conv) = store_and_conv();
        let request_id = publish_low_danger(&store, &conv);
        let mut readers: VecDeque<ScriptedReader> =
            VecDeque::from([ScriptedReader(VecDeque::from([
                Err(io::Error::from(io::ErrorKind::Interrupted)),
                Err(io::Error::from(io::ErrorKind::Interrupted)),
                Err(io::Error::from(io::ErrorKind::Interrupted)),
                Ok(Some(ModalLine::Back)),
            ]))]);
        let mut state = PermissionPromptState {
            web_store: Some(store.clone()),
            ..Default::default()
        };
        let gate = web_gate!(
            &mut state,
            conv.clone(),
            Duration::from_secs(3600),
            None,
            None
        );
        let win = Terminal::suspend_for_prompt();
        let reacquired = std::cell::Cell::new(0u32);
        let sleeps = std::cell::Cell::new(0u32);
        let (choice, _s) = gate.run_web_wait(
            &store,
            &request_id,
            &win,
            || {
                reacquired.set(reacquired.get() + 1);
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            stepping_clock(Duration::from_millis(50)),
            |_d| sleeps.set(sleeps.get() + 1),
        );
        assert_eq!(
            choice,
            PromptChoice::Back,
            "same reader survives EINTR and yields Esc"
        );
        assert_eq!(
            reacquired.get(),
            1,
            "an Interrupted error must NOT drop/recreate the reader"
        );
        // Paced (slept between EINTR retries) and bounded (no busy-spin).
        assert!(
            sleeps.get() >= 3,
            "must pace between EINTR retries: {}",
            sleeps.get()
        );
        assert!(sleeps.get() < 10_000, "bounded: {}", sleeps.get());
    }

    #[test]
    fn an_initial_unsupported_still_retries_and_recovers() {
        // A terminal-loss race at the FIRST acquisition (Unsupported) must NOT
        // permanently disable controls — the gate is only built for an
        // interactive session, so this is a race, not a headless session. Keep
        // retrying, re-arm when the terminal returns, and Esc still resolves.
        let (_r, _w, store, conv) = store_and_conv();
        let request_id = publish_low_danger(&store, &conv);
        let mut outcomes: VecDeque<io::Result<ScriptedReader>> = VecDeque::from([
            Err(io::Error::from(io::ErrorKind::Unsupported)), // INITIAL acquisition fails
            Ok(ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Back))]))), // terminal returns
        ]);
        let mut state = PermissionPromptState {
            web_store: Some(store.clone()),
            ..Default::default()
        };
        let gate = web_gate!(
            &mut state,
            conv.clone(),
            Duration::from_secs(3600),
            None,
            None
        );
        let win = Terminal::suspend_for_prompt();
        let (choice, _s) = gate.run_web_wait(
            &store,
            &request_id,
            &win,
            || {
                outcomes
                    .pop_front()
                    .unwrap_or_else(|| Err(broken()))
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
            },
            stepping_clock(Duration::from_millis(50)),
            |_d| {},
        );
        assert_eq!(
            choice,
            PromptChoice::Back,
            "an initial Unsupported must not permanently disable controls"
        );
    }

    #[test]
    fn a_post_live_unsupported_keeps_retrying_and_recovers() {
        // A gate built for an interactive session that momentarily loses its
        // terminal (reacquire → Unsupported) keeps retrying (bounded) and re-arms
        // when the terminal returns, then Esc still resolves. Guards against
        // recreating the original permanent-disable defect shape.
        let (_r, _w, store, conv) = store_and_conv();
        let request_id = publish_low_danger(&store, &conv);
        let mut outcomes: VecDeque<io::Result<ScriptedReader>> = VecDeque::from([
            Ok(ScriptedReader(VecDeque::from([Err(broken())]))), // Live, then breaks
            Err(io::Error::from(io::ErrorKind::Unsupported)),    // terminal "gone"
            Err(io::Error::from(io::ErrorKind::Unsupported)),    // still gone
            Ok(ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Back))]))), // back
        ]);
        let mut state = PermissionPromptState {
            web_store: Some(store.clone()),
            ..Default::default()
        };
        let gate = web_gate!(
            &mut state,
            conv.clone(),
            Duration::from_secs(3600),
            None,
            None
        );
        let win = Terminal::suspend_for_prompt();
        let (choice, _s) = gate.run_web_wait(
            &store,
            &request_id,
            &win,
            || {
                outcomes
                    .pop_front()
                    .unwrap_or_else(|| Err(broken()))
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
            },
            stepping_clock(Duration::from_millis(50)),
            |_d| {},
        );
        assert_eq!(
            choice,
            PromptChoice::Back,
            "a transient Unsupported must not permanently disable controls"
        );
    }

    // ---- defect: authorization-prompt policy is separate from human presence -
    // The gate is built whenever the session has a usable TTY; permission
    // prompting is a separate policy (`authorization_prompts_enabled`). Disabling
    // it must deny authorization WITHOUT prompting, and must NOT erase the
    // operator from `request_user_input` (proven in newt-core's
    // `request_user_input_reaches_the_operator_even_when_permissions_are_denied`).

    #[test]
    fn authorization_prompts_disabled_denies_without_opening_a_prompt() {
        // TTY + permissions DISABLED: ask() denies and never consults the human
        // (the empty script would panic on any prompt).
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0usize));
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            None,
            vec![],
            prompts.clone(),
        );
        gate.authorization_prompts_enabled = false;
        let decision = gate.ask(&[exec_request("bash")]);
        assert!(matches!(decision, newt_core::PermissionDecision::Deny));
        assert_eq!(prompts.get(), 0, "disabled prompts must not open a prompt");
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.decisions[0].scope, "authorization-prompts-disabled");
    }

    #[test]
    fn authorization_prompts_enabled_consults_the_operator() {
        // TTY + permissions ENABLED: ask() DOES prompt (scripted allow-once).
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0usize));
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            None,
            vec![PromptChoice::AllowOnce],
            prompts.clone(),
        );
        let decision = gate.ask(&[exec_request("bash")]);
        assert!(matches!(decision, newt_core::PermissionDecision::Allow(_)));
        assert_eq!(
            prompts.get(),
            1,
            "enabled prompts consult the operator once"
        );
    }

    #[test]
    fn allow_permanent_records_session_scope_when_net_persist_fails() {
        let root = tempfile::TempDir::new().unwrap();
        let config = root.path().join("blocked-config-dir");
        std::fs::create_dir_all(&config).unwrap();
        let base = base_caveats("/ws");
        let net_req = newt_core::PermissionRequest {
            tool: "web_fetch".to_string(),
            kind: DenialKind::Net,
            target: "github.com".to_string(),
            reason: "net does not permit 'github.com'".to_string(),
        };

        let mut state = PermissionPromptState::default();
        {
            let mut gate = PromptPermissionGate {
                state: &mut state,
                base,
                key_path: None,
                conversation_id: "conv-config-fail".to_string(),
                log_path: None,
                denials_path: None,
                config_path: Some(config.clone()),
                preset_clamp: None,
                danger: danger::DangerTable::builtin(),
                color: false,
                verbose: false,
                authorization_prompts_enabled: true,
                web_decision_timeout: Duration::from_secs(2),
                cancel: None,
                exit: None,
                ask_human: move |_w: &PromptWindow, _q: &Question<PromptChoice>| {
                    PromptChoice::AllowPermanent
                },
            };
            assert!(matches!(
                gate.ask(std::slice::from_ref(&net_req)),
                newt_core::PermissionDecision::Allow(_)
            ));
        }
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(
            state.decisions[0].scope, "permanent-persist-failed",
            "failed net persistence should not be logged as durable"
        );
    }

    /// A gate whose "human" is a script of choices; counts every prompt.
    pub(super) fn scripted_gate<'a>(
        state: &'a mut PermissionPromptState,
        base: Caveats,
        key_path: Option<std::path::PathBuf>,
        log_path: Option<std::path::PathBuf>,
        script: Vec<PromptChoice>,
        prompts: Rc<Cell<usize>>,
    ) -> PromptPermissionGate<'a, impl FnMut(&PromptWindow, &Question<PromptChoice>) -> PromptChoice>
    {
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
            authorization_prompts_enabled: true,
            web_decision_timeout: Duration::from_secs(2),
            cancel: None,
            exit: None,
            ask_human: move |_w: &PromptWindow, _question: &Question<PromptChoice>| {
                prompts.set(prompts.get() + 1);
                script.next().expect("script exhausted — unexpected prompt")
            },
        }
    }

    #[test]
    fn nested_controls_cancel_without_recording_a_permission_decision() {
        for (choice, exits) in [(PromptChoice::Back, false), (PromptChoice::Exit, true)] {
            let cancel = AtomicBool::new(false);
            let exit = AtomicBool::new(false);
            let mut state = PermissionPromptState::default();
            let prompts = Rc::new(Cell::new(0));
            let mut gate = scripted_gate(
                &mut state,
                base_caveats("/ws"),
                None,
                None,
                vec![choice],
                prompts,
            );
            gate.cancel = Some(&cancel);
            gate.exit = Some(&exit);
            assert!(matches!(
                gate.ask(&[exec_request("npm")]),
                newt_core::PermissionDecision::Deny
            ));
            drop(gate);
            assert!(cancel.load(Ordering::Relaxed));
            assert_eq!(exit.load(Ordering::Relaxed), exits);
            assert!(state.decisions.is_empty());
        }
    }

    #[test]
    fn question_policy_and_markdown_cover_each_axis_and_danger_tier() {
        let danger = danger::DangerTable::builtin();
        for (kind, target, wording) in [
            (DenialKind::FsRead, "/etc/hosts", "read"),
            (DenialKind::FsWrite, "/ws/f", "write"),
            (DenialKind::Net, "docs.rs", "reach"),
            (DenialKind::RemoteTool, "remote__tool", "call"),
            (DenialKind::GitWrite, "commit", "commit/stage via git"),
        ] {
            let q = permission_question(
                &PermissionRequest {
                    tool: "tool".into(),
                    kind,
                    target: target.into(),
                    reason: String::new(),
                },
                &danger,
            );
            assert!(q.markdown.contains(&format!("{wording} `{target}`")));
        }

        let low = permission_question(&exec_request("npm"), &danger);
        assert!(low
            .actions
            .iter()
            .any(|a| a.value == PromptChoice::AllowSession));
        assert!(low.markdown.contains("outside the granted exec allowlist"));

        let high = permission_question(
            &PermissionRequest {
                tool: "request_permissions".into(),
                kind: DenialKind::Exec,
                target: "bash".into(),
                reason: "list the files".into(),
            },
            &danger,
        );
        assert!(!high
            .actions
            .iter()
            .any(|a| a.value == PromptChoice::AllowSession));
        let text = high.terminal_text();
        for expected in [
            "interpreter",
            "arbitrary command execution",
            "model-authored, unverified",
            "list the files",
            "session allow refused",
        ] {
            assert!(text.contains(expected), "missing {expected:?}: {text}");
        }

        let root = permission_question(
            &PermissionRequest {
                tool: "request_permissions".into(),
                kind: DenialKind::FsWrite,
                target: "/".into(),
                reason: String::new(),
            },
            &danger,
        );
        assert!(root.markdown.contains("filesystem root"));
        assert!(!root
            .actions
            .iter()
            .any(|a| a.value == PromptChoice::AllowSession));

        let web_low = question_for(&exec_request("npm"), &danger, Audience::Web);
        assert_eq!(
            web_low.actions.iter().map(|a| a.value).collect::<Vec<_>>(),
            [
                PromptChoice::AllowOnce,
                PromptChoice::AllowSession,
                PromptChoice::Deny
            ]
        );
        assert_eq!(
            serde_json::from_str::<Question<PromptChoice>>(
                &serde_json::to_string(&web_low).unwrap()
            )
            .unwrap(),
            web_low
        );
        let web_high = question_for(&exec_request("bash"), &danger, Audience::Web);
        assert_eq!(
            web_high.actions.iter().map(|a| a.value).collect::<Vec<_>>(),
            [PromptChoice::AllowOnce, PromptChoice::Deny]
        );
    }

    #[test]
    fn high_danger_target_is_not_session_allowable_but_allow_once_works() {
        let base = base_caveats("/ws");

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
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.decisions[0].decision, "deny");
        assert!(
            state.decisions[0].scope.contains("refused"),
            "the record must mark the high-danger refusal, got: {}",
            state.decisions[0].scope
        );

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
        assert!(once_state.session_grants.is_empty());
    }

    fn ocap(
        verdict: newt_core::ocap_store::Verdict,
        toml: &str,
    ) -> newt_core::ocap_store::PolicySet {
        newt_core::ocap_store::build_store(&[(verdict, Some(toml.to_string()))]).0
    }

    #[test]
    fn durable_ocap_approve_allows_without_prompting_and_grants_authority() {
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
        assert!(state.session_grants.is_empty());
    }

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
                authorization_prompts_enabled: true,
                web_decision_timeout: Duration::from_secs(2),
                cancel: None,
                exit: None,
                ask_human: move |_w: &PromptWindow, _q: &Question<PromptChoice>| {
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
                authorization_prompts_enabled: true,
                web_decision_timeout: Duration::from_secs(2),
                cancel: None,
                exit: None,
                ask_human: |_w: &PromptWindow, _q: &Question<PromptChoice>| {
                    panic!("must NOT prompt: target was permanently denied")
                },
            };
            assert!(matches!(
                gate.ask(std::slice::from_ref(&net_req)),
                newt_core::PermissionDecision::Deny
            ));
        }
        assert!(fresh.decisions.is_empty());
    }

    #[test]
    fn permanent_allow_offered_for_net_only() {
        let danger = danger::DangerTable::builtin();
        let net = permission_question(
            &PermissionRequest {
                tool: "web_fetch".to_string(),
                kind: DenialKind::Net,
                target: "github.com".to_string(),
                reason: String::new(),
            },
            &danger,
        )
        .terminal_text();
        let exec = permission_question(&exec_request("npm"), &danger).terminal_text();
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

    #[test]
    fn allow_permanently_grants_now_and_persists_host_to_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
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
                authorization_prompts_enabled: true,
                web_decision_timeout: Duration::from_secs(2),
                cancel: None,
                exit: None,
                ask_human: move |_w: &PromptWindow, _q: &Question<PromptChoice>| {
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
        assert!(state
            .session_grants
            .contains(&(DenialKind::Net, "github.com".to_string())));
        assert_eq!(state.decisions[0].scope, "permanent");
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
        assert!(matches!(
            gate.ask(&[exec_request("/opt/bin/mytool")]),
            newt_core::PermissionDecision::Allow(_)
        ));
        assert_eq!(prompts.get(), 1, "basename covers the resolved path");
        assert!(matches!(
            gate.ask(&[exec_request("othertool")]),
            newt_core::PermissionDecision::Allow(_)
        ));
        assert_eq!(prompts.get(), 2, "a different program is not covered");
    }

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
        let mut gate = scripted_gate(
            &mut state,
            base,
            None,
            None,
            vec![PromptChoice::AllowOnce],
            prompts.clone(),
        );
        gate.preset_clamp = Some(clamp);
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

    #[test]
    fn session_grant_cannot_pierce_the_preset_floor() {
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        let clamp = newt_core::NamedPermissionPreset {
            readonly: true,
            ..Default::default()
        }
        .clamp();
        let base = base_caveats("/ws").meet(&clamp);
        assert!(
            !base.permits_exec("cargo"),
            "the preset clamped exec to none"
        );

        let mut gate = scripted_gate(
            &mut state,
            base.clone(),
            None,
            None,
            vec![PromptChoice::AllowOnce, PromptChoice::AllowSession],
            prompts.clone(),
        );
        gate.preset_clamp = Some(clamp.clone());
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
        assert!(state
            .session_grants
            .contains(&(DenialKind::Exec, "rm".to_string())));
    }

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
            assert!(matches!(
                gate.ask(&req),
                newt_core::PermissionDecision::Allow(_)
            ));
        }
        {
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
        assert_eq!(state.decisions, records);
    }

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
        drop(gate);
        assert_eq!(base, base_caveats("/ws"));
        let policy = newt_core::widen_caveats(&base, &[(DenialKind::Exec, "npm".to_string())]);
        let key = mint_operating_key(&key_path, &policy).unwrap();
        assert_eq!(newt_identity::enforced_caveats(&key).unwrap(), minted);
    }

    #[serial_test::serial(real_fs)]
    #[tokio::test]
    async fn execute_tool_with_tui_gate_allow_once_then_reprompt() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("outside.txt"), "gated contents").unwrap();
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
        assert!(out.contains("request_permissions"), "got: {out}");
        assert_eq!(prompts.get(), 2, "allow-once does not stick");
        drop(gate);
        assert_eq!(state.decisions.len(), 2);
    }

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

    /// Exhaust the boolean product: no headless/non-TTY case may open a prompt.
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

/// **A0 byte goldens (#1823, epic #1803): the plain permission-prompt
/// rendering, frozen verbatim.** These strings ARE the current contract —
/// including the deliberately DIFFERENT terminal/web action matrices (web
/// omits every durable grant; net+low+terminal is the only permanent-allow) —
/// and epic law 1 says a later slice must keep the plain fallback while
/// C0 extracts `terminal_text()` out of the semantic type. An intentional
/// rendering change updates these strings in the same PR, listed as an
/// intentional diff (unlisted golden drift is a bug, per the epic's F0 rule).
#[cfg(test)]
mod a0_freeze_goldens {
    use super::*;
    use newt_core::{DenialKind, PermissionRequest};

    fn net_low() -> PermissionRequest {
        PermissionRequest {
            tool: "http".into(),
            kind: DenialKind::Net,
            target: "https://example.com/api".into(),
            reason: String::new(),
        }
    }

    fn exec_high() -> PermissionRequest {
        PermissionRequest {
            tool: "run_command".into(),
            kind: DenialKind::Exec,
            target: "bash".into(),
            reason: "exec of \"bash\" is not within the granted authority".into(),
        }
    }

    fn text(req: &PermissionRequest, audience: Audience) -> String {
        let table = danger::DangerTable::builtin();
        question_for(req, &table, audience).terminal_text()
    }

    #[test]
    fn terminal_low_net_offers_every_grant_including_the_permanents() {
        assert_eq!(
            text(&net_low(), Audience::Terminal),
            "\u{2298} http wants to reach `https://example.com/api` \u{2014} outside the granted net allowlist.\n\
             Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
             [a]llow once   [s]ession allow   [A]llow permanently (adds host to config)   [d]eny (default)   [D]eny always   [P]ermanently deny"
        );
    }

    #[test]
    fn web_low_net_omits_every_durable_grant() {
        assert_eq!(
            text(&net_low(), Audience::Web),
            "\u{2298} http wants to reach `https://example.com/api` \u{2014} outside the granted net allowlist.\n\
             Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
             [a]llow once   [s]ession allow   [d]eny (default)"
        );
    }

    #[test]
    fn terminal_high_exec_refuses_session_allow_and_says_why() {
        assert_eq!(
            text(&exec_high(), Audience::Terminal),
            "\u{2298} run_command wants to run `bash` \u{2014} outside the granted exec allowlist.\n\
             \u{26a0} `bash` is an interpreter: this grants arbitrary command execution\n  \
             (exec of \"bash\" is not within the granted authority)\n\
             high-danger: session allow refused; key allow / step-up is the future path, P3\n\
             Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
             [a]llow once   [d]eny (default)   [D]eny always   [P]ermanently deny"
        );
    }

    #[test]
    fn web_high_exec_is_allow_once_or_deny_only() {
        assert_eq!(
            text(&exec_high(), Audience::Web),
            "\u{2298} run_command wants to run `bash` \u{2014} outside the granted exec allowlist.\n\
             \u{26a0} `bash` is an interpreter: this grants arbitrary command execution\n  \
             (exec of \"bash\" is not within the granted authority)\n\
             High danger: session authorization is unavailable.\n\
             Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
             [a]llow once   [d]eny (default)"
        );
    }

    /// The control hint every prompt note carries; the goldens above embed it,
    /// and the free-text form (`prompt_user_input`) reuses the same constant.
    #[test]
    fn the_modal_control_hint_is_the_frozen_control_vocabulary() {
        assert_eq!(MODAL_CONTROL_HINT, "Esc=back \u{b7} Ctrl-C/Ctrl-D=exit");
    }
}

/// **B0a (#1841): one definition builder for both surfaces.**
///
/// A0 froze the rendered strings; these tests prove the DEFINITION path
/// reproduces them and that the per-surface action matrices live in the
/// definition rather than in a renderer. The golden strings are repeated
/// here deliberately: A0's copy freezes the contract, this copy proves the
/// new construction path reaches it, and an accidental edit to one copy is
/// caught by the other.
///
/// **`Question::parse` remains the authoritative accept/deny path.**
/// Nothing here uses `binding::validate_response` — that move, with the
/// behavior-map and formal-model updates it requires, is #1842.
#[cfg(test)]
mod b0a {
    use super::*;
    use newt_core::interaction_adapter::question_to_definition;
    use newt_core::{Action, DenialKind, PermissionRequest};

    fn net_low() -> PermissionRequest {
        PermissionRequest {
            tool: "http".into(),
            kind: DenialKind::Net,
            target: "https://example.com/api".into(),
            reason: String::new(),
        }
    }

    fn exec_high() -> PermissionRequest {
        PermissionRequest {
            tool: "run_command".into(),
            kind: DenialKind::Exec,
            target: "bash".into(),
            reason: "exec of \"bash\" is not within the granted authority".into(),
        }
    }

    /// The option wire names a definition offers, in order.
    fn offered(definition: &InteractionDefinition) -> Vec<String> {
        let [control] = definition.controls.as_slice() else {
            panic!("expected exactly one control");
        };
        let ControlKind::Choice { options } = &control.kind else {
            panic!("the decision control is not a choice");
        };
        options.iter().map(|o| o.id.as_str().to_string()).collect()
    }

    fn definition_of(req: &PermissionRequest, audience: Audience) -> InteractionDefinition {
        permission_definition(req, &danger::DangerTable::builtin(), audience)
    }

    #[test]
    fn both_surfaces_build_one_definition_from_one_builder() {
        for req in [net_low(), exec_high()] {
            for audience in [Audience::Terminal, Audience::Web] {
                let definition = definition_of(&req, audience.clone());
                // ONE definition, ONE control, and it is the reserved
                // decision control the adapter and #1842 both address.
                assert_eq!(definition.controls.len(), 1);
                assert_eq!(definition.controls[0].id.as_str(), DECISION_CONTROL);
                assert!(matches!(
                    definition.controls[0].kind,
                    ControlKind::Choice { .. }
                ));
                assert_eq!(definition.kind, InteractionKind::Choice);
                assert_eq!(definition.controls[0].requirement, Requirement::Required);

                // The rendered form is the adapter's output for exactly
                // this definition — not a second renderer that happens to
                // agree.
                let via_adapter = definition_to_question(&definition).expect("adapts");
                assert_eq!(
                    question_for(&req, &danger::DangerTable::builtin(), audience.clone()),
                    via_adapter
                );
            }
        }
        // The two surfaces differ only where policy says they do.
        assert_ne!(
            offered(&definition_of(&net_low(), Audience::Terminal)),
            offered(&definition_of(&net_low(), Audience::Web)),
            "the surfaces would be indistinguishable, so the matrices are not being applied"
        );
    }

    #[test]
    fn the_terminal_matrix_is_byte_identical_to_its_a0_golden() {
        let definition = definition_of(&net_low(), Audience::Terminal);
        assert_eq!(
            offered(&definition),
            [
                "allow_once",
                "allow_session",
                "allow_permanent",
                "deny",
                "deny_always",
                "deny_permanent"
            ]
        );
        assert_eq!(
            definition_to_question(&definition)
                .expect("adapts")
                .terminal_text(),
            "\u{2298} http wants to reach `https://example.com/api` \u{2014} outside the granted net allowlist.\n\
             Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
             [a]llow once   [s]ession allow   [A]llow permanently (adds host to config)   [d]eny (default)   [D]eny always   [P]ermanently deny"
        );
    }

    #[test]
    fn the_web_matrix_is_byte_identical_to_its_a0_golden() {
        let definition = definition_of(&net_low(), Audience::Web);
        assert_eq!(
            offered(&definition),
            ["allow_once", "allow_session", "deny"]
        );
        assert_eq!(
            definition_to_question(&definition)
                .expect("adapts")
                .terminal_text(),
            "\u{2298} http wants to reach `https://example.com/api` \u{2014} outside the granted net allowlist.\n\
             Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
             [a]llow once   [s]ession allow   [d]eny (default)"
        );
    }

    #[test]
    fn a_high_tier_target_offers_no_session_allow_on_either_surface() {
        for audience in [Audience::Terminal, Audience::Web] {
            let offered = offered(&definition_of(&exec_high(), audience.clone()));
            assert!(
                !offered.iter().any(|id| id == "allow_session"),
                "a high-danger target offered a session allow to {audience:?}: {offered:?}"
            );
            assert!(
                !offered.iter().any(|id| id == "allow_permanent"),
                "a high-danger target offered a permanent allow to {audience:?}: {offered:?}"
            );
            // ...and it still offers a way to say yes once, or the prompt
            // would be a notice rather than a decision.
            assert!(offered.iter().any(|id| id == "allow_once"));
        }
    }

    #[test]
    fn the_web_definition_never_offers_a_durable_grant() {
        const DURABLE: [&str; 3] = ["allow_permanent", "deny_always", "deny_permanent"];
        for req in [net_low(), exec_high()] {
            let offered = offered(&definition_of(&req, Audience::Web));
            for durable in DURABLE {
                assert!(
                    !offered.iter().any(|id| id == durable),
                    "the web definition offered `{durable}` for {}: {offered:?}",
                    req.target
                );
            }
        }
        // The terminal still does, for the one case A0 froze — otherwise
        // this test would pass by the grants having been deleted for
        // everyone.
        assert!(offered(&definition_of(&net_low(), Audience::Terminal))
            .iter()
            .any(|id| id == "allow_permanent"));
    }

    /// Aliases and ambiguity denial are properties of `Question::parse`,
    /// which B0a leaves authoritative. What changes is that the form now
    /// arrives through a definition — so the property must survive the
    /// round trip.
    #[test]
    fn an_alias_still_resolves_and_an_ambiguous_answer_is_still_denied() {
        let with_alias = Question::<PromptChoice> {
            markdown: "confirm".to_string(),
            actions: vec![
                Action::new(PromptChoice::AllowOnce, "y", "yes").with_aliases(["Y"]),
                Action::new(PromptChoice::Deny, "n", "no").with_aliases(["N"]),
            ],
            note: None,
        };
        let adapted = definition_to_question(&question_to_definition(&with_alias).expect("adapts"))
            .expect("back");
        assert_eq!(adapted.parse("Y"), Some(PromptChoice::AllowOnce));
        assert_eq!(adapted.parse("n"), Some(PromptChoice::Deny));

        // Ambiguity still denies: two actions sharing one alias resolve to
        // nothing, and the caller's fail-closed default stands.
        let ambiguous = Question::<PromptChoice> {
            markdown: "confirm".to_string(),
            actions: vec![
                Action::new(PromptChoice::AllowOnce, "y", "yes").with_aliases(["x"]),
                Action::new(PromptChoice::Deny, "n", "no").with_aliases(["x"]),
            ],
            note: None,
        };
        let adapted = definition_to_question(&question_to_definition(&ambiguous).expect("adapts"))
            .expect("back");
        assert_eq!(adapted.parse("x"), None, "an ambiguous answer resolved");

        // And the real permission menu still parses its own keys.
        let menu = permission_question(&net_low(), &danger::DangerTable::builtin());
        assert_eq!(menu.parse("a"), Some(PromptChoice::AllowOnce));
        assert_eq!(menu.parse("A"), Some(PromptChoice::AllowPermanent));
        assert_eq!(menu.parse("zzz"), None);
    }

    /// The `expect`s in `permission_definition` and `question_for` are
    /// unreachable rather than merely unlikely: every combination this
    /// policy can produce builds and adapts.
    #[test]
    fn every_offered_action_is_a_valid_option_id() {
        let kinds = [
            (DenialKind::Exec, "npm"),
            (DenialKind::Exec, "bash"),
            (DenialKind::FsRead, "/etc/passwd"),
            (DenialKind::FsWrite, "/tmp/x"),
            (DenialKind::Net, "https://example.com/api"),
            (DenialKind::RemoteTool, "some_tool"),
            (DenialKind::GitWrite, "origin/main"),
        ];
        for (kind, target) in kinds {
            for audience in [Audience::Terminal, Audience::Web] {
                let req = PermissionRequest {
                    tool: "t".into(),
                    kind,
                    target: target.into(),
                    reason: String::new(),
                };
                let definition = definition_of(&req, audience.clone());
                assert!(!offered(&definition).is_empty());
                definition_to_question(&definition).unwrap_or_else(|e| {
                    panic!("{kind:?}/{target}/{audience:?} did not adapt: {e}")
                });
            }
        }
    }
}

/// **B0b-1 (#1842): the accept/deny decision, on the A3 controller.**
///
/// `Question::parse` is KEPT and demoted to input decoding — aliases,
/// ambiguity denial, case-distinct keys. `validate_response` authorizes.
/// These tests pin the four facts that move, and the two that must not.
#[cfg(test)]
mod b0b {
    use super::*;
    // The store/gate fixtures already exist next door; a second copy of
    // them would drift from the ones the pre-B0b tests use, and then the
    // parity these tests claim would be against a different fixture.
    use super::permission_prompt_tests::{publish_low_danger, scripted_gate, store_and_conv};
    use newt_core::interaction_gate::{
        authorize_action, mint_offer, now_tick, permission_registry,
    };
    use newt_core::{AnswerOutcome, Caveats, DenialKind, PermissionGate, PermissionRequest};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::{Arc, Barrier};

    fn request() -> PermissionRequest {
        PermissionRequest {
            tool: "run_command".into(),
            kind: DenialKind::Exec,
            target: "bash".into(),
            reason: String::new(),
        }
    }

    fn offer(
        audience: Audience,
    ) -> (
        InteractionDefinition,
        newt_interaction::InteractionInstance,
        newt_interaction::Lifecycle,
    ) {
        let definition = permission_definition(
            &request(),
            &danger::DangerTable::builtin(),
            audience.clone(),
        );
        let (instance, lifecycle) =
            mint_offer(&definition, "ws", "conv-1", audience, now_tick()).expect("mints");
        (definition, instance, lifecycle)
    }

    /// The two wall clocks are now ONE relationship, asserted.
    #[test]
    fn the_gate_timeout_is_shorter_than_the_store_ttl() {
        let gate_nanos = i64::try_from(WEB_DECISION_TIMEOUT.as_nanos()).expect("fits");
        let store_ttl = newt_core::ConversationStore::PERMISSION_REQUEST_TTL_NANOS;
        assert!(
            gate_nanos < store_ttl,
            "the gate must give up while the offer is still answerable: \
             gate {gate_nanos}ns vs store TTL {store_ttl}ns"
        );
        // ...and an offer carries the store's number, so the two cannot
        // drift apart again.
        let (_d, instance, _l) = offer(Audience::Terminal);
        assert_eq!(instance.ttl_ticks, store_ttl);
    }

    /// An offer past its TTL authorizes nothing — the fail-closed default
    /// is a denial produced by refusing every response, never a synthesized
    /// allow.
    #[test]
    fn an_expired_permission_denies_by_default() {
        let (definition, instance, lifecycle) = offer(Audience::Terminal);
        assert!(!newt_interaction::Lifecycle::has_elapsed(
            &instance,
            instance.provenance.minted_tick
        ));
        assert!(newt_interaction::Lifecycle::has_elapsed(
            &instance,
            instance.provenance.minted_tick + instance.ttl_ticks
        ));

        let expired = lifecycle.expire().expect("expires");
        let registry = permission_registry(Audience::Terminal);
        for action in [PromptChoice::AllowOnce, PromptChoice::AllowSession] {
            assert!(
                authorize_action(
                    &definition,
                    &instance,
                    &expired,
                    "ws",
                    &registry,
                    action,
                    Audience::Terminal
                )
                .is_err(),
                "an expired offer authorized {action:?}"
            );
        }
    }

    /// The registry is the CALLER's and is not derived from the form, so
    /// an action the form offers but the gate cannot execute is refused —
    /// and the durable grants stay terminal-only even against a form that
    /// offered them.
    #[test]
    fn the_registry_is_independent_of_the_form() {
        let (definition, instance, lifecycle) = offer(Audience::Terminal);
        // Empty registry: nothing is executable, so nothing authorizes.
        assert!(authorize_action(
            &definition,
            &instance,
            &lifecycle,
            "ws",
            &[],
            PromptChoice::AllowOnce,
            Audience::Terminal
        )
        .is_err());

        // The web registry refuses a durable grant even when handed a
        // TERMINAL form that offers it.
        let low = PermissionRequest {
            tool: "http".into(),
            kind: DenialKind::Net,
            target: "https://example.com/api".into(),
            reason: String::new(),
        };
        let terminal_form =
            permission_definition(&low, &danger::DangerTable::builtin(), Audience::Terminal);
        let (inst, life) =
            mint_offer(&terminal_form, "ws", "conv-1", Audience::Web, now_tick()).expect("mints");
        assert!(
            authorize_action(
                &terminal_form,
                &inst,
                &life,
                "ws",
                &permission_registry(Audience::Web),
                PromptChoice::AllowPermanent,
                Audience::Web
            )
            .is_err(),
            "the web authorized a durable grant"
        );
    }

    /// The fence is supplied by the CALLER, so a mismatch is detectable
    /// rather than a tautology.
    #[test]
    fn a_foreign_workspace_cannot_authorize() {
        let (definition, instance, lifecycle) = offer(Audience::Terminal);
        let registry = permission_registry(Audience::Terminal);
        assert!(authorize_action(
            &definition,
            &instance,
            &lifecycle,
            "ws",
            &registry,
            PromptChoice::AllowOnce,
            Audience::Terminal
        )
        .is_ok());
        assert!(
            authorize_action(
                &definition,
                &instance,
                &lifecycle,
                "ws-elsewhere",
                &registry,
                PromptChoice::AllowOnce,
                Audience::Terminal
            )
            .is_err(),
            "a foreign workspace key authorized a decision"
        );
    }

    /// **Q1's answer, pinned.** With `NEWT_WEB_DECISIONS` unset the gate
    /// has no store, so the default terminal path performs NO store write.
    /// The second half is the anti-vacuous twin: the same assertion must
    /// go the other way when a store IS wired, or it would pass by
    /// measuring nothing.
    #[test]
    fn the_default_terminal_path_performs_no_store_write() {
        let (_r, _w, store, conv) = store_and_conv();
        let prompts = Rc::new(Cell::new(0));
        let mut state = PermissionPromptState::default();
        {
            let mut gate = scripted_gate(
                &mut state,
                Caveats::default(),
                None,
                None,
                vec![PromptChoice::AllowOnce],
                Rc::clone(&prompts),
            );
            gate.conversation_id = conv.clone();
            let _ = gate.ask(&[request()]);
        }
        assert_eq!(prompts.get(), 1, "the terminal prompt did not run");
        assert!(
            store.pending_permission_request(&conv).unwrap().is_none(),
            "the DEFAULT terminal path published to the store"
        );

        // Twin: with a store wired, the same read DOES see a row — so the
        // assertion above is measuring something.
        let question = question_for(&request(), &danger::DangerTable::builtin(), Audience::Web);
        store
            .publish_permission_question(&conv, &question, "\"high\"")
            .unwrap();
        assert!(
            store.pending_permission_request(&conv).unwrap().is_some(),
            "the pending read cannot see a published offer, so the check above is vacuous"
        );
    }

    /// A permission resolves exactly once through the controller: the
    /// authorization is `validate_response`'s, and the verdict is
    /// consumable once.
    #[test]
    fn a_permission_resolves_exactly_once_through_the_controller() {
        let (_r, _w, store, conv) = store_and_conv();
        let request_id = publish_low_danger(&store, &conv);

        assert_eq!(
            store
                .answer_permission_action(&conv, &request_id, PromptChoice::AllowOnce)
                .unwrap(),
            AnswerOutcome::Answered
        );
        // A second answer finds it already answered — never a second win.
        assert_eq!(
            store
                .answer_permission_action(&conv, &request_id, PromptChoice::Deny)
                .unwrap(),
            AnswerOutcome::AlreadyResolved
        );
        // The verdict is consumable exactly once.
        assert!(store
            .take_permission_decision(&conv, &request_id)
            .unwrap()
            .is_some());
        assert!(store
            .take_permission_decision(&conv, &request_id)
            .unwrap()
            .is_none());
    }

    /// An action the published form never offered is refused by the store
    /// — now through `validate_response`, not through a decode.
    #[test]
    fn an_undisplayed_action_is_refused_by_the_controller() {
        let (_r, _w, store, conv) = store_and_conv();
        let request_id = publish_low_danger(&store, &conv);
        // `bash` is high danger, so the web form offers allow_once/deny
        // only. A session allow was never displayed.
        assert_eq!(
            store
                .answer_permission_action(&conv, &request_id, PromptChoice::AllowSession)
                .unwrap(),
            AnswerOutcome::InvalidAction
        );
        // ...and the request is still open for a legitimate answer.
        assert_eq!(
            store
                .answer_permission_action(&conv, &request_id, PromptChoice::AllowOnce)
                .unwrap(),
            AnswerOutcome::Answered
        );
    }

    /// **The real race**: two INDEPENDENT connections, one per thread —
    /// the shape newt-web actually produces — released together by a
    /// `Barrier`. Two threads sharing one store would exercise the
    /// in-process `Arc<Mutex<Connection>>` and prove nothing.
    #[test]
    fn separate_connections_racing_one_permission_resolve_once() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let seed = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let conv = seed.create("s", None).unwrap();
        let request_id = publish_low_danger(&seed, &conv);
        drop(seed);

        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for action in [PromptChoice::AllowOnce, PromptChoice::Deny] {
            let root_path = root.path().to_path_buf();
            let ws_path = ws.path().to_path_buf();
            let conv = conv.clone();
            let request_id = request_id.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                // A FRESH store — its own connection, exactly as a web
                // request gets.
                let store = newt_core::ConversationStore::new(root_path, ws_path, 100).unwrap();
                barrier.wait();
                (
                    action,
                    store.answer_permission_action(&conv, &request_id, action),
                )
            }));
        }
        barrier.wait();
        let outcomes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();

        let winners: Vec<PromptChoice> = outcomes
            .iter()
            .filter(|(_, r)| matches!(r, Ok(AnswerOutcome::Answered)))
            .map(|(a, _)| *a)
            .collect();
        assert_eq!(
            winners.len(),
            1,
            "exactly one connection must win; got {outcomes:#?}"
        );
        assert!(
            outcomes
                .iter()
                .any(|(_, r)| matches!(r, Ok(AnswerOutcome::AlreadyResolved))),
            "the loser must be told it lost, not silently succeed: {outcomes:#?}"
        );

        // The loser observes the WINNER's verdict, not its own.
        let reopened = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let observed: PromptChoice = reopened
            .take_permission_decision(&conv, &request_id)
            .unwrap()
            .expect("a verdict was recorded")
            .into();
        assert_eq!(
            observed, winners[0],
            "the recorded verdict is not the winner's"
        );
    }

    /// The loser sees the same terminal fact the winner produced — the
    /// property `web_verdict_and_local_control_resolve_exactly_once`
    /// pins for the local-abort race, stated for the store race.
    #[test]
    fn the_loser_observes_the_winners_verdict() {
        let (_r, _w, store, conv) = store_and_conv();
        let request_id = publish_low_danger(&store, &conv);
        assert_eq!(
            store
                .answer_permission_action(&conv, &request_id, PromptChoice::AllowOnce)
                .unwrap(),
            AnswerOutcome::Answered
        );
        // A later answer loses...
        assert_eq!(
            store
                .answer_permission_action(&conv, &request_id, PromptChoice::Deny)
                .unwrap(),
            AnswerOutcome::AlreadyResolved
        );
        // ...and what everyone reads afterwards is the WINNER's verdict.
        let observed: PromptChoice = store
            .take_permission_decision(&conv, &request_id)
            .unwrap()
            .expect("a verdict")
            .into();
        assert_eq!(observed, PromptChoice::AllowOnce);
    }
}
