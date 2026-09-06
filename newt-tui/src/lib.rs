//! Newt-Agent TUI — a lean chat + agentic-coding TUI in the spirit of Codex /
//! Claude Code, deliberately scoped to *chat and agentic coding* (not as
//! feature-rich). Splash + chat REPL + slash commands + ocap-gated tool use.
//! NOT a settings UI: configuration is plain `~/.newt/config.toml`
//! (see `newt config`). Additional features and the multi-agent matrix live in
//! the downstream `gilamonster-agent`, which inherits these crates.
//!
//! Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 16:53 EDT | Date: 2026-08-12

mod auth_command;
mod brand;
mod chat;
mod codex_env;
mod color;
mod crew_form;
mod help;
mod navigator_cmds;
// `newt auth`'s driver. Re-exported, not moved out of the public API:
// newt-cli calls `newt_tui::run_auth`, so the path must not change.
pub use auth_command::run_auth;
// Only `lib.rs` itself calls these (`resolve_backend_choice`); nothing else in
// the crate does, so this is a private import rather than a re-export.
use codex_env::{codex_env_allowed, codex_env_backend};
mod turn_input;
// `with_live_spill_watch` (both cfg arms) and `with_interrupt_watch` are the
// only two the rest of the crate calls from non-test code: chat.rs,
// commands/model.rs and interrupt_ack_pty_test.rs. Both exist in EVERY config,
// so this needs no cfg. Named, not a glob.
pub(crate) use turn_input::{with_interrupt_watch, with_live_spill_watch};
mod roadmap_cmds;
// Crate-internal consumers of the roadmap family: `chat.rs`'s `/roadmap` arm.
// The test-only names are imported by `lib_tests/skills_integration.rs`
// directly, so no re-export here is dead in a non-test build. Named, not a glob.
pub(crate) use roadmap_cmds::{
    autocapture_commit_after_turn, git_head_short, handle_roadmap_command, roadmap_subcommand_reads,
};
// Danger-tiering for permission grants (facade P1b, §7-F3/F4): pure-data
// classification of a `(capability, target)` grant into a `DangerTier`, read by
// the permission prompt to show a system-computed blast-radius line and refuse a
// plain `[s]ession allow` for high-danger targets.
mod commands;
#[cfg(feature = "live-spill")]
mod completed_spill;
mod danger;
pub mod dgx_probe;
/// #2085 PR-E2: where a non-setting state mutator's journal event is named,
/// and the registry gate that decides whether it may be written at all.
mod event_receipt;
pub mod herdr;
/// #1950: the ONE inline-viewport constructor, and the one answer to "where is
/// the cursor?" when the terminal will not say.
#[cfg(feature = "rich-tui")]
mod inline_viewport;
// C2 (#1876): the RichTUI renderer for one interaction. Rich-only — the pure
// view model it draws lives in `newt_core::interaction_view`, where ratatui is
// not a dependency, so "no renderer in the model" is a compile error.
#[cfg(feature = "rich-tui")]
mod interaction_view;
/// C2 (#1876) — the PTY acceptance proof that the inline INTERACTION frame
/// hands the terminal back on every exit path: clean close, panic, and error
/// return. Same tier and same reasoning as the pager's, and it additionally
/// pins that this surface never enters the alternate screen, which the
/// plain-scroller carve-out does not permit during a turn.
#[cfg(all(test, unix, feature = "rich-tui"))]
mod interaction_view_pty_test;
#[cfg(feature = "live-spill")]
mod live_spill;
/// #1889 — the PTY acceptance proof that the operator PANELS hand the
/// terminal back (cooked mode) on every exit path, panic included. Same tier
/// and same reasoning as `interaction_view_pty_test`, which it is modelled on:
/// a Drop-on-unwind restoration cannot be observed from inside the process
/// doing the unwinding.
#[cfg(all(test, unix, feature = "rich-tui"))]
mod panel_raw_mode_pty_test;
mod permissions;
mod prompt;
/// §6.5 — the PTY regression proof that a permission prompt stays VISIBLE when
/// the harness blocks on a human. Its own file because it needs crate-private
/// access to the production gate + prompt reader, and its own tier because it
/// needs a real terminal to observe a terminal property. See the module docs.
#[cfg(all(test, unix))]
mod prompt_visibility_test;
// #2010: press-time acknowledgement on a real terminal — every Ctrl-C (1st,
// 2nd, Nth) changes the rendered grid WHILE the turn is still blocked. Same
// self-re-exec tier as `prompt_visibility_test`; not `#[ignore]`d, because the
// assertion is structural ("on screen while still running"), not a stopwatch.
#[cfg(all(test, unix))]
mod interrupt_ack_pty_test;
/// #1981: the ONE list of top-level slash commands. Three lists knew this
/// before and none agreed; see the module doc.
/// #1981: `/settings` — the typed form the knob verbs are absorbed into.
mod settings_form;
/// #2002 — the PTY acceptance that WORKS the `/settings` form end to end:
/// menu rendered on the grid, a field picked by number, a value applied, and
/// the receipt read back off disk. Same tier and same reasoning as its
/// siblings above; it grounds the mocked `settings_form` unit tests.
#[cfg(all(test, unix, feature = "rich-tui"))]
mod settings_form_pty_test;
#[cfg(feature = "rich-tui")]
mod shell;
mod slash_registry;
#[cfg(feature = "live-spill")]
mod spill_view;
mod status_topics;
/// #1669 PR-A — the staged tab switch and the one tab-action handler, against
/// live session state. Separate from the pure model so the staging discipline
/// (Stage-0 read, deactivate, hydrate, reset⊕overlay, owner handoff) reads in
/// one place instead of inside `run_chat`.
mod tab_switch;
/// #1669 PR-A — the pure session-tab model. TTY-free and unit-tested on its
/// own; `chat.rs` performs the staged switch and the lifecycle owner handoff
/// this module only reports.
mod tabs;
/// #1677 — the PTY acceptance proof that the transcript pager hands the
/// terminal back (cooked mode + primary screen) on every exit path, including
/// a panic. Same tier and same reasoning as `prompt_visibility_test`: a
/// terminal property can only be observed on a terminal. Rich-only, because
/// the surface it proves exists only there.
#[cfg(all(test, unix, feature = "rich-tui"))]
mod transcript_pager_pty_test;
// #1670: the transcript view. The module splits along the charter boundary —
// the PURE row model (stored turns → styled rows) is always compiled and
// always tested, while the terminal half (alt-screen + raw mode + ratatui
// draw loop) is COMPILE-gated to `rich-tui` inside the module. That gate is
// mechanical, not merely runtime: `ratatui`/`crossterm` are non-optional deps
// of this crate, so without it a lean binary would carry an alt-screen
// surface it can never legitimately enter — the boundary erosion
// `plain_scroller_tui.md` forbids. Dependency direction, deliberately:
// durable conversation model → transcript view model → RichTUI pager.
//
// On LEAN the view model has no consumer (lean answers `/transcript` with the
// existing plain `conversation_show_message` spine), so it is dead code there
// — deliberately. It stays compiled so its pure row/fold/scroll tests run in
// the DEFAULT `cargo test`, the configuration the lean CI lane and most
// contributors actually execute; gating the whole module would silently drop
// that correctness suite from the default gate. The allowance is scoped to the
// lean config alone, so genuinely-unused code in a rich build still fails the
// zero-warnings policy.
#[cfg_attr(not(feature = "rich-tui"), allow(dead_code))]
mod transcript_pager;
mod type_ahead;
// OSC 8 terminal hyperlinks — clickable URLs in modern terminals (issue #771).
mod mcp;
mod mcp_token;
pub mod probe;
pub mod terminal_hyperlink;
mod workspace_state;
// The TTY rich inline input surface (issue #416). Feature-gated so the default
// and headless/wyvern builds never compile it in — newt stays amphibious.
#[cfg(feature = "rich-tui")]
mod rich_input;
/// #1898 — the PTY acceptance proof that a rich TURN hands back BOTH raw mode
/// and bracketed paste on every exit path, panic included. #1411 had recorded
/// this site as safe; it was safe against the error path only.
#[cfg(all(test, unix, feature = "rich-tui"))]
mod rich_input_pty_test;
// The harness config panel (#14) — a severable, TTY-only overlay for the psyche
// operator dials. Gated with the other rich TTY surfaces so wyvern/lean strip it.
#[cfg(feature = "rich-tui")]
mod config_panel;
// The slash-command palette (#1674): pure state + a thin render fn, drawn by
// the rich input surface. Gated with the other rich TTY surfaces so the lean /
// piped / wyvern paths never compile it in.
#[cfg(feature = "rich-tui")]
mod lines_panel;
#[cfg(feature = "rich-tui")]
mod palette;
// One table of named roles, so a colour is chosen once and named everywhere —
// the answer to a census of thirty-odd literals across nine files, including
// `DarkGray` twenty times beside `DarkGrey` thirteen.
#[cfg(feature = "rich-tui")]
mod theme;
// The ONE inline-panel driver — raw mode, viewport, repaint cadence — shared by
// every panel, so a third panel cannot inherit a third copy of the event loop
// `/psyche` and `/backends` were each carrying.
#[cfg(feature = "rich-tui")]
mod panel;
// `/settings` as a chooser (slash_registry's `Disposition::Panel`), over the
// same driver and the same chrome as /psyche and /backends. The typed form
// stays for every surface that has no region to draw in.
#[cfg(feature = "rich-tui")]
mod settings_panel;
// The backend chooser/editor panel (#1667) — same grammar, same gating; its
// persistence rides the setup wizard's crash-safe lock + plan machinery.
#[cfg(feature = "rich-tui")]
mod backend_panel;
// Assembling the backend chooser — its options and its two filesystem writers
// — in ONE place, now that `/backends` is not its only caller.
#[cfg(feature = "rich-tui")]
mod backend_chooser;
#[cfg(feature = "rich-tui")]
mod vi;
// #2005: the Esc / Ctrl-C precedence table (`assets/esc_ladder.toml`) plus the
// key→trigger-name mapping the `precedence-ladder` crate deliberately does not
// own. Gated with the surfaces whose claimants it ranks — palette, vi, cockpit
// presenter — so the lean / piped / wyvern paths never compile it in.
//
// `unix` as well, because the only NON-TEST consumer is the cockpit
// presenter's key loop and `cockpit`'s live half is unix-gated
// (`cockpit/mod.rs:39` — fd capture via `dup2`/`openpty`). Without it the
// Windows build compiles the table and every claim accessor with no caller,
// and `-D warnings` turns four dead-code lints into a failed build. Matching
// the gate to the callers keeps the boundary a compile error when it drifts,
// rather than an `allow` that hides the drift.
#[cfg(all(unix, feature = "rich-tui"))]
mod esc_ladder;
// #2005: the real-PTY acceptance for the rung above — Esc during a running
// turn interrupts from vi NORMAL, and does NOT from vi INSERT or from a
// half-typed operator. NOT `#[ignore]`d: it is the primary per-PR guard, and a
// guard that runs in no lane is decoration.
#[cfg(all(test, unix, feature = "rich-tui"))]
mod esc_ladder_pty_test;
// #1669: the cockpit — the terminal owned by the UI thread for the whole
// session, editor mounted while a turn runs, session output relayed through a
// pty. The live presenter is unix (fd capture via `dup2`/`openpty`); the module
// itself compiles on Windows too so the platform-agnostic scanner (`ansi`) and
// the #1746 ConPTY feasibility probe can live beside it — see
// `docs/decisions/windows_cockpit_conpty.md`.
#[cfg(feature = "rich-tui")]
mod cockpit;
// The opt-in mouse-capture RAII guard + panic-hook release (#1303). Compiled
// only when an interactive surface is on — the wyvern/lean build never links it.
// unix-only: the mouse tier's sole construction site (`with_live_spill_watch`)
// and the raw-byte decoder are `#[cfg(unix)]`, so on Windows the guard would be
// dead code (`-D warnings`). The whole tier rides the unix gate.
#[cfg(all(unix, any(feature = "rich-tui", feature = "live-spill")))]
mod mouse;
#[cfg(all(unix, any(feature = "rich-tui", feature = "live-spill")))]
pub use mouse::install_panic_release_hook;
/// The chrome every modal wears — one themed border for every dialog.
#[cfg(feature = "rich-tui")]
mod modal;
// The lean input surface (issue #527): a dead-simple word-wrapped text box, the
// flight/wyvern morphology. Always built — it is the footer-off / lean tier.
mod lean_input;
/// Cursor + window arithmetic for a list longer than its panel.
#[cfg(feature = "rich-tui")]
mod list_cursor;
/// The model picker: a windowed, arrow-navigated list.
#[cfg(feature = "rich-tui")]
mod models_panel;
// #1669: the channel seam that lets a session's turn stop owning the keyboard.
mod session_worker;
// #1669 PR-B: the tab projection that crosses the surface protocol.
mod setup;
mod setup_tui;
mod tab_bar;
mod wizard;

use anyhow::Context as _;
use mcp::Mcp;
// Step 9.7: the agentic loop (ChatCtx / chat_complete / execute_tool and their
// dependency closure) lives in `newt_core::agentic` now — the TUI is a thin
// wrapper that resolves config + caveats per turn and threads them in.
pub(crate) use brand::{
    brand_active, brand_logo, brand_name, brand_plugins, brand_tagline, logo_for_size, LOGO_20,
    LOGO_PLAIN, NEWT_ORANGE, VERSION,
};
#[cfg(feature = "rich-tui")]
pub(crate) use chat::footer_continues;
use chat::run_chat;
pub(crate) use chat::{InputSurface, ReadOutcome};
pub use color::color_supported;
use color::{color_enabled_for, resolve_color_mode};
#[cfg(any(feature = "rich-tui", test))]
pub(crate) use help::help_lines;
pub use help::render_help;
#[cfg(test)]
use help::{canonical_help_topic, command_help_page};
use help::{print_command_help, render_help_for_tui};
#[cfg(test)]
use newt_core::agentic::newt_line;
use newt_core::agentic::{print_harness_notice, print_newt, ChatCtx, NEWT_ORANGE_CT};
use newt_core::recover_context_window_400;
#[cfg(test)]
use prompt::expand_prompt_tokens;
#[cfg(feature = "rich-tui")]
use prompt::resolve_gutter_setting;
#[cfg(feature = "rich-tui")]
use prompt::rich_surface_selected;
use prompt::{
    current_prompt_and_preview, debug_mode, footer_mode, footer_rich_enabled, prompt_str,
    prompt_token_help, resolve_edit_mode, strip_one_quote_pair, trace_mode, verbose_mode,
};
pub use prompt::{DEFAULT_LEAN_PROMPT, DEFAULT_RICH_PROMPT, PROMPT_TOKENS};
use workspace_state::workspace_state_block;
#[cfg(test)]
use workspace_state::{
    format_workspace_state_block, parse_git_porcelain_dirty_files, WorkspaceStateSnapshot,
};

/// Run the (non-interactive) setup wizard unconditionally — used by `newt init`.
/// Probes Ollama and (re)writes `~/.newt/config.toml`; edit that file for
/// anything else.
mod splash;

pub fn run_init(color: bool) -> anyhow::Result<()> {
    wizard::run_init(color)
}

/// Run the interactive setup wizard — used by `newt setup`. Asks where the
/// model runs (local Ollama or a remote DGX endpoint), probes for installed
/// models, and writes `~/.newt/config.toml` after a preview + confirmation.
pub fn run_setup(color: bool) -> anyhow::Result<()> {
    setup::run(color)
}

pub async fn run_setup_target_with_model(
    target: &str,
    token_env: Option<&str>,
    token_file: Option<&std::path::Path>,
    model: Option<&str>,
    yes: bool,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    setup::run_target(target, token_env, token_file, model, yes, config_path).await
}

/// Run the interactive crew-settings form — used by `newt crew --edit [name]`
/// and the in-session `/crew edit`. Prompts field-by-field (planner/navigator/
/// triage loadouts, control loop, test command, budgets), previews, and writes
/// `~/.newt/crews/<name>.toml`. A cooked-terminal prompt/response form (NOT a
/// ratatui surface — `docs/decisions/plain_scroller_tui.md`).
pub fn run_crew_edit(name: Option<&str>, color: bool) -> anyhow::Result<()> {
    crew_form::run_edit(name, color)
}

/// [`run_crew_edit`], asking through the session's surface seam (#1862 C1)
/// rather than this thread's terminal. `/crew edit` uses this; the `newt crew
/// edit` CLI, which owns the terminal, uses [`run_crew_edit`].
///
/// # Errors
///
/// Propagates a crew write failure.
pub(crate) fn run_crew_edit_with_ask(
    name: Option<&str>,
    color: bool,
    ask: SlashAsk<'_>,
) -> anyhow::Result<()> {
    crew_form::run_edit_with_ask(name, color, ask)
}

/// Open the harness config panel (#14) for the psyche operator dials and return
/// its [`config_panel::PanelOutcome`], or — when stdout is not a TTY (piped /
/// headless) — print a short note pointing at the text `/psyche` view and return
/// `Cancelled`. The panel applies (only the changed) dials through the same
/// setters the flags / slash commands use; `persist` (the caller's closure, which
/// owns the `PersonaStore`) is the only filesystem I/O the panel's TRANSACTION
/// depends on, so a failed save keeps the panel open without mutating the
/// runtime (review-3 §1). Not the only I/O it performs any more: applying a dial
/// now appends a settings receipt (#1965), which is best-effort by construction
/// — `settings_receipt::record` swallows its own failures, because failing to
/// observe a change must never undo it. The caller acts on the
/// returned outcome — applying the persona action, rerouting, and reporting from
/// fresh runtime state. **Rich-tui only** — the lean build has no ratatui surface,
/// so the `/psyche edit` handler prints the fallback directly. See
/// `harness_config_panel.md`.
#[cfg(feature = "rich-tui")]
pub(crate) fn run_psyche_panel(
    seed: config_panel::PanelSeed,
    persist: impl FnMut(&str, &str, bool) -> config_panel::SaveResult,
    color: bool,
    verbose: bool,
) -> config_panel::PanelOutcome {
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        match config_panel::run(seed, persist) {
            Ok(outcome) => outcome,
            Err(e) => {
                print_newt(&format!("psyche panel error: {e}"), color, verbose);
                config_panel::PanelOutcome::Cancelled
            }
        }
    } else {
        // rich-tui compiled but stdout is not a TTY (piped / headless): no overlay.
        print_newt(
            "the psyche panel needs an interactive rich terminal — use /psyche status \
             for the text view, or /psyche cognition / /psyche tenacity <level> to \
             change the dials.",
            color,
            verbose,
        );
        config_panel::PanelOutcome::Cancelled
    }
}

use std::io::{self, IsTerminal, Write as _};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    execute,
    style::{Color as CtColor, Print, ResetColor, SetForegroundColor},
    terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use newt_core::tty::raw_mode::RawModeGuard;

/// The PRODUCTION half of a source file, for the structural guards that assert
/// "no other path exists" (#1889, #1898).
///
/// Those tests live IN the file they read, so `include_str!` pulls in their own
/// needles; cutting at the test module is both the fix and the right scope.
///
/// Two things this gets right that the first copy did not, both found by
/// `rich_input.rs`:
///
/// * It cuts at the test MODULE, not at the first `#[cfg(test)]` anywhere.
///   `rich_input.rs` has an inline `#[cfg(test)]` item at :360, ~700 lines
///   above the code under test, so a first-occurrence split returned a prefix
///   that contained none of it.
/// * It PANICS when the marker is missing instead of returning "". An empty
///   string satisfies every `count() == 0` assertion, so the failure mode of
///   the convenient version is a guard that passes because it read nothing.
///
/// Gated with its callers, not merely with `test`: both structural guards live
/// in `rich-tui` modules, so under the LEAN configuration this function has no
/// caller and `-D warnings` refuses it. Caught by the lean clippy gate added in
/// #1890 — the configuration that had no gate at all a day ago.
#[cfg(all(test, feature = "rich-tui"))]
pub(crate) fn production_source(src: &str) -> &str {
    src.split("\n#[cfg(test)]\nmod tests {")
        .next()
        .filter(|prefix| prefix.len() < src.len())
        .expect(
            "the file must end in an unindented `#[cfg(test)] mod tests {` — \
             without the marker this helper would hand back an empty string, \
             and every count-based assertion would pass having read nothing",
        )
}

/// Runs `restore` on every exit path of the scope holding it — normal return,
/// `?`, or panic. Split out from [`SplashScreenGuard`] so the "it always runs"
/// property is unit-testable without owning a real terminal: the guard proper
/// closes over crossterm calls, this closes over anything.
struct RestoreOnDrop<F: FnMut()> {
    restore: F,
}

impl<F: FnMut()> Drop for RestoreOnDrop<F> {
    fn drop(&mut self) {
        (self.restore)();
    }
}

/// RAII owner of the splash's terminal state: raw mode, the alternate screen,
/// and cursor visibility.
///
/// **#1411.** The splash used to enable raw mode and restore it ~30 lines
/// later, with *three* fallible `?` operators in between: the `execute!` that
/// enters the alternate screen and `show_splash`. Any I/O
/// error in that window returned early past the restore and left the operator
/// in the alternate screen, in raw mode, with the cursor hidden — a terminal
/// that echoes nothing and shows no cursor, recoverable only via `reset`. A
/// panic did the same. Of the three crossterm raw-mode pairs in this crate this
/// was the only one with no guard at all (`lean_input::RawGuard` has one;
/// `rich_input::read_turn` avoids `?` around its event loop).
///
/// **CORRECTION (#1898).** That last clearance was wrong, and it stood for
/// long enough to matter. "Avoids `?` around its event loop" is true and
/// covers the ERROR path only — a PANIC inside `event_loop` unwound past both
/// `DisableBracketedPaste` and `disable_raw_mode()`. `read_turn` now owns
/// `rich_input::RawPasteGuard`, which restores both from `Drop`.
///
/// The lesson is about this comment, not that function: a site recorded as
/// SAFE is worse than one nobody has looked at, because nobody looks twice.
/// The audit that found it (#1897) re-read every raw-mode pair in the
/// workspace rather than trusting this list. As of #1898 every pair is
/// Drop-guarded: `SplashScreenGuard`, `lean_input::RawGuard`,
/// `newt_core::tty::modal::RawGuard`,
/// `transcript_pager::AltScreenGuard`, `interaction_view::InlineGuard`,
/// `config_panel::PanelRawGuard` (both panels, #1889),
/// `rich_input::RawPasteGuard`, and `cockpit/presenter` via `RestoreOnDrop`.
///
/// Per the repo's "make the bug unrepresentable" rule, the fix is ownership
/// rather than another restore call: the terminal cannot be taken without
/// binding something that gives it back.
///
/// **This deliberately adds a fourth mode guard** to a crate whose #1408 story
/// C1 is *consolidating* mode ownership into one nesting-aware owner. That is
/// intentional sequencing, not an oversight: C1 needs the lean firewall
/// (#1409) under it first, and leaving a reachable terminal-corrupting bug
/// parked until then is the worse trade. This type is written to be absorbed —
/// enter/restore is exactly the shape C1 needs.
struct SplashScreenGuard {
    /// **Composed onto the one nesting-aware owner (#1905).** The splash owns
    /// the alternate screen and cursor visibility; raw mode is
    /// [`RawModeGuard`]'s. Its own doc predicted this: *"This type is written
    /// to be absorbed — enter/restore is exactly the shape C1 needs."*
    ///
    /// DECLARATION ORDER IS THE CONTRACT. Rust drops fields in declaration
    /// order, after the struct's own `Drop::drop` — and this type has none, so
    /// `_restore` runs first and `_raw` second. Screen and cursor come back
    /// before line discipline does, which is the order this guard already had
    /// when both lived in one closure. Swapping these two lines would invert
    /// it silently.
    _restore: RestoreOnDrop<fn()>,
    /// **The whole screen, held (#1980).** Declared AFTER `_restore` on
    /// purpose: this type has no `Drop::drop`, so fields are the only ordering
    /// there is, and the rows must come back only once the alternate screen has
    /// been left. Declared BEFORE `_raw` for the reason the comment above
    /// gives — screen state before line discipline.
    ///
    /// `SuspendHolder`: entering the alternate screen genuinely suspends what
    /// is beneath it, the terminal preserves and restores the primary screen,
    /// and the policy therefore never refuses. No new failure path here; what
    /// changes is that a surface minting `Refuse` or `Shift` can now see that
    /// the screen is taken.
    _region: newt_core::tty::RegionLease,
    _raw: RawModeGuard,
}

impl SplashScreenGuard {
    /// Take the terminal: raw mode, then the alternate screen on a cleared
    /// frame with the cursor hidden.
    ///
    /// The guard is bound *before* the fallible `execute!`, so a failure
    /// entering the alternate screen still gives raw mode back — that path was
    /// itself one of the three leaks.
    fn enter() -> io::Result<Self> {
        let raw = RawModeGuard::enter()?;
        let region = newt_core::tty::Terminal::lease_region(
            newt_core::tty::Region::WholeScreen,
            newt_core::tty::OnCollision::SuspendHolder,
        )
        .ok_or_else(|| io::Error::other("the screen could not be leased"))?;
        let guard = Self {
            // Raw mode is NOT in this closure any more — `_raw` owns it. What
            // remains is exactly the splash's own state.
            _restore: RestoreOnDrop {
                restore: || {
                    let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
                },
            },
            _region: region,
            _raw: raw,
        };
        // Flush the tty input queue on taking the terminal. This stays HERE and
        // must never move into `RawModeGuard` (#1905): it is not part of raw
        // mode, and every frame silently eating pending input would be a new
        // bug wearing a refactor's clothes. A slow pre-splash step
        // — notably the first-run summarizer model pull (#661 group C), which runs
        // for seconds in cooked mode — lets impatient keystrokes or echoed bytes
        // queue up; entering raw mode does NOT discard them, so the splash's first
        // input poll would consume one (a lone Esc / q / Ctrl-C reads as quit) and
        // newt would exit before the splash is ever seen. TCIFLUSH drops the
        // pending input atomically — no poll/read drain race.
        #[cfg(unix)]
        // SAFETY: tcflush on the stdin tty fd passes no pointers and touches no
        // memory; the worst case on a non-tty fd is a harmless ENOTTY.
        unsafe {
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
        }
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            Hide,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        Ok(guard)
    }
}

/// #1411 regression cover for the splash's terminal-restore contract.
///
/// These drive [`RestoreOnDrop`] rather than a real terminal, deliberately:
/// the defect was never "the restore call is wrong", it was "the restore call
/// is *skipped* on some exit paths". That is a control-flow property, and
/// control flow is exactly what a fully-mocked unit test can prove. The
/// end-to-end proof that the restored terminal actually echoes again is a
/// real-PTY concern and lands with the shared harness in #1410.
///
/// Each test below mirrors one of the three `?` operators that used to sit
/// between `enable_raw_mode()` and `disable_raw_mode()`; the pre-#1411 shape —
/// a bare `disable_raw_mode()` call at the bottom of the block — fails all of
/// them, because on those paths control never reaches the bottom.
#[cfg(test)]
#[path = "lib_tests/splash_guard_tests.rs"]
mod splash_guard_tests;

#[cfg(test)]
#[path = "lib_tests/slash_ask_seam_tests.rs"]
mod slash_ask_seam_tests;

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

pub fn run_code(
    path: Option<&std::path::Path>,
    no_splash: bool,
    persona: Option<&str>,
    // FR-5 (#999): the session operating altitude from `--altitude` (doer vs
    // coach). `None` ⇒ each persona's own altitude applies (doer when unset).
    // When set it overrides a loaded persona's altitude, or — with no `--persona`
    // — synthesizes a minimal altitude-only persona. It rides on `active_persona`
    // so no session-management helper needs new plumbing.
    altitude: Option<newt_core::Altitude>,
    // #479 part 2: the crew/team runner, BUILT BY THE BINARY (newt-cli owns
    // newt-scheduler + the worktree) and injected down — newt-tui stays
    // scheduler-free. `None` ⇒ the `/team` tools are never advertised.
    crew_runner: Option<&dyn newt_core::agentic::CrewRunner>,
    // First-run provisioning (#985): when the binary needs to fetch the on-host
    // summarizer model, the splash covers that work with a spinner. `None` ⇒ no
    // setup step. The download runs on a background thread; see `SetupHandle`.
    setup: Option<SetupHandle>,
) -> anyhow::Result<()> {
    // Color: NEWT_COLOR (--color/--mono) > NO_COLOR/TERM=dumb > [tui] color > auto
    // (issue #527). Config is folded in here, where a session reads it; the
    // `color_supported_with` shim (used pre-config, e.g. the wizard) stays Auto.
    let cfg_color = newt_core::Config::resolve()
        .ok()
        .and_then(|c| c.tui)
        .map(|t| t.color)
        .unwrap_or_default();
    let color = color_enabled_for(
        resolve_color_mode(&|k| std::env::var(k).ok(), cfg_color),
        io::stdout().is_terminal(),
    );

    let workspace = resolve_workspace(path);

    // `no_splash` is already resolved by the caller (CLI flags + config).
    let inline = no_splash;

    if !inline {
        // SPLASH FIRST — it covers the initialization work, and the first-run
        // wizard's menus roll underneath (normal scrollback) after it drops.
        // #1411: the guard owns raw mode + the alternate screen + the cursor for
        // this whole block, so the `?`s below cannot strand the terminal.
        let screen = SplashScreenGuard::enter()?;
        let mut stdout = io::stdout();
        // Background work starts AT splash entry: a configured box pre-warms
        // its backend probe while the logo shows; run_chat consumes the result
        // when (and only when) the resolved choice still matches.
        let prewarm = spawn_backend_prewarm();
        // The unboxed state names the splash context ("initial setup") —
        // computed before `setup` is consumed below.
        let unconfigured = newt_core::Config::resolve()
            .map(|c| c.is_unconfigured())
            .unwrap_or(true);
        let context = if unconfigured || setup.is_some() {
            "initial setup"
        } else {
            "starting"
        };
        // ONE splash covers everything (field note: two consecutive splash
        // screens read as a bug): a first-run provision (#985) renders as an
        // extra spinner line on this same screen — input blocked except a
        // triple abort while it runs — and the splash's own spinner keeps a
        // launch from ever looking hung.
        let status = if prewarm.is_some() {
            "warming up backend…"
        } else {
            "initializing…"
        };
        let cont = splash::show_splash(&mut stdout, &workspace, color, status, context, setup)?;
        // Give the terminal back before anything else prints: chat must not run
        // inside the alternate screen. Explicit rather than implicit at the end
        // of the block so the ordering stays visible to a reader.
        drop(screen);
        if !cont {
            if let Some(pw) = prewarm {
                pw.handle.abort();
            }
            return Ok(());
        }
        // The branded crawl header opens the post-splash scrollback (the seam
        // inheriting agents override — see brand::crawl_header), then the
        // first-run wizard's menus roll underneath it when unconfigured.
        if unconfigured {
            print!("{}", brand::crawl_header(Some("initial setup")));
        }
        wizard::maybe_run(color)?;
        print_inline_header(&workspace, color);
        return run_chat(&workspace, color, persona, altitude, crew_runner, prewarm);
    }

    // No-splash paths keep the pre-splash order exactly (wizard first): CI,
    // piped, and --no-splash launches see no behavior change.
    wizard::maybe_run(color)?;
    if let Some(setup) = setup {
        // No-splash (#985): the header still shows, then an inline setup spinner
        // covers the provisioning before chat starts.
        print_inline_header(&workspace, color);
        run_setup_inline(&setup);
        return run_chat(&workspace, color, persona, altitude, crew_runner, None);
    }

    print_inline_header(&workspace, color);
    run_chat(&workspace, color, persona, altitude, crew_runner, None)
}

/// A backend probe started at splash entry so a configured box's session
/// start finds the answer already in flight (or done) instead of probing
/// cold after the splash.
pub(crate) struct Prewarm {
    /// The URL the probe ran against — consumption is gated on it still
    /// matching the resolved choice (a first-run wizard may have changed
    /// everything in between).
    pub(crate) url: String,
    pub(crate) handle:
        tokio::task::JoinHandle<Option<newt_core::backend_probe::EndpointProbeResult>>,
}

/// True when a pre-warmed probe is for the same endpoint the session
/// resolved — trailing-slash-insensitive. Pure.
pub(crate) fn prewarm_applies(choice_url: &str, prewarm_url: &str) -> bool {
    choice_url.trim_end_matches('/') == prewarm_url.trim_end_matches('/')
}

/// Whole-request bound for the session's endpoint probes. LAN boxes answer in
/// well under a second; a hosted HTTPS gateway needs DNS + TLS + auth and
/// real-world jitter — field testing showed api.openai.com blowing a 3 s
/// bound while being perfectly healthy, so the remote beat is a patient 10 s.
pub(crate) fn probe_timeout_secs(url: &str) -> u64 {
    if url.starts_with("https://") {
        10
    } else {
        1
    }
}

/// Start the pre-warm probe for the resolved backend choice, if this box is
/// configured (an unconfigured box has nothing real to probe — the wizard is
/// about to change everything) and a tokio runtime is available.
fn spawn_backend_prewarm() -> Option<Prewarm> {
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let cfg = newt_core::Config::resolve_runtime_unpublished().ok()?;
    if cfg.is_unconfigured() {
        return None;
    }
    let choice = resolve_backend_choice(&cfg).ok()?;
    let url = choice.url.clone();
    let api_key = choice.api_key.clone();
    let needs_probe = choice.kind_needs_probe;
    let kind = choice.kind;
    let secs = probe_timeout_secs(&url);
    let task_url = url.clone();
    let handle = runtime.spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(secs))
            .build()
            .ok()?;
        if needs_probe {
            newt_core::backend_probe::detect_endpoint(&client, &task_url, api_key.as_deref())
                .await
                .ok()
        } else {
            // Same shape adopt_backend_choice's known-kind path fetches cold.
            let api = newt_core::backend_probe::api_for(kind);
            let models = api
                .list_models(&client, &task_url, api_key.as_deref())
                .await
                .ok()?;
            let warm = api
                .warm_models(&client, &task_url, api_key.as_deref())
                .await
                .unwrap_or_default();
            Some(newt_core::backend_probe::EndpointProbeResult {
                endpoint: task_url.trim_end_matches('/').to_string(),
                serving: api.serving(models.len()),
                kind,
                models,
                warm,
                engine: None,
            })
        }
    });
    Some(Prewarm { url, handle })
}

/// Compact inline header — the session preamble.
/// Prints LOGO_20 with text to the right using ANSI column-move escapes,
/// then scrolls naturally into history. No alt screen, no raw mode.
/// Printed in BOTH splash and no-splash modes (see `run_code`).
fn print_inline_header(workspace: &str, color: bool) {
    print!("{}", render_inline_header(workspace, color));
}

/// Render the inline header to a string. Pure — unit-testable.
fn render_inline_header(workspace: &str, color: bool) -> String {
    use std::fmt::Write as _;

    if !color {
        let plugins = brand_plugins()
            .map(|p| format!("  ·  {p}"))
            .unwrap_or_default();
        return format!("{} v{VERSION}  ·  {workspace}{plugins}\n\n", brand_name());
    }

    // Place text at column 23 (just past the 20-col logo).
    let text_col = 23u16;
    let logo = brand_logo(LOGO_20, "ansi-20");
    let logo_lines: Vec<&str> = logo.lines().collect();
    let n = logo_lines.len();

    // Text lines aligned to the middle-right of the logo.
    let mid = n / 2;
    let header = format!("{}  ·  {}", brand_name(), brand_tagline());
    let plugins = brand_plugins();
    let version = format!("v{VERSION}");
    let text: &[(&str, bool)] = &[
        (header.as_str(), false),
        (version.as_str(), true),                 // dim
        (plugins.as_deref().unwrap_or(""), true), // dim; empty row hides itself
        (
            "ready — type a task, /help for commands, /exit to quit",
            true,
        ),
        ("keybindings — /vi (default) · /emacs · /nano", true),
    ];
    let text_start = mid.saturating_sub(1);

    let mut out = String::new();
    for (i, logo_line) in logo_lines.iter().enumerate() {
        // Logo line already contains ANSI color codes.
        out.push_str(logo_line);
        // Move cursor to column text_col on this row, print text if scheduled.
        let ti = i.wrapping_sub(text_start);
        if let Some((msg, dim)) = text.get(ti) {
            if !msg.is_empty() {
                // \x1b[{col}G moves cursor to absolute column (1-indexed).
                let style_on = if *dim { "\x1b[38;2;100;100;100m" } else { "" };
                let style_off = if *dim { "\x1b[0m" } else { "" };
                let _ = write!(out, "\x1b[{text_col}G{style_on}{msg}{style_off}");
            }
        }
        out.push('\n');
    }
    out.push('\n');
    out
}

pub fn run_pilot(_flight_id: &str) -> anyhow::Result<()> {
    anyhow::bail!("newt-tui::run_pilot not yet implemented")
}

// The binary (newt-cli) provisions the on-host summarizer model on a background
// The first-run setup / provisioning screen (`SetupEvent`, `SetupHandle`,
// the spinner render loop) lives in `setup_tui.rs` — the UI of the setup step.
// (`setup.rs` is the config wizard logic; `wizard.rs` is the silent prober.)
pub use crate::permissions::ocap_high_danger_predicate;
#[cfg(unix)]
use crate::permissions::try_watch_stdin;
use crate::permissions::{
    permission_prompting_configured, production_danger_table, prompt_permission_choice,
    should_prompt_permissions, PermissionPromptState, PromptChoice, PromptPermissionGate,
};
use crate::setup_tui::run_setup_inline;
pub use crate::setup_tui::{SetupEvent, SetupHandle};

// ---------------------------------------------------------------------------
// Chat — plain terminal REPL
//
// No alternate screen, no custom scroll. The terminal's own scrollback
// buffer handles history. Works identically over SSH and inside tmux.
//
// This is a standing design decision, not a stopgap — newt is amphibious
// (human CLI + headless swarm) and the chat surface stays a plain scroller.
// Advanced TUI belongs in gilamonster-agent / monitor repos. Before adding
// any screen control here, read docs/decisions/plain_scroller_tui.md.
//
// NEWT_CHAT_STYLE=verbose  — show "newt" / "you" labels before the caret
// (default is compact: just the colored caret / symbol)
// ---------------------------------------------------------------------------

/// Return today's date as `YYYY-MM-DD` using the system clock.
pub(crate) fn today_date() -> String {
    // Using std::time for a lightweight date without a chrono dep.
    // We only need YYYY-MM-DD so we derive it from epoch seconds manually.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since Unix epoch.
    let days = secs / 86400;
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

// ---------------------------------------------------------------------------
// File-descriptor hygiene — prevent EMFILE from killing the input surface
// ---------------------------------------------------------------------------

/// Mark every open fd above stderr (`fd > 2`) as `O_CLOEXEC` so that
/// subprocesses spawned by `run_command` (via agent-bridle / brush) do NOT
/// inherit the parent's terminal fd, history file handle, or socket fds.
///
/// Call this **after** the input surface and history are initialised so that
/// their fds are also marked. Safe to call multiple times — already-CLOEXEC fds
/// are skipped.
///
/// macOS returns `LONG_MAX` for `sysconf(_SC_OPEN_MAX)`; we cap the sweep
/// at 4096 which covers any realistic fd table.
#[cfg(unix)]
fn mark_fds_cloexec() {
    let max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let max_fd = if max > 0 {
        max.min(4096) as libc::c_int
    } else {
        256
    };
    for fd in 3..max_fd {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && (flags & libc::FD_CLOEXEC == 0) {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// Probe whether the fd table has at least one free slot by attempting to
/// open the platform null device. Returns `false` when the process is at EMFILE — i.e.,
/// the next `open("/dev/tty")` by the input surface would fail and panic.
///
/// Uses only `std::fs` (no libc dep) so it compiles on all platforms.
fn terminal_fd_available() -> bool {
    terminal_fd_available_from_probe(|| std::fs::File::open(null_device_path()).map(drop))
}

fn terminal_fd_available_from_probe(probe: impl FnOnce() -> std::io::Result<()>) -> bool {
    probe().is_ok()
}

#[cfg(windows)]
fn null_device_path() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn null_device_path() -> &'static str {
    "/dev/null"
}

/// Collect the basenames of all executables in the CLI-granted directories
/// (`--venv` and `--exec-path`). Used to widen the exec caveat at session start
/// so the agent can run tools from those directories without needing them in
/// `[tui.permissions] extra_exec` in the config file.
#[cfg(unix)]
fn scan_cli_exec_grants() -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(venv) = std::env::var("NEWT_VENV") {
        dirs.push(format!("{venv}/bin"));
    }
    if let Ok(paths) = std::env::var("NEWT_EXEC_PATHS") {
        for p in paths.split(':') {
            if !p.is_empty() {
                dirs.push(p.to_string());
            }
        }
    }
    dirs.iter()
        .flat_map(|dir| std::fs::read_dir(dir).ok().into_iter().flatten())
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.is_file() {
                return None;
            }
            let mode = path.metadata().ok()?.permissions().mode();
            if mode & 0o111 == 0 {
                return None;
            }
            path.file_name()?.to_str().map(str::to_string)
        })
        .collect()
}

#[cfg(not(unix))]
fn scan_cli_exec_grants() -> Vec<String> {
    Vec::new()
}

fn runtime_authority_note() -> Option<&'static str> {
    match (
        newt_core::agentic::ocap_disabled(),
        newt_core::agentic::full_access_requested(),
    ) {
        (true, true) => Some(
            "Runtime authority: --disable-ocap/--yolo AND --full-access are active. \
             run_command is available with unrestricted exec authority and uses the \
             unconfined host shell; the configured [tui.permissions] preset and its \
             exec floor are lifted. A posture, persona, or operating mode may still \
             attenuate that session baseline. \
             Do not claim a capability wall without first calling run_command and \
             grounding the claim in its returned tool result. Native fs tools are \
             unrestricted; web_fetch has unrestricted configured net authority.",
        ),
        (true, false) => Some(
            "Runtime authority: --disable-ocap/--yolo is active. run_command uses the \
             unconfined host shell when the active exec floor permits it, not the \
             brush/agent-bridle confined shell. Do not claim run_command is unavailable \
             due to brush in this mode. Native fs tools remain workspace-fenced; \
             web_fetch remains net-leashed.",
        ),
        (false, true) => Some(
            "Runtime authority: --full-access is active. run_command is available with \
             unrestricted exec authority; the configured permission preset and exec \
             floor are lifted for this invocation, though a posture, persona, or \
             operating mode may still attenuate that session baseline. Do not claim a capability wall \
             without first calling run_command and grounding the claim in its returned \
             tool result. Native fs tools and configured net authority are unrestricted.",
        ),
        (false, false) => None,
    }
}

fn filesystem_authority_note() -> &'static str {
    if newt_core::agentic::full_access_requested() {
        "# Filesystem authority\n\
         The full-access session baseline permits reads and writes outside the \
         workspace. A posture, persona, or operating mode may still attenuate it. \
         Treat an actual fs_read/fs_write tool denial as authoritative; do not \
         infer confinement without a returned tool result.\n"
    } else {
        "# Filesystem confinement\n\
         You are confined to the workspace (the current directory) plus any paths \
         the operator explicitly opened. A read or write outside that returns \
         `capability denied: fs_read/fs_write does not permit '<path>'`. Do NOT \
         retry a denied path or try to work around it — instead tell the operator \
         it's outside your workspace and that they can relaunch with \
         `--read <path>` (read-only) or `--write <path>` (read+write) to grant it.\n"
    }
}

/// The ambient "environment" the agent can see — its own model name, the
/// harness + version, the backend it's talking to, and the current date/time.
/// Prepended to the system prompt each turn so these are *current* (the system
/// prompt itself is frozen at conversation start). Without it the model has no
/// way to know its identity and confabulates one (e.g. inventing a name for
/// commit attribution). Kept short — it rides in every request.
fn runtime_context_block(
    model: &str,
    endpoint: &str,
    kind: newt_core::BackendKind,
    identity: &newt_core::AgentIdentity,
) -> String {
    let now = chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S %Z")
        .to_string();
    let backend = match kind {
        newt_core::BackendKind::Openai => "openai-compatible",
        newt_core::BackendKind::Anthropic => "anthropic",
        _ => "ollama",
    };
    let author_email = identity.email.as_str();
    let author_name = identity.name.as_str();
    let harness = newt_core::build_info::harness_name();
    let runtime_authority = runtime_authority_note()
        .map(|note| format!("# Runtime authority\n{note}\n"))
        .unwrap_or_default();
    let filesystem_authority = filesystem_authority_note();
    format!(
        "# Environment (refreshed every turn)\n\
         Harness: newt-agent v{VERSION}\n\
         Model: {model}\n\
         Backend: {backend} @ {endpoint}\n\
         Current date/time: {now}\n\
         You are the model named above, running under the newt-agent harness. \
         When asked who or what you are — and when attributing work (commit \
         trailers, git notes, PR text) — use this real model name and harness; \
         never invent or guess an identity.\n\
         {runtime_authority}\
         # Git commit identity\n\
         Prefer the `git` tool: it commits as `{author_name} <{author_email}>` and \
         auto-signs `Co-authored-by: {model} ({harness} v<version> <build>) <{author_email}>` for \
         every model/harness that materially contributed since the last commit \
         (not just this one) — do NOT add that trailer yourself, just write the \
         plain message; for the last commit use op=amend (don't claim to amend \
         without calling it).\n\
         If you instead commit with the SHELL `git` command (run_command), you \
         MUST set the same identity explicitly — the email is what attributes the \
         commit to the harness account on GitHub. Use:\n\
         `git -c user.name='{author_name}' -c user.email='{author_email}' commit -m \"…\"`\n\
         (the author name may be `{author_name}` or this model's name, but the \
         email must always be `{author_email}`). Never commit with a guessed or \
         personal email. The shell path bypasses the harness entirely, so it \
         gets NO automatic Co-authored-by credit — prefer the `git` tool \
         whenever multi-contributor attribution matters.\n\
         {filesystem_authority}"
    )
}

/// Read-only enforcement caveats (read anything; no write/exec/net) — the safe
/// default when nothing is configured or identity setup fails.
fn read_only_caveats(workspace: &str) -> newt_core::caveats::Caveats {
    newt_core::ToolPermissions {
        preset: newt_core::PermissionPreset::ReadOnly,
        extra_exec: Vec::new(),
        net: Vec::new(),
        prompt: false,
    }
    .to_caveats(workspace)
}

/// Lower the configured TUI permission policy to a `Caveats` value. With no
/// `[tui]` config the policy is **read-only** — never `Caveats::top()`. Pure in
/// its inputs, so the safe-default behavior is unit-testable.
///
/// CLI exec grants (`--venv`, `--exec-path`) are injected here: they widen the
/// exec scope at session-start so the agent can run tools in those directories
/// without requiring per-binary `extra_exec` entries in the config file. Only
/// `Scope::Only` is widened; `Scope::All` (FullAccess) is already unrestricted.
///
/// The agent's reads are **locked to the workspace by default**: newt's presets
/// ship `fs_read = All` (read anything anywhere), but the operator wants the
/// agent confined to the CWD unless a path is explicitly opened. So `policy_for`
/// fences `fs_read` to `workspace + the granted paths` for every preset EXCEPT
/// `full_access` (which is `fs_write = All` — an explicit "no fence" choice).
/// `--read <path>` adds a read path; `--write <path>` adds a read+write path
/// (write implies read) and is also widened into the already-fenced `fs_write`.
fn policy_for(tui: Option<newt_core::TuiConfig>, workspace: &str) -> newt_core::caveats::Caveats {
    use newt_core::caveats::Scope;
    // --full-access / NEWT_FULL_ACCESS=1: per-invocation preset override —
    // build the policy from `full_access` (`Caveats::top()`, the exact value
    // `ToolPermissions::to_caveats` yields for that preset) regardless of the
    // configured preset. This also empties the #774 exec floor (`Scope::All`
    // + no mode ⇒ `exec_floor_from` → None), so combined with --yolo the
    // host-shell bypass covers every command. Surfaced loudly at session
    // start by `full_access_banner`.
    let mut caveats = if newt_core::agentic::full_access_requested() {
        newt_core::caveats::Caveats::top()
    } else {
        tui.map(|t| t.permissions.to_caveats(workspace))
            .unwrap_or_else(|| read_only_caveats(workspace))
    };
    let extra = scan_cli_exec_grants();
    if !extra.is_empty() {
        if let Scope::Only(ref mut set) = caveats.exec {
            set.extend(extra);
        }
    }
    // Opt out of the read-fence ONLY for a preset whose reads AND writes are
    // BOTH deliberately unbounded — i.e. `full_access` (`fs_read == All &&
    // fs_write == All`). Keying on both axes (not `fs_write` alone) makes the
    // read-axis intent explicit: `read_only` is `fs_read == All, fs_write ==
    // none`, and its broad read scope is fenced ON PURPOSE here, not as an
    // accident of the discriminator. That is deliberate — `read_only` is the
    // DEFAULT preset for an unconfigured session, and confining the default is
    // the entire point of the lock (an unconfigured agent must not read outside
    // the CWD). `read_only` still reads broadly *within* the workspace and any
    // `--read`/`--write` grants. Every other preset is locked to the workspace +
    // the grants by the shared newt-core helper (the headless crew/worker paths
    // call the same helper).
    let reads_deliberately_unbounded =
        matches!(caveats.fs_read, Scope::All) && matches!(caveats.fs_write, Scope::All);
    if !reads_deliberately_unbounded {
        newt_core::caveats::apply_cli_fs_grants(&mut caveats, workspace);
    }
    caveats
}

/// Resolve the configured `[tui]` block, if any.
fn resolve_tui(cfg: &newt_core::Config) -> Option<newt_core::TuiConfig> {
    cfg.tui.clone()
}

/// Mint a signed operating key for `policy`, rooted in the per-user key at
/// `key_path` (generated on first use). Its caveats are provably `⊑` the user's
/// full authority, and it can only ever delegate *narrower* children.
pub(crate) fn mint_operating_key(
    key_path: &std::path::Path,
    policy: &newt_core::caveats::Caveats,
) -> Result<newt_identity::AgentKey, newt_identity::IdentityError> {
    let user = newt_identity::load_or_generate(key_path)?;
    let root = newt_identity::session_root(&user);
    newt_identity::attenuate(&root, policy)
}

/// The session's signed operating capability.
///
/// Established once from the per-user key (`~/.newt/identity.pem`) and the
/// configured preset, it enforces **in-session monotonic narrowing**:
/// re-applying a policy (e.g. after a config reload) can only ever *narrow* the live
/// authority, never widen it — widening would require re-rooting from the user
/// key, which only happens on a fresh session. The running agent can tighten its
/// own leash but never loosen it.
///
/// Safe by default: an absent config, or any identity error, yields read-only —
/// never `Caveats::top()`. The one exception is the explicit per-invocation
/// `--full-access` / `NEWT_FULL_ACCESS=1` preset override (see `policy_for`),
/// which is operator-asserted per run and loudly surfaced at session start.
struct SessionCapability {
    /// The live operating key. `None` if the per-user key is unavailable; the
    /// capability then degrades to a plain caveats floor (still narrowing-only).
    op: Option<newt_identity::AgentKey>,
    caveats: newt_core::caveats::Caveats,
}

impl SessionCapability {
    /// Establish the session capability from the configured policy + per-user key.
    fn establish(
        tui: Option<newt_core::TuiConfig>,
        key_path: Option<&std::path::Path>,
        workspace: &str,
    ) -> Self {
        let policy = policy_for(tui, workspace);
        let op = key_path.and_then(|p| mint_operating_key(p, &policy).ok());
        let caveats = match &op {
            Some(k) => newt_identity::enforced_caveats(k).unwrap_or(policy),
            None => policy,
        };
        Self { op, caveats }
    }

    /// The active enforcement caveats the tool loop consults.
    fn caveats(&self) -> &newt_core::caveats::Caveats {
        &self.caveats
    }

    /// Mint a plugin-side envelope for a subprocess running under `role`
    /// with `child_caveats`, by delegating from the live operating key.
    ///
    /// **Issue #93:** when the TUI eventually spawns a subprocess
    /// plugin (today: in-process tool calls only), the resulting
    /// `AgentKey` it hands the plugin MUST chain back to the operator's
    /// `UserKey` from `~/.newt/identity.pem` — never a synthetic key
    /// minted at spawn time. This helper is the chokepoint the TUI's
    /// future subprocess-spawn path will route through.
    ///
    /// Returns:
    /// - `Some(Ok(envelope))` when the operating key is present and the
    ///   delegation succeeded — the envelope's cert chain roots back to
    ///   the operator.
    /// - `Some(Err(_))` when delegation refused (`child_caveats` would
    ///   amplify the operating key's authority).
    /// - `None` when the per-user key is unavailable (`SessionCapability`
    ///   degraded to a plain caveats floor).
    #[cfg_attr(not(test), allow(dead_code))]
    fn plugin_envelope_for(
        &self,
        role: &str,
        child_caveats: newt_core::Caveats,
    ) -> Option<std::result::Result<String, newt_identity::EnvelopeError>> {
        let op = self.op.as_ref()?;
        Some(newt_identity::delegate_for_plugin(op, role, child_caveats))
    }

    /// Re-apply a (possibly changed) policy, **narrowing-only**. Returns `true`
    /// if the request asked for *more* authority than the session currently
    /// holds and was therefore clamped (so the caller can tell the user a
    /// restart is required to widen).
    fn reapply(&mut self, tui: Option<newt_core::TuiConfig>, workspace: &str) -> bool {
        let requested = policy_for(tui, workspace);
        let narrowed = requested.meet(&self.caveats);
        let clamped = narrowed != requested;
        if let Some(op) = self.op.take() {
            match newt_identity::attenuate(&op, &narrowed)
                .and_then(|child| newt_identity::enforced_caveats(&child).map(|c| (child, c)))
            {
                Ok((child, c)) => {
                    self.op = Some(child);
                    self.caveats = c;
                }
                // Unreachable in practice (narrowed ⊑ op): keep the old key but
                // still apply the narrowed caveats.
                Err(_) => {
                    self.op = Some(op);
                    self.caveats = narrowed;
                }
            }
        } else {
            self.caveats = narrowed;
        }
        clamped
    }
}

// ---------------------------------------------------------------------------
// Prompted ocap grants — issue #263 (`--prompt-for-permissions`)
// Interactive permission prompting (`--prompt-for-permissions`, #263) — the
// OCAP grant UI + `PromptPermissionGate`/`PermissionPromptState` state machine
// — lives in `permissions.rs`. The `/posture` named-preset clamp (#307) stays
// below.

// ---------------------------------------------------------------------------
// Operating modes (`/mode`) — working style, never authority.
//
// The TYPE lives in `newt_core::operating_mode` since #2009 PR4b (shared
// vocabulary belongs in the minimal layer, and `settings_form::apply` has
// to reach it). This alias keeps every `OperatingMode` reference in this
// crate reading the same as it did.
// ---------------------------------------------------------------------------

pub(crate) use newt_core::operating_mode::OperatingMode;

/// Session-local model selection behind `/mode auto`.
///
/// The selected style is bound to the conversation that requested it and is
/// consumed by its next action-shaped turn. Conversation-boundary handlers
/// clear it eagerly, so `/new`, restore, and persona-driven rotation cannot
/// resurrect a stale model choice.
#[derive(Debug, Default)]
struct AutoModeState {
    selected: std::sync::Mutex<Option<(String, OperatingMode)>>,
}

impl AutoModeState {
    fn take_for(&self, conversation_id: &str) -> Option<OperatingMode> {
        let Ok(mut selected) = self.selected.lock() else {
            return None;
        };
        match selected.take() {
            Some((bound_id, mode)) if bound_id == conversation_id => Some(mode),
            Some(_) | None => None,
        }
    }

    #[cfg(test)]
    fn pending_for(&self, conversation_id: &str) -> Option<OperatingMode> {
        self.selected
            .lock()
            .ok()
            .and_then(|selected| match selected.as_ref() {
                Some((bound_id, mode)) if bound_id == conversation_id => Some(*mode),
                _ => None,
            })
    }

    fn clear(&self) {
        if let Ok(mut selected) = self.selected.lock() {
            *selected = None;
        }
    }

    fn bind<'a>(&'a self, conversation_id: &'a str) -> TurnAutoModeControl<'a> {
        TurnAutoModeControl {
            state: self,
            conversation_id,
        }
    }
}

/// Per-TUI-session state for the model-entered Plan phase. This is deliberately
/// not process-global: multiple embedded or test sessions must never clamp one
/// another's tool calls.
#[derive(Debug, Default)]
struct PlanModeState {
    active: std::sync::atomic::AtomicBool,
}

impl PlanModeState {
    fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Acquire)
    }

    fn clear(&self) {
        self.active
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

impl newt_core::agentic::PlanModeControl for PlanModeState {
    fn is_plan_mode(&self) -> bool {
        self.is_active()
    }

    fn set_plan_mode(&self, active: bool) -> Result<(), String> {
        self.active
            .store(active, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

/// Conversation-scoped operating state with one boundary-clear operation.
///
/// Keeping these together makes it impossible for `/new`, persona rotation,
/// or restore to clear one model-selected mode while accidentally retaining
/// the other.
#[derive(Debug, Default)]
struct ConversationModeStates {
    auto: AutoModeState,
    plan: PlanModeState,
}

impl ConversationModeStates {
    fn clear(&self) {
        self.auto.clear();
        self.plan.clear();
    }
}

/// Turn-bound adapter lent to the core loop only while the human-selected
/// session mode is Auto.
struct TurnAutoModeControl<'a> {
    state: &'a AutoModeState,
    conversation_id: &'a str,
}

impl newt_core::agentic::OperatingModeControl for TurnAutoModeControl<'_> {
    fn select_operating_mode(&self, requested: &str) -> Result<String, String> {
        let Some(mode) = OperatingMode::from_keyword(requested) else {
            return Err(
                "choose one of: chat, dev, admin, plan, diagnose (full-auto is human-only)"
                    .to_string(),
            );
        };
        if matches!(mode, OperatingMode::Auto | OperatingMode::FullAuto) {
            return Err(format!(
                "{} cannot be model-selected; choose chat, dev, admin, plan, or diagnose",
                mode.as_str()
            ));
        }
        let mut selected = self
            .state
            .selected
            .lock()
            .map_err(|_| "session mode state is unavailable".to_string())?;
        *selected = Some((self.conversation_id.to_string(), mode));
        Ok(format!(
            "selected {} for the next action-shaped turn in this conversation. \
             The current turn's operating mode, disposition, caveats, and permissions are unchanged.",
            mode.as_str()
        ))
    }
}

/// Apply a human `/mode` command to session state and return printable lines.
/// Invalid input is fail-closed: the active mode is left untouched.
fn operating_mode_command_lines(
    arg: &str,
    active: &mut OperatingMode,
) -> Result<Vec<String>, String> {
    let arg = arg.trim().to_ascii_lowercase();
    if arg.is_empty() || arg == "list" {
        let mut lines = vec![format!(
            "active operating mode: {} — {}",
            active.as_str(),
            active.description()
        )];
        lines.push("available operating modes:".to_string());
        lines.extend(
            OperatingMode::all()
                .iter()
                .map(|mode| format!("  {:<9} {}", mode.as_str(), mode.description())),
        );
        return Ok(lines);
    }
    if matches!(arg.as_str(), "show" | "status") {
        return Ok(vec![format!(
            "active operating mode: {} — {}",
            active.as_str(),
            active.description()
        )]);
    }
    if matches!(arg.as_str(), "off" | "clear" | "reset" | "default") {
        *active = OperatingMode::Chat;
        return Ok(vec![
            "operating mode reset to chat (the default)".to_string()
        ]);
    }
    let Some(mode) = OperatingMode::from_keyword(&arg) else {
        let names = OperatingMode::all()
            .iter()
            .map(|mode| mode.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        return Err(format!("unknown /mode '{arg}' — usage: /mode <{names}>"));
    };
    *active = mode;
    Ok(vec![format!(
        "operating mode set to {} — {}",
        mode.as_str(),
        mode.description()
    )])
}

/// Resolve the working style for this turn. An explicit human mode is stable;
/// `auto` first honors a conversation-local model selection on action-shaped
/// work, then falls back to a deterministic, inspectable choice from accepted
/// prompt intake. Protected non-action intake always wins. A legacy
/// model-entered plan phase is represented as `plan` so the prompt card and
/// executor cannot disagree.
fn effective_operating_mode(
    configured: OperatingMode,
    intake: &newt_core::agentic::PromptIntake,
    model_plan_phase: bool,
    auto_selected: Option<OperatingMode>,
) -> OperatingMode {
    use newt_core::agentic::PromptDisposition;
    if model_plan_phase {
        return OperatingMode::Plan;
    }
    if configured != OperatingMode::Auto {
        return match (configured, intake.disposition()) {
            // These two modes carry implementation imperatives. On protected
            // non-action intake, render disposition-compatible instructions
            // instead so small models are not told both to edit and not edit.
            (
                OperatingMode::Dev | OperatingMode::Admin | OperatingMode::FullAuto,
                PromptDisposition::Research,
            ) => OperatingMode::Diagnose,
            (
                OperatingMode::Dev | OperatingMode::Admin | OperatingMode::FullAuto,
                PromptDisposition::Plan,
            ) => OperatingMode::Plan,
            (
                OperatingMode::Dev | OperatingMode::Admin | OperatingMode::FullAuto,
                PromptDisposition::Ask | PromptDisposition::Explain,
            ) => OperatingMode::Chat,
            _ => configured,
        };
    }

    let prompt = intake
        .atomic_asks()
        .iter()
        .map(newt_core::agentic::AtomicAsk::text)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    match intake.disposition() {
        PromptDisposition::Act => {
            if let Some(selected) = auto_selected
                .filter(|mode| !matches!(mode, OperatingMode::Auto | OperatingMode::FullAuto))
            {
                return selected;
            }
            // Action keywords win prompt intake. Only an explicit leading
            // orchestration intent overrides the normal developer style, so
            // "fix the diagnose mode" and "implement plan mode" remain dev.
            let mut objective = prompt.trim_start();
            for prefix in ["please ", "can you ", "could you ", "would you "] {
                if let Some(rest) = objective.strip_prefix(prefix) {
                    objective = rest.trim_start();
                    break;
                }
            }
            let starts_any =
                |needles: &[&str]| needles.iter().any(|needle| objective.starts_with(needle));
            if starts_any(&[
                "diagnose ",
                "investigate ",
                "troubleshoot ",
                "find the root cause",
            ]) {
                OperatingMode::Diagnose
            } else if starts_any(&["plan ", "write a plan", "make a plan", "formulate a plan"]) {
                OperatingMode::Plan
            } else if starts_any(&[
                "use admin mode",
                "work in admin mode",
                "act as a sysadmin",
                "perform system administration",
            ]) {
                OperatingMode::Admin
            } else {
                OperatingMode::Dev
            }
        }
        PromptDisposition::Research => OperatingMode::Diagnose,
        PromptDisposition::Ask | PromptDisposition::Explain => OperatingMode::Chat,
        PromptDisposition::Plan => OperatingMode::Plan,
    }
}

/// A mode can preserve or narrow prompt intake, never widen it. Plan and
/// diagnose force action-shaped prompts through the same `PromptIntake` object
/// later used by the model card, artifact recorder, catalog, and dispatcher.
fn apply_operating_mode_to_intake(
    mode: OperatingMode,
    intake: &mut newt_core::agentic::PromptIntake,
) {
    match mode {
        OperatingMode::Plan => {
            intake.enforce_read_only(newt_core::agentic::PromptDisposition::Plan);
        }
        OperatingMode::Diagnose => {
            intake.enforce_read_only(newt_core::agentic::PromptDisposition::Research);
        }
        _ => {}
    }
}

/// Defense in depth for modes that promise no mutation. This is a meet with
/// the existing plan-phase clamp, so it can only attenuate ambient authority.
fn operating_mode_caveats(mode: OperatingMode, caveats: newt_core::Caveats) -> newt_core::Caveats {
    match mode {
        OperatingMode::Plan => caveats.meet(&newt_core::agentic::plan_phase_clamp()),
        OperatingMode::Diagnose => caveats.meet(&diagnose_mode_clamp()),
        _ => caveats,
    }
}

/// Diagnose may gather remote read-only evidence, while still denying every
/// workspace mutation and executable spawn. Plan is stricter and remains
/// fully offline through [`newt_core::agentic::plan_phase_clamp`].
fn diagnose_mode_clamp() -> newt_core::Caveats {
    use newt_core::{CountBound, Scope};
    newt_core::Caveats {
        fs_read: Scope::All,
        fs_write: Scope::none(),
        exec: Scope::none(),
        net: Scope::All,
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

fn operating_mode_prompt(configured: OperatingMode, effective: OperatingMode) -> String {
    let identity = if configured == effective {
        format!(
            "Operating mode: {} — {}",
            effective.as_str(),
            effective.description()
        )
    } else {
        format!(
            "Configured session mode: {}. Effective working style for this turn: {} — {}.",
            configured.as_str(),
            effective.as_str(),
            effective.description(),
        )
    };
    let auto_control = if configured == OperatingMode::Auto {
        "\nAuto-mode control: use select_operating_mode when the next action-shaped turn \
         should use chat, dev, admin, plan, or diagnose. A selection takes effect only \
         on a later turn and grants no authority. Never attempt to select full-auto."
    } else {
        ""
    };
    let configured_invariants = if configured == OperatingMode::Admin {
        "\nConfigured admin invariants remain in force: Do no harm. Make minimal changes. \
         Respect privacy. With great power comes great responsibility."
    } else {
        ""
    };
    format!(
        "<operating_mode configured=\"{}\" effective=\"{}\">\n{}\n\
         Effective instructions:\n{}{}{}\n\
         This mode controls working style only. It grants no authority, bypasses no \
         permission or safety boundary, and cannot turn a read-only prompt into an \
         action prompt.\n</operating_mode>",
        configured.as_str(),
        effective.as_str(),
        identity,
        effective.instructions(),
        auto_control,
        configured_invariants,
    )
}

// ---------------------------------------------------------------------------
// Named permission presets + the `/posture` command (issue #307).
// ---------------------------------------------------------------------------

// `ActivePosture` and `build_posture` live in `newt_core::posture` since #2009
// PR10c — §5.1's ledger named the posture as what blocked the Permissions
// section's write half, and a pure `settings_form::apply` cannot read a
// `run_chat` local. These aliases keep every reference in this crate reading
// as it did.
pub(crate) use newt_core::posture::{build_posture, ActivePosture};

/// Compose posture guidance from current session state on every turn. It is
/// deliberately not appended to the frozen base prompt: switching or clearing
/// a posture therefore cannot leave stale instructions behind, and `/new`
/// cannot accidentally drop an active posture.
fn posture_prompt(posture: &ActivePosture) -> String {
    let authority_line = if posture.permission_clamp().is_none() {
        format!(
            "Active permission posture: {} (no permission clamp)",
            posture.name
        )
    } else {
        format!(
            "Active permission posture: {} — {}",
            posture.name, posture.clamp_summary
        )
    };
    let mut lines = vec![
        format!(
            "<permission_posture name=\"{}\">",
            posture.name.replace('"', "&quot;")
        ),
        authority_line,
    ];
    if posture.permission_clamp().is_none() {
        lines.push(
            "This posture has no configured permission floor. Its skill and framing \
             do not grant authority; the session's existing boundaries remain in force."
                .to_string(),
        );
    } else {
        lines.push(
            "This posture's permission preset is an authority floor. It can only \
             narrow permissions and cannot be overridden by the operating mode or \
             a session grant."
                .to_string(),
        );
    }
    if let Some(framing) = &posture.framing {
        lines.push(format!("Posture framing: {framing}"));
    }
    if let Some(skill) = &posture.skill_body {
        lines.push(format!("Preloaded posture skill guidance:\n{skill}"));
    }
    lines.push("</permission_posture>".to_string());
    lines.join("\n")
}

fn session_control_prompt(
    configured_mode: OperatingMode,
    effective_mode: OperatingMode,
    posture: Option<&ActivePosture>,
) -> String {
    let mut prompt = operating_mode_prompt(configured_mode, effective_mode);
    if let Some(posture) = posture {
        prompt.push_str("\n\n");
        prompt.push_str(&posture_prompt(posture));
    }
    prompt
}

/// The session's EFFECTIVE authority for this turn: the session base
/// (`SessionCapability::caveats`) intersected with the active posture's
/// optional preset clamp. This is the single intersection point the
/// enforcement path consults — `meet` is the greatest lower bound, so the
/// result can never exceed EITHER the base or a configured preset. With no
/// active posture, or a posture with no preset, it is the base unchanged.
fn effective_caveats(
    base: &newt_core::Caveats,
    posture: Option<&ActivePosture>,
) -> newt_core::Caveats {
    match posture.and_then(ActivePosture::permission_clamp) {
        Some(clamp) => base.meet(clamp),
        None => base.clone(),
    }
}

/// FR-1 (#997): a persona's read-only `[caveats]` are now ENFORCED, not merely
/// shown by `/persona show`. Meet them into the turn's authority — `meet` is the
/// greatest lower bound, so a persona can only TIGHTEN the session grant (e.g. a
/// read-only coach drops `fs_write`/`exec`), never widen it. No persona, or a
/// persona without a `[caveats]` block, leaves the authority unchanged.
fn meet_persona_caveats(base: newt_core::Caveats, persona: Option<&Persona>) -> newt_core::Caveats {
    match persona.and_then(|p| p.profile.caveats.as_ref()) {
        Some(profile) => base.meet(&profile.to_caveats()),
        None => base,
    }
}

/// #774 (P0): the always-on exec FLOOR threaded to `execute_tool` as
/// `exec_floor`. The operator's `[tui.permissions]` exec clamp is a
/// NON-OPTIONAL floor — enforced even with NO active `/posture`.
///
/// `effective_exec` is the base `[tui.permissions]` exec scope already met with
/// any configured `/posture` clamp (i.e.
/// `effective_caveats(base, posture).exec`), so the floor is meet-only: a
/// posture preset can only TIGHTEN it, never widen it past either the base or
/// the preset.
///
/// Returns `Some(effective_exec)` whenever the operator configured a restrictive
/// exec clamp (`Scope::Only`) OR a `/posture` permission floor is active, so an
/// out-of-floor command can never take the `--disable-ocap` / `--yolo`
/// unconfined bypass — it falls through to the confined shell, which enforces
/// the (already-clamped) caveats and denies it. Returns `None` ONLY when exec is
/// unrestricted (`Scope::All`) AND no posture permission floor is active,
/// leaving the unrestricted `--disable-ocap` bypass exactly as it was pre-#307.
///
/// Before #774 the floor was sourced from the active `/posture` ALONE
/// (`active_posture.map(|p| p.clamp.exec.clone())`), so a configured
/// `[tui.permissions]` clamp imposed NO floor without a posture — the
/// design-review F1 finding: the operator's exec clamp was not enforced by
/// default on the bypass path.
fn exec_floor_from(
    effective_exec: &newt_core::caveats::Scope<String>,
    posture_floor_active: bool,
) -> Option<newt_core::caveats::Scope<String>> {
    use newt_core::caveats::Scope;
    match effective_exec {
        // Unrestricted base, no posture preset ⇒ no floor (pre-#307 bypass).
        Scope::All if !posture_floor_active => None,
        // A configured restriction or posture preset is an always-on floor.
        scope => Some(scope.clone()),
    }
}

/// Render the `/permissions` listing: this session's prompted decisions (in
/// prompt order) plus where the durable record lives. Promotion to a lasting
/// grant is deliberately NOT offered here — that is a human editing
/// `[tui.permissions]` in the config (see issues #263/#181).
///
/// `active_posture` (issue #307) reflects an applied `/posture` preset as an
/// authority floor at the top of the listing, even when prompting is off.
pub(crate) fn permissions_command_lines(
    state: &PermissionPromptState,
    enabled: bool,
    log_path: Option<&std::path::Path>,
    active_posture: Option<&ActivePosture>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(posture) = active_posture {
        if posture.permission_clamp().is_none() {
            lines.push(format!(
                "active permission posture: {} (no permission clamp)",
                posture.name
            ));
        } else {
            lines.push(format!(
                "active permission posture: {} — preset '{}' clamps authority (floor): {}",
                posture.name, posture.preset_name, posture.clamp_summary
            ));
            lines.push(
                "this clamp WINS over --disable-ocap/--yolo and over session grants (#307)"
                    .to_string(),
            );
        }
    }
    if !enabled {
        lines.push(
            "permission prompting is OFF (start with --prompt-for-permissions or set \
             [tui.permissions] prompt = true)"
                .to_string(),
        );
    }
    if state.decisions.is_empty() {
        lines.push("no prompted permission decisions this session".to_string());
    } else {
        lines.push("prompted permission decisions this session:".to_string());
        for d in &state.decisions {
            lines.push(format!(
                "  {:<5} {:<7}  {}:{}  via {}",
                d.decision, d.scope, d.kind, d.target, d.tool
            ));
        }
    }
    if let Some(path) = log_path {
        lines.push(format!("log: {}", path.display()));
        lines.push(
            "to make an allow permanent, edit [tui.permissions] in your newt config \
             (extra_exec / net) — the log is a record, never authority"
                .to_string(),
        );
    }
    lines
}

/// Parse the permission log into human-readable audit rows (newest-first).
///
/// Malformed lines are skipped so a corrupt log never blocks the `/permissions
/// audit` path. An empty or unreadable log returns a one-line user-facing
/// message instead.
pub(crate) fn permission_audit_lines(log_path: &std::path::Path, limit: usize) -> Vec<String> {
    let body = match std::fs::read_to_string(log_path) {
        Ok(body) => body,
        Err(e) => {
            return vec![format!(
                "unable to read permission log {}: {e}",
                log_path.display()
            )];
        }
    };

    let records: Vec<newt_core::PermissionRecord> = body
        .lines()
        .filter_map(|line| serde_json::from_str::<newt_core::PermissionRecord>(line).ok())
        .collect();

    if records.is_empty() {
        return vec!["no permission log entries yet".to_string()];
    }

    let show = if limit == 0 { records.len() } else { limit };
    let shown = records.len().min(show);
    let mut lines = vec![format!(
        "permission audit: {shown} of {} (newest first)",
        records.len()
    )];
    for rec in records.iter().rev().take(show) {
        lines.push(format!(
            "  {:<7} {:<9} {:<8} {} via {}",
            rec.decision, rec.scope, rec.kind, rec.target, rec.tool
        ));
    }
    lines
}

// ---------------------------------------------------------------------------
// INTERIM (#297): --disable-ocap / --yolo session surfacing. Removed with the
// bypass when brush upstreams CommandInterceptor (agent-bridle#20).
// ---------------------------------------------------------------------------

/// INTERIM (#297): the unmissable session-start banner shown when the ocap
/// exec bypass is asserted (`--disable-ocap` / `--yolo` /
/// `NEWT_DISABLE_OCAP=1`). The bypass itself lives at the `run_command`
/// dispatch in newt-core; this is the loud surfacing half of the contract.
fn ocap_disabled_banner() -> String {
    "⚠ ocap DISABLED (--disable-ocap): permitted commands may run unconfined on the \
     host shell; active exec floors can force confinement or denial — fs tools keep \
     the workspace fence; drop the flag to restore default confinement (#297)"
        .to_string()
}

/// INTERIM (#297): the ONE `ocap-disabled` line written to the #263
/// permission log at session start, so the audit trail shows this session
/// requested the unconfined exec bypass. `decision: "ocap-disabled"`, `scope:
/// "session"` per the issue; the `*` target means the bypass is enabled for the
/// session, while effective exec floors still decide each command. A record,
/// not authority: nothing reads it back.
fn ocap_disabled_record(conversation_id: &str) -> newt_core::PermissionRecord {
    newt_core::PermissionRecord::new(
        conversation_id,
        "run_command",
        newt_core::DenialKind::Exec,
        "*",
        "ocap-disabled",
        "session",
    )
}

/// The unmissable session-start banner shown when the `--full-access` preset
/// override is asserted (`NEWT_FULL_ACCESS=1`). The override itself lives in
/// `policy_for`; this is the loud surfacing half of the contract, mirroring
/// `ocap_disabled_banner`.
fn full_access_banner() -> String {
    // #926: frame this as *ambient authority* + OCAP attenuation, not merely
    // "unrestricted" — the point of the harness is to attenuate the user's full
    // ambient authority into structural grants; --full-access temporarily hands
    // that ambient authority back. It also switches run_command to the `host`
    // shell engine (a real /bin/sh in the platform kernel jail).
    "⚠ FULL ACCESS (--full-access): the agent is granted your full AMBIENT authority \
     for this run — the Object-Capability attenuations (fs fence, net leash, exec \
     allowlist) are lifted and run_command uses the `host` shell engine (a real \
     /bin/sh inside the platform kernel jail). Writes are still prompted. Drop the \
     flag to restore Object-Capability authority restrictions."
        .to_string()
}

/// The ONE `full-access` line written to the #263 permission log at session
/// start, so the audit trail shows this session ran with the unrestricted
/// preset override. The override widens every capability axis; the log schema
/// records one line keyed on the exec axis (the sharpest one), `target: "*"`,
/// `scope: "session"` — the override is per-session, never per-command. A
/// record, not authority: nothing reads it back.
fn full_access_record(conversation_id: &str) -> newt_core::PermissionRecord {
    newt_core::PermissionRecord::new(
        conversation_id,
        "session",
        newt_core::DenialKind::Exec,
        "*",
        "full-access",
        "session",
    )
}

/// Process-environment synchronization for tests.
///
/// `cargo test` runs tests of this binary concurrently while the environment
/// is process-global. Tests that mutate env vars (e.g. `NEWT_EXEC_PATHS`,
/// `NEWT_VENV`) or that read resolution which depends on them take a guard,
/// so a mutation can never land mid-test.
///
/// **All four functions return the SAME lock** — `newt_core::process_env`'s
/// (#1850). This module used to own a private `RwLock` while
/// `newt_core::test_guard::GlobalSettingsGuard` owned an independent `Mutex`
/// over the same variables; two locks over one environment serialize nothing,
/// and the resulting race took down `tab_switch::state_machine_tests` and
/// `helper_fn_tests` wholesale in ~30% of `--all-features` runs. The
/// read/write and sync/async names are kept because they still document a
/// call site's INTENT, but the lock underneath is one exclusive, reentrant
/// lock that the production writers take as well.
#[cfg(test)]
pub(crate) mod test_env_guard {
    use newt_core::process_env::{lock, EnvGuard};

    /// Guard for synchronous tests that READ env-dependent resolution.
    pub(crate) fn env_read_guard() -> EnvGuard {
        lock()
    }

    /// Reader guard for `#[tokio::test]` tests. Kept as its own `async fn`
    /// so call sites keep their `.await`; the lock itself is sync and needs
    /// no runtime, which is why the old `blocking_read`-panics-in-a-runtime
    /// hazard is gone.
    pub(crate) async fn env_read_guard_async() -> EnvGuard {
        lock()
    }

    /// Guard for tests that MUTATE the process environment.
    pub(crate) fn env_write_guard() -> EnvGuard {
        lock()
    }

    /// Writer guard for `#[tokio::test]` tests. Gated like the rest of the
    /// #297 disable-ocap tests it serves, so Windows does not trip
    /// `-D warnings` on dead code.
    #[cfg(unix)]
    pub(crate) async fn env_write_guard_async() -> EnvGuard {
        lock()
    }
}

/// **#1850 regression — one lock over the process environment.**
///
/// The flake this pins was never a wrong assertion. It was TWO locks:
/// `newt_core::test_guard::GlobalSettingsGuard` (a `Mutex`) and this crate's
/// `test_env_guard` (an `RwLock`), each claiming `NEWT_PROVIDER` /
/// `NEWT_DGX_MODEL`, neither excluding the other — plus production writers
/// holding neither under `// SAFETY: single-threaded REPL`, a claim that is
/// true of the REPL and false under `cargo test`.
///
/// The cost was ~30% of `cargo test -p newt-tui --lib --all-features` runs,
/// with whole modules going down together: 30 `tab_switch::state_machine_tests`
/// panicking inside a backend re-resolve, and
/// `helper_fn_tests::resolver_default_backend_beats_the_openai_heuristic`
/// asserting against a literal `"bound-model"` that exists nowhere but
/// `lib_tests::env_resolution`'s fixture.
#[cfg(test)]
#[path = "lib_tests/process_env_isolation_tests.rs"]
mod process_env_isolation_tests;

#[cfg(test)]
#[path = "lib_tests/caveat_policy_tests.rs"]
mod caveat_policy_tests;

/// `NoteSink` over the session `MemoryManager` (Step 19.3, #248): the model's
/// `save_note` tool and the human's `/remember` command route through the
/// SAME `MemoryManager` → `NoteStore` write path — one store, one write-time
/// security scan, one char budget. Borrows the manager only for the duration
/// of a single `chat_complete` call.
struct ManagerNoteSink<'m> {
    memory: &'m mut newt_core::MemoryManager,
}

impl newt_core::NoteSink for ManagerNoteSink<'_> {
    fn add(&mut self, fact: &str) -> anyhow::Result<()> {
        self.memory.add_note(fact)
    }
    fn replace(&mut self, old_substring: &str, new_text: &str) -> anyhow::Result<()> {
        self.memory.replace_note(old_substring, new_text)
    }
    fn remove(&mut self, substring: &str) -> anyhow::Result<()> {
        self.memory.remove_note(substring)
    }
    fn usage_line(&self) -> String {
        self.memory
            .usage()
            .iter()
            .find(|(label, _, _)| label == "notes")
            .map(|(_, cur, max)| {
                let pct = if *max > 0 { cur * 100 / max } else { 0 };
                format!("notes: {cur}/{max} chars ({pct}%)")
            })
            .unwrap_or_else(|| "notes: usage unavailable".to_string())
    }
}

/// Build the agentic loop's compression summarizer (Step 18.4, #247): one
/// tools-disabled completion against the active backend, mirroring the
/// `Summarizing` provider's `with_summarizer` wiring below — but async,
/// because the loop invokes it mid-turn from inside its own async context.
/// A non-2xx status or empty content is an error so the loop falls back to
/// the static compaction marker instead of injecting garbage.
///
/// `num_ctx` is the same effective context cap the main loop sends
/// (`options.num_ctx` on Ollama): the summary request is typically the
/// largest single message of the session, and without the cap Ollama
/// silently truncates it at the model's default window (F5).
/// OpenAI-compatible endpoints configure context server-side — ignored.
/// Summarizer knobs (Step 24.2, #559). Consolidated into one struct so new knobs
/// (24.3's fallback model, …) add a field rather than another positional param
/// at every `make_loop_summarizer` call site. `Default` lets a call site set
/// only what it cares about (`..Default::default()`).
#[derive(Clone)]
struct SummarizerOpts {
    /// Effective `num_ctx` cap sent to Ollama (F5); `None` = model default.
    num_ctx: Option<u32>,
    /// Ollama `keep_alive` for the warm + summary requests (Step 24.1).
    keep_alive: String,
    /// Per-request timeout in seconds (Step 24.2; `summarizer.toml` `timeout_secs`).
    timeout_secs: u64,
    /// Retry attempts before falling back to the static marker (Step 24.2).
    retries: u32,
    /// Optional fallback model (Step 24.3; `summarizer.toml` `fallback_model`).
    /// When the primary model's attempts all fail, the summary is retried once
    /// on this model — a rung above the static marker. `None` = no fallback.
    fallback_model: Option<String>,
    /// Styling ONLY: may the live retry/fallback notices carry ANSI color?
    /// Never a capability signal — `color` used to gate whether these notices
    /// were emitted *at all*, which is exactly the styling-into-I/O-ownership
    /// overload `LineCaps` exists to end (see `newt_core::tty::caps`).
    color: bool,
    /// Whether this process may narrate onto the terminal at all (Step 24.7's
    /// real question). `None` — the default — emits zero bytes, so a headless
    /// or captured stream stays clean even under `NEWT_COLOR=always`.
    caps: newt_core::tty::LineCaps,
}

impl Default for SummarizerOpts {
    fn default() -> Self {
        Self {
            num_ctx: None,
            keep_alive: "5m".to_string(),
            timeout_secs: 60,
            retries: 2,
            fallback_model: None,
            color: false,
            caps: newt_core::tty::LineCaps::None,
        }
    }
}

/// Live summarizer-progress notices (Step 24.7, #559). The summarizer runs
/// under the `compressing context…` spinner, so each notice has to scroll into
/// history *without* destroying the row the spinner is redrawing.
///
/// These are the pure text builders; [`summarizer_notice`] wraps one in a
/// `Notice` and the arbiter does the cooperating. Until this migration the
/// wrapping was a local `summarizer_progress` that wrote `\r\x1b[K` straight to
/// stdout with no lease — the race documented on
/// `newt_core::tty::Terminal::emit_line`.
fn retry_progress_msg(attempt: u32, total: u32) -> String {
    format!("↻ summarizer retrying (attempt {attempt}/{total})…")
}
fn fallback_progress_msg(model: &str) -> String {
    format!("⚠ summarizer falling back to {model}…")
}
fn failure_progress_msg(err: &anyhow::Error) -> String {
    format!("⚠ summarizer failed ({err}); using static compression marker…")
}

/// One summarizer notice, as a `Notice` value. The text already leads with its
/// own sigil, so the glyph is empty and `line()` is the message verbatim —
/// which is what keeps the builders above (and their tests) untouched.
fn summarizer_notice(msg: String) -> newt_core::tty::Notice<'static> {
    newt_core::tty::Notice::new(newt_core::tty::Level::Warn, "", msg)
}

/// One summary attempt: send, check status, parse the content.
async fn summarize_attempt(
    client: &reqwest::Client,
    chat_url: &str,
    body: &serde_json::Value,
    api_key: &Option<String>,
    openai: bool,
) -> anyhow::Result<String> {
    let mut req = client.post(chat_url).json(body);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("summarizer endpoint {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    extract_summary(&json, openai)
}

/// Extract the summary text from a chat response, robust to THINKING models.
/// A thinking model asked to summarize may wrap the summary in inline
/// `<think>…</think>` (stripped here via `split_reasoning`) or — on Ollama — put
/// its reasoning in a separate `thinking` field and leave `content` EMPTY. The
/// request sends `think: false` to prevent the empty-content case for
/// cooperative models; this is the response-side belt-and-suspenders. A
/// still-empty result is a genuine empty summary (the caller degrades it to the
/// static marker) rather than a `<think>`-polluted string masquerading as one.
fn extract_summary(json: &serde_json::Value, openai: bool) -> anyhow::Result<String> {
    let raw = if openai {
        json["choices"][0]["message"]["content"].as_str()
    } else {
        json["message"]["content"].as_str()
    }
    .unwrap_or("");
    let (clean, _reasoning) = newt_core::split_reasoning(raw);
    if clean.trim().is_empty() {
        anyhow::bail!("summarizer returned empty content (thinking-only reply?)");
    }
    Ok(clean)
}

#[cfg(test)]
#[path = "lib_tests/summarizer_extract_tests.rs"]
mod summarizer_extract_tests;

/// Warm (Ollama), build the body, and run the retry loop for ONE model. Used
/// for the primary model and, on total failure, the optional fallback (24.3).
async fn summarize_one_model(
    url: &str,
    model: &str,
    openai: bool,
    prompt: &str,
    opts: &SummarizerOpts,
    api_key: &Option<String>,
) -> anyhow::Result<String> {
    let chat_url = if openai {
        format!("{}/v1/chat/completions", url.trim_end_matches('/'))
    } else {
        format!("{}/api/chat", url.trim_end_matches('/'))
    };
    // Step 24.1 (#559): for Ollama, warm the model under a generous timeout
    // BEFORE the short-timeout summary request, so a cold reload is absorbed
    // here instead of blowing the summary timeout. Best-effort: warm errors are
    // ignored (the real request surfaces a hard failure).
    if !openai {
        let warm_url = format!("{}/api/generate", url.trim_end_matches('/'));
        let warm_body = serde_json::json!({
            "model": model,
            "keep_alive": opts.keep_alive,
            "stream": false,
        });
        if let Ok(warm_client) = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
        {
            let mut wreq = warm_client.post(&warm_url).json(&warm_body);
            if let Some(key) = api_key {
                wreq = wreq.bearer_auth(key);
            }
            let _ = wreq.send().await;
        }
    }
    // No `tools` key => the model cannot emit tool calls. Ollama also gets
    // `keep_alive` (mirroring the main loop) so the summary request doesn't
    // reset the model's residency to Ollama's default.
    let body = if openai {
        serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": false,
        })
    } else {
        // `think: false` — a summary is a direct extraction task, not a
        // reasoning one. A thinking model (emits_thinking) otherwise returns a
        // thinking-only reply with EMPTY content that degrades to the static
        // marker (silent context loss). Non-thinking models / older Ollama
        // ignore the field.
        let mut b = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": false,
            "think": false,
            "keep_alive": opts.keep_alive,
        });
        if let Some(ctx_size) = opts.num_ctx {
            b["options"] = serde_json::json!({ "num_ctx": ctx_size });
        }
        b
    };
    // Step 24.2 (#559): retry with backoff before giving up. The per-request
    // timeout is configurable (`summarizer.toml` `timeout_secs`).
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(opts.timeout_secs))
        .build()?;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..=opts.retries {
        if attempt > 0 {
            // Step 24.7 (#559): surface the retry live (scrolls above the
            // `compressing context…` spinner) so the recovery is honest.
            summarizer_notice(retry_progress_msg(attempt + 1, opts.retries + 1)).emit(
                opts.caps,
                newt_core::tty::Sink::Stdout,
                opts.color,
            );
            // Exponential backoff capped at ~4s: 250ms, 500ms, 1s, …
            let backoff = std::time::Duration::from_millis(250u64 << (attempt - 1).min(4));
            tokio::time::sleep(backoff).await;
        }
        match summarize_attempt(&client, &chat_url, &body, api_key, openai).await {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("summarizer failed")))
}

fn make_loop_summarizer(
    url: String,
    model: String,
    kind: newt_core::BackendKind,
    api_key: Option<String>,
    model_path: Option<String>,
    opts: SummarizerOpts,
) -> newt_core::Summarizer {
    // #661 group C: an embedded summarizer runs the in-process candle engine
    // (#659) instead of an HTTP backend — zero contention with the primary model.
    if kind == newt_core::BackendKind::Embedded {
        return make_embedded_summarizer(model, model_path);
    }
    Box::new(move |prompt: String| {
        let url = url.clone();
        let model = model.clone();
        let api_key = api_key.clone();
        let opts = opts.clone();
        let openai = kind == newt_core::BackendKind::Openai;
        Box::pin(async move {
            match summarize_one_model(&url, &model, openai, &prompt, &opts, &api_key).await {
                Ok(s) => Ok(s),
                // Step 24.3 (#559): the primary model's attempts all failed — try
                // an explicitly configured fallback model once (a rung above the
                // static marker). Without explicit configuration, surface the
                // primary error so the compressor degrades immediately.
                Err(primary_err) => {
                    match opts.fallback_model.clone() {
                        Some(fb) if fb != model => {
                            // Step 24.7 (#559): announce the fallback live.
                            summarizer_notice(fallback_progress_msg(&fb)).emit(
                                opts.caps,
                                newt_core::tty::Sink::Stdout,
                                opts.color,
                            );
                            summarize_one_model(&url, &fb, openai, &prompt, &opts, &api_key)
                                .await
                                .map_err(|fallback_err| {
                                    anyhow::anyhow!(
                                        "primary summarizer failed: {primary_err}; fallback summarizer '{fb}' failed: {fallback_err}"
                                    )
                                })
                        }
                        _ => {
                            summarizer_notice(failure_progress_msg(&primary_err)).emit(
                                opts.caps,
                                newt_core::tty::Sink::Stdout,
                                opts.color,
                            );
                            Err(primary_err)
                        }
                    }
                }
            }
        })
    })
}

/// A summarizer that always fails with `msg`. The compressor degrades to the
/// deterministic static marker (group D guarantees a fit), and the failure is
/// surfaced — used when an embedded summarizer is requested but cannot be built.
fn failing_summarizer(msg: String) -> newt_core::Summarizer {
    Box::new(move |_prompt: String| {
        let msg = msg.clone();
        Box::pin(async move { Err(anyhow::anyhow!(msg)) })
    })
}

/// Build the in-process candle summarizer (#661 group C). The `EmbeddedBackend`
/// (#659) loads its GGUF and is shared across calls; each summarize is one
/// `complete` on a blocking thread, so it never contends the async runtime or the
/// primary model. Falls back to a failing summarizer (→ static marker) when the
/// model file is missing or the build lacks the `embedded` feature.
#[cfg_attr(not(feature = "embedded"), allow(unused_variables))]
fn make_embedded_summarizer(model: String, model_path: Option<String>) -> newt_core::Summarizer {
    #[cfg(feature = "embedded")]
    {
        let Some(path) = model_path else {
            return failing_summarizer(
                "summarizer kind=embedded needs `summarizer.model_path` (the local GGUF)"
                    .to_string(),
            );
        };
        match newt_inference::embedded::EmbeddedBackend::new(&model, &path) {
            Ok(backend) => {
                let backend = std::sync::Arc::new(backend);
                Box::new(move |prompt: String| {
                    let backend = backend.clone();
                    Box::pin(async move {
                        use newt_inference::InferenceBackend;
                        let req = newt_inference::ChatRequest {
                            messages: vec![newt_inference::backend::Message::user(prompt)],
                            max_tokens: Some(1024),
                        };
                        backend.complete(req).await.map(|reply| reply.content)
                    })
                })
            }
            Err(e) => failing_summarizer(format!("embedded summarizer init failed: {e}")),
        }
    }
    #[cfg(not(feature = "embedded"))]
    {
        failing_summarizer(
            "summarizer kind=embedded, but this build lacks the `embedded` feature — \
             rebuild with --features embedded"
                .to_string(),
        )
    }
}

// ---------------------------------------------------------------------------
// Semantic embedder construction (#720)
// ---------------------------------------------------------------------------

/// An [`Embedder`](newt_core::Embedder) that always fails with `msg`. Used when an
/// embedded embedder is requested but cannot be built (no `embedding_model_path`,
/// the model dir is absent, or the build lacks the `embedded` feature) — semantic
/// indexing then degrades to a no-op with one actionable message, mirroring the
/// summarizer's [`failing_summarizer`].
struct FailingEmbedder {
    msg: String,
}

#[async_trait::async_trait]
impl newt_core::Embedder for FailingEmbedder {
    async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Err(anyhow::anyhow!(self.msg.clone()))
    }
}

/// Whether the semantic feature should embed on the **in-process** backend
/// rather than over HTTP. Embedded is the default when the semantic config does
/// not name an HTTP endpoint/protocol; Ollama/OpenAI embeddings are an explicit
/// performance-tuning choice. Pure for testing.
fn embeddings_backend_is_embedded(cfg: &newt_core::SemanticConfig) -> bool {
    cfg.embeddings_api == Some(newt_core::BackendKind::Embedded)
        || (cfg.embeddings_api.is_none() && cfg.embeddings_endpoint.is_none())
}

/// #1279 precedence for the embedded model dir: an explicit
/// `[context.semantic].embedding_model_path` wins; otherwise the pulled default
/// model dir (`newt models pull-embed`) when it is present. Pure — the caller
/// supplies `default_present` from the fs check
/// (`newt_inference::palette::embed_model_dir_if_present`), so this stays
/// unit-testable. `None` ⇒ no model available anywhere (the caller coaches the
/// pull command).
fn effective_embedding_model_path(
    explicit: Option<String>,
    default_present: Option<std::path::PathBuf>,
) -> Option<String> {
    explicit.or_else(|| default_present.map(|d| d.to_string_lossy().into_owned()))
}

/// Build the semantic embedder (#720). By default, or when
/// `embeddings_api = "embedded"`, the in-process candle embedder runs on the
/// laptop so retrieval never touches the DGX chat model's VRAM. HTTP
/// [`EmbeddingsClient`](newt_core::EmbeddingsClient) embeddings are selected
/// only by explicit Ollama/OpenAI semantic config. Always returns a usable
/// `Box<dyn Embedder>` — a mis-configured embedded path yields a failing embedder
/// (→ indexing no-op) rather than panicking. Pure for testing.
fn build_semantic_embedder(
    semantic_cfg: &newt_core::SemanticConfig,
    inf_url: &str,
    inf_kind: newt_core::BackendKind,
    inf_key: Option<&str>,
) -> Box<dyn newt_core::Embedder> {
    if embeddings_backend_is_embedded(semantic_cfg) {
        return make_embedded_embedder(
            semantic_cfg.embedding_model.clone(),
            semantic_cfg.embedding_model_path.clone(),
        );
    }
    let (emb_url, emb_kind, emb_key) =
        resolve_embeddings_target(semantic_cfg, inf_url, inf_kind, inf_key);
    Box::new(newt_core::EmbeddingsClient::new(
        emb_url,
        semantic_cfg.embedding_model.clone(),
        emb_kind,
        emb_key,
        60,
        2,
    ))
}

/// Preflight semantic embedder availability before walking the workspace. This
/// keeps default-on semantic context cheap in binaries that were not built with
/// the embedded feature, or where the local model path is not configured yet.
fn semantic_embedder_unavailable_reason(cfg: &newt_core::SemanticConfig) -> Option<String> {
    if !embeddings_backend_is_embedded(cfg) {
        return None;
    }
    #[cfg(not(feature = "embedded"))]
    {
        Some(
            "embedded semantic retrieval is selected, but this binary lacks the `embedded` \
             feature; rebuild with --features embedded or configure an explicit \
             [context.semantic].embeddings_endpoint"
                .to_string(),
        )
    }
    #[cfg(feature = "embedded")]
    {
        let Some(path) = cfg.embedding_model_path.as_deref() else {
            return Some(
                "embedded semantic retrieval has no model — run `newt models pull-embed` to \
                 fetch the default on-host model (bge-small), or set \
                 [context.semantic].embedding_model_path to a local candle-clean standard-BERT dir"
                    .to_string(),
            );
        };
        let dir = std::path::Path::new(path);
        if !dir.is_dir() {
            return Some(format!(
                "embedded semantic retrieval model dir not found: {}",
                dir.display()
            ));
        }
        for required in ["config.json", "tokenizer.json", "model.safetensors"] {
            let file = dir.join(required);
            if !file.exists() {
                return Some(format!(
                    "embedded semantic retrieval model file not found: {}",
                    file.display()
                ));
            }
        }
        None
    }
}

/// Build the in-process candle embedder (#720). The `CandleEmbedder` loads a
/// candle-clean standard-BERT model from `model_path` (a local dir) once and
/// embeds on a non-contending device (CPU by default). Falls back to a failing
/// embedder (→ indexing no-op) when `model_path` is unset, the model dir is
/// absent, or the build lacks the `embedded` feature.
#[cfg_attr(not(feature = "embedded"), allow(unused_variables))]
fn make_embedded_embedder(
    model: String,
    model_path: Option<String>,
) -> Box<dyn newt_core::Embedder> {
    #[cfg(feature = "embedded")]
    {
        let Some(path) = model_path else {
            return Box::new(FailingEmbedder {
                msg: "embedded semantic retrieval has no model — run `newt models pull-embed` \
                      (fetches bge-small), or set `[context.semantic].embedding_model_path` to a \
                      local candle-clean standard-BERT model dir"
                    .to_string(),
            });
        };
        match newt_inference::embed::CandleEmbedder::new(&model, &path) {
            Ok(e) => Box::new(e),
            Err(err) => Box::new(FailingEmbedder {
                msg: format!("embedded embedder init failed: {err}"),
            }),
        }
    }
    #[cfg(not(feature = "embedded"))]
    {
        Box::new(FailingEmbedder {
            msg: "embeddings_api=embedded, but this build lacks the `embedded` feature — \
                  rebuild with --features embedded"
                .to_string(),
        })
    }
}

fn semantic_zero_index_hint(cfg: &newt_core::SemanticConfig) -> String {
    if embeddings_backend_is_embedded(cfg) {
        return "semantic: indexed 0 chunks — no embedding model; run `newt models pull-embed` \
                to fetch the default on-host model (bge-small), or set \
                [context.semantic].embedding_model_path to a local candle-clean standard-BERT \
                model dir (retrieval is a no-op until embeddings work)"
            .to_string();
    }
    let target = cfg
        .embeddings_endpoint
        .as_deref()
        .unwrap_or("the active chat backend");
    format!(
        "semantic: indexed 0 chunks — embeddings from {target} produced no vectors for '{}'; \
         configure [context.semantic].embeddings_endpoint/embeddings_api for an Ollama/OpenAI \
         embeddings service, or set embedding_model_path to use embedded embeddings \
         (retrieval is a no-op until embeddings work)",
        cfg.embedding_model
    )
}

// ---------------------------------------------------------------------------
// End-of-conversation note extraction (Step 19.4, #248)
// ---------------------------------------------------------------------------

/// Marker prefixed to every note the close-time extraction writes, so a
/// reader of NOTES.md (or the `/memory` listing) can tell auto-extracted
/// facts from ones a human (`/remember`) or the model mid-session
/// (`save_note`) chose to keep. Plain entry text — the note schema carries
/// no per-entry metadata and is deliberately not extended here.
const EXTRACTED_NOTE_PREFIX: &str = "(auto-extracted) ";

/// Per-message character cap for the rendered extraction transcript. The
/// message COUNT is bounded by [`newt_core::trim_for_summary`]; this bounds
/// the other axis so one giant paste can't make the request unbounded.
const EXTRACTION_MSG_CHAR_CAP: usize = 2_000;

/// The extraction prompt: at most 3 durable, conversation-transcending
/// facts as `- ` bullets, or the literal `NONE`. Kept tight on purpose —
/// the transcript carries the bulk of the tokens.
fn build_extraction_prompt(transcript: &str) -> String {
    format!(
        "This coding-agent conversation is closing. From the transcript below, \
         extract at most 3 durable facts worth remembering in future \
         conversations — decisions made, constraints discovered, preferences \
         stated. Reply with one short bullet per fact, each line starting with \
         \"- \". Do NOT record task progress or anything only meaningful to \
         this session. If nothing qualifies, reply with exactly NONE.\
         \n\nTranscript:\n{transcript}"
    )
}

/// Render the session history into a bounded transcript for the extraction
/// prompt. The message count is bounded by [`newt_core::trim_for_summary`]
/// — the SAME head+tail helper the cap-exit summary request uses for its
/// input — and each message is clipped to [`EXTRACTION_MSG_CHAR_CAP`] chars,
/// so the request never ships the whole history unbounded. Returns `None`
/// when there is no conversational content to extract from (the system
/// prompt and the empty current-task slot don't count).
fn render_extraction_transcript(messages: &[newt_core::MemMessage]) -> Option<String> {
    let json_msgs: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| m.role != newt_core::Role::System && !m.content.trim().is_empty())
        .map(|m| serde_json::json!({"role": m.role.as_str(), "content": m.content}))
        .collect();
    if json_msgs.is_empty() {
        return None;
    }
    let rendered = newt_core::trim_for_summary(&json_msgs, 2, 6)
        .iter()
        .map(|m| {
            let role = m["role"].as_str().unwrap_or("user");
            let content = m["content"].as_str().unwrap_or_default();
            let clipped: String = content.chars().take(EXTRACTION_MSG_CHAR_CAP).collect();
            let marker = if content.chars().count() > EXTRACTION_MSG_CHAR_CAP {
                " [clipped]"
            } else {
                ""
            };
            format!("{role}: {clipped}{marker}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(rendered)
}

/// Parse the extraction reply into at most 3 bullets. The literal `NONE`
/// reply and any reply without bullet lines both yield an empty list —
/// deliberately silent: "nothing worth keeping" is the common close and
/// must not print a notice every time (documented UX choice).
fn parse_extraction_bullets(reply: &str) -> Vec<String> {
    if reply.trim().eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    reply
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            ["- ", "* ", "• "].iter().find_map(|p| line.strip_prefix(p))
        })
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("none"))
        .take(3)
        .map(String::from)
        .collect()
}

/// The 19.4 gate, pure for testing: extraction runs only when the config key
/// (`[memory] extract_notes_on_close`, default off) is set, the session
/// persists (an `--ephemeral` session must leave no trace, and NOTES.md IS a
/// trace), and the conversation completed at least one turn in THIS session
/// (a resumed-then-immediately-closed conversation adds nothing new).
fn should_extract_on_close(enabled: bool, ephemeral: bool, turns: usize) -> bool {
    enabled && !ephemeral && turns > 0
}

/// Whether the current process is an ephemeral session (`--ephemeral` /
/// `NEWT_EPHEMERAL`). Such sessions must leave no trace on disk, so the sticky
/// `/backends`/`/model` writers ([`newt_core::settings`]) are skipped — the
/// same "no trace" invariant that gates [`should_extract_on_close`].
pub(crate) fn is_ephemeral_session() -> bool {
    std::env::var_os("NEWT_EPHEMERAL").is_some()
}

/// One honest line about what the close-time extraction wrote. Scan- or
/// budget-rejected bullets are dropped (never retried) and disclosed here;
/// the cause goes to `tracing::warn`, so the visible line stays
/// cause-neutral and true for every rejection kind.
fn close_extraction_notice(saved: usize, rejected: usize) -> String {
    let noun = if saved == 1 { "note" } else { "notes" };
    if rejected > 0 {
        format!("extracted {saved} {noun} on close ({rejected} rejected)")
    } else {
        format!("extracted {saved} {noun} on close")
    }
}

/// Step 19.4 (#248): ONE synchronous tools-disabled completion at
/// conversation close (`/new` and clean exit — never the EMFILE/panic crash
/// paths), distilling at most 3 durable facts into NOTES.md. No background
/// fork (design doc § Do-Not-Copy #3): this runs inline, once, bounded by
/// the request's own timeout.
///
/// `complete` is built by [`make_loop_summarizer`] — the same request the
/// cap-exit summary uses, which sends **no `tools` key**, so the model
/// structurally cannot emit tool calls. Every accepted bullet goes through
/// `MemoryManager::add_note` → `NoteStore::add` → the 19.2 write-time scan —
/// the SAME path `save_note` and `/remember` use; there is no raw file
/// write. Mid-session note writes don't reach the frozen system prompt
/// anyway, so writing during close loses nothing — the notes load next
/// session.
///
/// Returns the notice line when bullets were processed, `None` when the gate
/// skipped, nothing qualified (silent NONE), or the backend failed — a
/// failure logs a warning and must NEVER block `/new` or exit.
async fn run_close_extraction(
    enabled: bool,
    ephemeral: bool,
    turns: usize,
    memory: &mut newt_core::MemoryManager,
    complete: &newt_core::Summarizer,
) -> Option<String> {
    if !should_extract_on_close(enabled, ephemeral, turns) {
        return None;
    }
    let transcript = render_extraction_transcript(&memory.build_messages("", ""))?;
    let reply = match complete(build_extraction_prompt(&transcript)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "close-time note extraction failed — moving on");
            return None;
        }
    };
    let bullets = parse_extraction_bullets(&reply);
    if bullets.is_empty() {
        return None;
    }
    let (mut saved, mut rejected) = (0usize, 0usize);
    for bullet in &bullets {
        match memory.add_note(&format!("{EXTRACTED_NOTE_PREFIX}{bullet}")) {
            Ok(()) => saved += 1,
            Err(e) => {
                rejected += 1;
                tracing::warn!(error = %e, "extracted note rejected — dropped");
            }
        }
    }
    Some(close_extraction_notice(saved, rejected))
}

/// One-line banner naming the active profile and the techniques it composes —
/// printed once at session start so the operator sees what the profile turned on.
fn announce_profile(
    name: &str,
    profile: &newt_core::config::ProfileConfig,
    via: &newt_core::config::PickVia,
    color: bool,
) {
    let techs = if profile.techniques.is_empty() {
        "no techniques".to_string()
    } else {
        profile.techniques.join(", ")
    };
    let source = match via {
        newt_core::config::PickVia::Profile => String::new(),
        newt_core::config::PickVia::Bundle(b) => format!(" (via bundle '{b}')"),
        newt_core::config::PickVia::InferredBundle(b) => format!(" (via bundle '{b}', inferred)"),
    };
    if color {
        let _ = execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!("▸ profile '{name}' — {techs}{source}\n")),
            ResetColor,
        );
    } else {
        println!("▸ profile '{name}' — {techs}{source}");
    }
}

/// The session's resolved loadout state, rendered for `/loadout show` — the
/// audit companion to `/config`. Separates what a named loadout *declares* (its
/// `[loadouts.<name>]` axes) from what actually *resolved* this session
/// (backend endpoint + model, the active profile and its provenance, the
/// persona), so an operator can see at a glance whether an axis was overridden
/// by an explicit flag. Pure (no I/O) so it can be unit-tested.
struct LoadoutView<'a> {
    /// Active loadout name (`NEWT_LOADOUT`), if one was selected.
    name: Option<&'a str>,
    /// The named loadout's declared composition, if it resolves in config.
    loadout: Option<&'a newt_core::Loadout>,
    /// Resolved backend endpoint + model in effect this session.
    inf_url: &'a str,
    inf_model: &'a str,
    /// The active profile pick (name + provenance), if any.
    profile_pick: Option<&'a newt_core::config::ProfilePick>,
    /// The active persona/role name, if any.
    persona: Option<&'a str>,
}

impl LoadoutView<'_> {
    fn render(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        match self.name {
            Some(n) => lines.push(format!("Active loadout: {n}")),
            None => lines.push("No loadout active.".to_string()),
        }
        // What the named loadout declared (dispatcher inputs).
        if let Some(l) = self.loadout {
            lines.push("  declared:".to_string());
            for (label, val) in [
                ("provider", l.provider.as_deref()),
                ("model", l.model.as_deref()),
                ("kit", l.kit.as_deref()),
                ("profile", l.profile.as_deref()),
                ("role", l.role.as_deref()),
            ] {
                if let Some(v) = val {
                    lines.push(format!("    {label:<9}{v}"));
                }
            }
            if let Some(s) = &l.settings {
                if let Some(n) = s.num_ctx {
                    lines.push(format!("    {:<9}{n}", "num_ctx"));
                }
                if let Some(f) = &s.framing {
                    lines.push(format!("    {:<9}{f}", "framing"));
                }
            }
        } else if let Some(n) = self.name {
            lines.push(format!("  (no [loadouts.{n}] in config)"));
        }
        // What actually resolved this session (the effect each axis had).
        lines.push("  resolved:".to_string());
        lines.push(format!(
            "    {:<9}{} @ {}",
            "backend", self.inf_model, self.inf_url
        ));
        let profile_line = match self.profile_pick {
            Some(p) => {
                let via = match &p.via {
                    newt_core::config::PickVia::Profile => String::new(),
                    newt_core::config::PickVia::Bundle(b) => format!(" (via bundle '{b}')"),
                    newt_core::config::PickVia::InferredBundle(b) => {
                        format!(" (via bundle '{b}', inferred)")
                    }
                };
                format!("{}{}", p.name, via)
            }
            None => "(none)".to_string(),
        };
        lines.push(format!("    {:<9}{profile_line}", "profile"));
        lines.push(format!(
            "    {:<9}{}",
            "persona",
            self.persona.unwrap_or("(none)")
        ));
        lines.join("\n")
    }
}

/// The `verify_gate` technique (R2), post-turn: resolve the produced Python files'
/// imports against the workspace's FFI surface and return a one-block warning of
/// any that import modules absent from it — `None` when clean, or when the
/// workspace has no PyO3 surface to check against. **Non-destructive** (it
/// surfaces fabrications; revert/retry is a later increment). The `surface_match`
/// strictness comes from the profile knob.
///
/// Scope: resolves against the project's PyO3 surface + the Python stdlib — apt
/// for the binding-examples workflow it was built for; a broader Python project
/// may see informational false positives (hence warn-only, opt-in per profile).
fn verify_gate_summary(
    workspace: &str,
    mode: newt_core::verify_gate::SurfaceMatch,
) -> Option<String> {
    let ws = std::path::Path::new(workspace);
    let manifest = newt_core::ffi_manifest::FfiManifest::from_workspace(ws).ok()?;
    if manifest.is_empty() {
        return None; // no authoritative surface to check against
    }
    let report =
        newt_core::verify_gate::gate_python_workspace_with(ws, &manifest.known_modules(), mode)
            .ok()?;
    if report.accept() {
        return None;
    }
    let mut s = format!(
        "verify_gate: {} file(s) import modules not in the workspace surface (the real paths \
         are in the system prompt):",
        report.revert_set().len()
    );
    for f in &report.files {
        if f.is_clean() {
            continue;
        }
        let mods: Vec<&str> = f.fabrications.iter().map(|x| x.module.as_str()).collect();
        s.push_str(&format!(
            "\n    {}  [{}]",
            f.path.display(),
            mods.join(", ")
        ));
    }
    Some(s)
}

/// The result of the `retry` technique's post-turn pass: what was reverted (for the
/// `↩` banner) and the grounded corrective re-prompt to feed the model if the
/// re-prompt budget allows (increment 2b).
struct RevertAction {
    banner: String,
    corrective: String,
    /// Workspace-relative paths actually restored by the governed write
    /// ledger. Used to append compensating prompt artifacts without claiming
    /// access to the restored bytes.
    reverted: Vec<std::path::PathBuf>,
}

/// What the loop should do after a revert, given the remaining re-prompt budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryStep {
    /// Re-prompt the model (the loop queues the corrective turn); budget decremented.
    Reprompt,
    /// The cap is spent — leave the files reverted and report honestly.
    GiveUp,
}

/// Pure decision: with `budget` re-prompts left after a revert, do we re-prompt or
/// give up? Split out so the cap/give-up logic is unit-tested independently of the
/// interactive loop (the headless equivalent is [`apply_revert_retry`]'s own loop).
fn retry_step(budget: u32) -> RetryStep {
    if budget > 0 {
        RetryStep::Reprompt
    } else {
        RetryStep::GiveUp
    }
}

/// The `retry` technique's post-turn pass: gate the produced `.py` files, **revert**
/// the flagged set to its pre-turn state via the per-turn write `ledger`, and return
/// the banner + the grounded corrective re-prompt (`None` when clean / no surface /
/// nothing newt wrote was flagged).
///
/// The ledger records only newt's own `write_file`/`edit_file` writes this turn, so
/// [`revert_only`](newt_core::verify_gate::revert_only) restores edited files and
/// removes created ones **without ever touching a file newt did not write** (build
/// output, pre-existing fabrications, anything reached via a symlink). The caller
/// decides whether to act on `corrective` based on the re-prompt budget
/// ([`retry_step`]).
async fn retry_revert(
    workspace: &str,
    mode: newt_core::verify_gate::SurfaceMatch,
    ledger: &std::cell::RefCell<newt_core::verify_gate::WriteLedger>,
) -> Option<RevertAction> {
    use newt_core::verify_gate::{
        corrective_prompt, gate_python_workspace_with, revert_only, RetrySurface,
    };
    let ws = std::path::Path::new(workspace);
    let manifest = newt_core::ffi_manifest::FfiManifest::from_workspace(ws).ok()?;
    if manifest.is_empty() {
        return None; // no authoritative surface to gate against
    }
    let modules = manifest.known_modules();
    let report = gate_python_workspace_with(ws, &modules, mode).ok()?;
    if report.accept() {
        return None;
    }
    // Per-file fabricated modules for the banner, captured before the report moves.
    let mods_by_path: std::collections::BTreeMap<std::path::PathBuf, String> = report
        .files
        .iter()
        .filter(|f| !f.is_clean())
        .map(|f| {
            let mods: Vec<&str> = f.fabrications.iter().map(|x| x.module.as_str()).collect();
            (f.path.clone(), mods.join(", "))
        })
        .collect();
    let block = manifest.render_block();
    // The grounded re-prompt, built before the report is consumed by revert_only.
    let corrective = corrective_prompt(&report, &block);
    let surface = RetrySurface {
        modules: &modules,
        mode,
        block: &block,
    };
    let outcome = revert_only(ws, &surface, ledger, report).await.ok()?;
    if outcome.reverted.is_empty() {
        return None; // every flagged file was something newt did not write — left as-is
    }
    let mut detail = String::new();
    for p in &outcome.reverted {
        let mods = mods_by_path.get(p).map(String::as_str).unwrap_or("");
        detail.push_str(&format!("\n    {}  [{}]", p.display(), mods));
    }
    let banner = format!(
        "retry: reverted {} file(s) newt wrote this turn to pre-turn state:{detail}",
        outcome.reverted.len()
    );
    Some(RevertAction {
        banner,
        corrective,
        reverted: outcome.reverted,
    })
}

/// The inference backend the TUI session should talk to — a flat split of
/// immutable INTENT (what config + operator asked for, captured at resolve
/// time and never touched by probes) and mutable ROUTE (what probing and
/// adoption established since). Adoption evidence survives refreshes via
/// [`merge_refresh`], and overlays never destroy the declaration they
/// overlaid.
#[derive(Debug)]
pub(crate) struct BackendChoice {
    /// The configured backend's name ("" for env-synthesized/legacy choices) —
    /// feeds cap_key (instance keying) and honest status lines.
    pub(crate) name: String,
    pub(crate) url: String,
    // ── immutable intent ─────────────────────────────────────────────
    /// The backend's declared model, if any.
    pub(crate) declared_model: Option<String>,
    /// The operator's explicit per-session request (`NEWT_DGX_MODEL`),
    /// kept apart from the declaration so adoption can fall back to the
    /// declared model when the request is unavailable.
    pub(crate) requested_model: Option<String>,
    /// The DECLARED serving axis (declaration layer + the core's
    /// destination normalization: a model_path route is Instance even when
    /// the declaration omitted the axis).
    pub(crate) declared_serving: Option<newt_core::Serving>,
    /// The explicit `--backend-serving` REQUEST for this invocation, if
    /// any — operator intent, outranking every cached observation.
    pub(crate) requested_serving: Option<newt_core::Serving>,
    /// The config-declared wire protocol (`None` = probe at connect).
    pub(crate) configured_kind: Option<newt_core::BackendKind>,
    /// The config-declared OpenAI surface (`None` = probe at connect).
    pub(crate) configured_api: Option<newt_core::OpenAiApi>,
    /// Managed mode — lets adoption prefer a warm model on a Shared box.
    pub(crate) managed: Option<newt_core::ManagedMode>,
    pub(crate) api_key: Option<String>,
    /// The embedded artifact path, when this backend routes to a local
    /// model file instead of an endpoint — the other destination axis.
    pub(crate) model_path: Option<String>,
    // ── mutable route ────────────────────────────────────────────────
    /// The model the session is actually driving: request > declaration at
    /// resolve time; adoption replaces it with served reality — INCLUDING
    /// clearing it when a live multiplexer establishes no pick (`None` is
    /// truthful; display falls back through [`Self::display_model`]).
    pub(crate) active_model: Option<String>,
    /// The serving axis a CACHED probe observation attests (the receipt's
    /// observation layer) — provenance kept separate from live adoption,
    /// so a fresh resolution's cache always lands and the two can never
    /// overwrite each other.
    pub(crate) observed_serving: Option<newt_core::Serving>,
    /// The serving axis LIVE adoption/probing established THIS session —
    /// written only by the adopt path, the strongest route evidence.
    pub(crate) adopted_serving: Option<newt_core::Serving>,
    pub(crate) kind: newt_core::BackendKind,
    /// True when the config omitted `kind` — adopt must run `detect_endpoint`
    /// instead of trusting a placeholder wire protocol.
    pub(crate) kind_needs_probe: bool,
    /// For an OpenAI backend: which HTTP surface (chat/completions vs the newer
    /// /v1/responses). Surfaced to the agent loop via `NEWT_OPENAI_API`.
    pub(crate) api: newt_core::OpenAiApi,
    /// True when the config omitted `api` — adopt must run `detect_openai_api`
    /// for OpenAI backends instead of trusting the chat_completions placeholder.
    pub(crate) api_needs_probe: bool,
    /// The server-declared context window (#1199), probed FRESH at session-start
    /// adopt — not read from the persisted cache (which could hold a stale
    /// None). `None` when the API can't be asked; the budget then falls back to
    /// config / the learned cache.
    pub(crate) context_window: Option<u32>,
    // ── capability layers + display history ──────────────────────────
    /// The immutable capability layers (card binding + inline), constructed
    /// once per resolution by `ResolvedCapabilities::resolve` from the
    /// config's pre-overlay binding seed.
    pub(crate) capabilities: newt_core::model_card::ResolvedCapabilities,
    /// A card-resolution error minted by THIS resolution (unknown/invalid
    /// card) — held for identity-compared display, never destroyed.
    pub(crate) card_resolution_error: Option<String>,
    /// What the card display owner last SHOWED — survives refreshes so an
    /// unchanged state stays quiet.
    pub(crate) notices: CardNotices,
}

impl BackendChoice {
    /// A choice with no config-backed intent — the env-synthesized/legacy
    /// arms and tests. Route starts at `active_model` with nothing adopted;
    /// callers set the request/declaration facts they actually have.
    pub(crate) fn synthesized(
        name: &str,
        url: String,
        kind: newt_core::BackendKind,
        active_model: Option<String>,
    ) -> Self {
        Self {
            name: name.to_string(),
            url,
            declared_model: None,
            requested_model: None,
            declared_serving: None,
            requested_serving: None,
            observed_serving: None,
            configured_kind: None,
            configured_api: None,
            managed: None,
            api_key: None,
            model_path: None,
            active_model: active_model.filter(|m| !m.trim().is_empty()),
            adopted_serving: None,
            kind,
            kind_needs_probe: false,
            api: newt_core::OpenAiApi::default(),
            api_needs_probe: false,
            context_window: None,
            capabilities: newt_core::model_card::ResolvedCapabilities::none(),
            card_resolution_error: None,
            notices: CardNotices::default(),
        }
    }

    /// The serving axis the session currently operates under, by
    /// PROVENANCE strength: live adoption > explicit request > cached
    /// observation > declaration (destination-normalized).
    pub(crate) fn route_serving(&self) -> Option<newt_core::Serving> {
        self.adopted_serving
            .or(self.requested_serving)
            .or(self.observed_serving)
            .or(self.declared_serving)
    }

    /// The route's typed destination — where the session's bytes go: the
    /// endpoint, or the embedded artifact path. Feeds the destination-first
    /// capability decision ([`newt_core::model_card::ResolvedCapabilities::for_route`]).
    pub(crate) fn route_destination(&self) -> newt_core::BackendDestination {
        newt_core::BackendDestination::new(Some(self.url.clone()), self.model_path.clone())
    }

    /// The label for status lines: the live route model, else the declared
    /// intent, else "(server decides)". DISPLAY ONLY — never an identity.
    pub(crate) fn display_model(&self) -> &str {
        self.active_model
            .as_deref()
            .or(self.declared_model.as_deref())
            .unwrap_or("(server decides)")
    }

    /// The serving principal this choice currently represents, for the
    /// capability decision.
    ///
    /// * An established/declared **Instance** (or a config-declared embedded
    ///   engine) is a single artifact.
    /// * A **Multiplexer** with a live model is that model; with none yet,
    ///   Unknown (a half-initialized choice must not report a retarget it
    ///   cannot know about).
    /// * **No axis at all**: an operator-SELECTED identity (request, else
    ///   declaration) justifies exact association — an adopted guess never
    ///   does, so `active_model` is deliberately not consulted here.
    pub(crate) fn principal(&self) -> newt_core::model_card::ServingPrincipal<'_> {
        use newt_core::model_card::ServingPrincipal as P;
        match self.route_serving() {
            Some(newt_core::Serving::Instance) => P::Instance,
            Some(newt_core::Serving::Multiplexer) => {
                match self
                    .active_model
                    .as_deref()
                    .filter(|m| !m.trim().is_empty())
                {
                    Some(m) => P::MultiplexerModel(m),
                    // A live multiplexer with no established pick: Unknown —
                    // a stale declared/requested label must not become the
                    // principal a card activates against.
                    None => P::Unknown,
                }
            }
            // An embedded route (a model_path destination) serves ONE
            // artifact — derived from the TYPED destination, never from a
            // possibly-stale declared kind beside an endpoint.
            None if self.url.is_empty()
                && self.model_path.as_deref().is_some_and(|p| !p.is_empty()) =>
            {
                P::Instance
            }
            None => match self
                .requested_model
                .as_deref()
                .or(self.declared_model.as_deref())
                .filter(|m| !m.trim().is_empty())
            {
                Some(m) => P::SelectedModel(m),
                None => P::Unknown,
            },
        }
    }

    /// The capability decision for the CURRENT principal — computed at use
    /// time, never cached across a serving/model change, so a rebuilt or
    /// refreshed choice cannot re-enable a card the current model does not
    /// match.
    pub(crate) fn capability_decision(&self) -> newt_core::model_card::CapabilityDecision {
        self.capabilities
            .for_route(&self.route_destination(), self.principal())
    }

    /// Observe + render card-layer state at a printing seam — THE display
    /// owner. Compares typed applicability identity and the resolution
    /// error against what was last shown, records the new history, and
    /// returns only the lines worth printing (nothing on a no-op).
    pub(crate) fn card_notice_lines(&mut self) -> Vec<String> {
        let now = self.capability_decision().applicability().clone();
        let (next, lines) =
            notice_transition(&self.notices, self.card_resolution_error.as_deref(), &now);
        self.notices = next;
        lines
    }
}

/// The display history for card-layer notices — what has been SHOWN, so
/// every seam (startup, adoption, refresh) dedupes by TYPED identity
/// instead of prose comparison or destructive `take()`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CardNotices {
    last_error: Option<String>,
    last_applicability: Option<newt_core::model_card::CardApplicability>,
}

/// Pure transition function: compare what is now true against what was last
/// shown; return the updated history and the lines worth printing.
/// Identity rules: an identical `Inactive(A,B)` stays quiet;
/// `Inactive(…,B) → Inactive(…,C)` is a new transition; `Inactive → Active`
/// announces the card re-applying; a resolution error prints once per
/// distinct message.
fn notice_transition(
    prev: &CardNotices,
    error: Option<&str>,
    now: &newt_core::model_card::CardApplicability,
) -> (CardNotices, Vec<String>) {
    use newt_core::model_card::CardApplicability as A;
    let mut lines = Vec::new();
    if let Some(e) = error {
        if prev.last_error.as_deref() != Some(e) {
            lines.push(e.to_string());
        }
    } else if prev.last_error.is_some() {
        // Error RECOVERY is a transition too: a fixed card must not just
        // silently start applying after sessions of error lines.
        lines.push("card resolution recovered — the configured card resolves again".to_string());
    }
    // Applicability transitions render EXHAUSTIVELY over typed identity —
    // any change in which card (or whether one) governs card-derived
    // behavior is visible: replacement, removal, activation, deactivation,
    // and every inactive shape. Only two states stay deliberately quiet:
    // an unchanged identity (dedupe), and the very first observation when
    // the card simply applies (startup Active needs no banner).
    if prev.last_applicability.as_ref() != Some(now) {
        match (prev.last_applicability.as_ref(), now) {
            // Card replaced: A → B, both applying.
            (Some(A::Active { card: before }), A::Active { card: after }) if before != after => {
                lines.push(format!(
                    "card switched: `{before}` → `{after}` — card-derived behavior now \
                     follows `{after}`"
                ));
            }
            // Re-activation after any non-applying shape.
            (
                Some(A::InactiveModel { .. } | A::InactiveDestination { .. } | A::Undecided { .. }),
                A::Active { card },
            ) => {
                lines.push(format!(
                    "card `{card}` applies again — the session is serving its bound model"
                ));
            }
            // Activation from an explicitly card-less state (mid-session:
            // a card was configured/bound where none governed before).
            (Some(A::None), A::Active { card }) => {
                lines.push(format!(
                    "card `{card}` now applies — card-derived behavior follows it"
                ));
            }
            // First observation: a card that simply applies at startup
            // stays quiet; anything else renders its prose below.
            (None, A::Active { .. }) => {}
            // Card removed / binding gone: card-derived behavior is off.
            (Some(prev_state), A::None) if !matches!(prev_state, A::None) => {
                lines.push(
                    "card removed — no card governs this backend; card-derived behavior \
                     is off (inline backend settings kept)"
                        .to_string(),
                );
            }
            // Every non-applying shape has prose of its own.
            _ => {
                if let Some(line) = applicability_prose(now) {
                    lines.push(line);
                }
            }
        }
    }
    (
        CardNotices {
            last_error: error.map(str::to_string),
            last_applicability: Some(now.clone()),
        },
        lines,
    )
}

/// The operator-facing prose for a non-applying card state; `None` for the
/// quiet states. THE one prose owner — core keeps every outcome typed
/// (renderer-neutrality, #1803), and every display seam renders through
/// here, never its own copy. Public: headless `solve` (newt-cli) renders
/// its stderr line through this same owner.
pub fn applicability_prose(a: &newt_core::model_card::CardApplicability) -> Option<String> {
    use newt_core::model_card::CardApplicability as A;
    match a {
        A::None | A::Active { .. } => None,
        A::InactiveModel {
            card,
            bound_model,
            active_model,
        } => Some(format!(
            "card `{card}` is bound to {} — the session is serving {active_model}, so \
             card-derived behavior (capabilities and family policy) is off (inline \
             backend settings kept); rebind the card or switch back",
            bound_model.as_deref().unwrap_or("(no declared model)"),
        )),
        A::InactiveDestination {
            card,
            bound_destination,
            active_destination,
        } => Some(format!(
            "card `{card}` is bound at {} — the session is routed to {}, so \
             card-derived behavior (capabilities and family policy) is off (inline \
             backend settings kept)",
            describe_destination(bound_destination),
            describe_destination(active_destination),
        )),
        A::Undecided { card } => Some(format!(
            "card `{card}` is configured but the serving principal is not established — \
             card-derived behavior (capabilities and family policy) stays off until \
             adoption decides"
        )),
        // Core's applicability enum is non-exhaustive-shaped by policy: an
        // unrecognized future state renders conservatively rather than
        // silently.
        #[allow(unreachable_patterns)]
        other => Some(format!("card state changed: {other:?}")),
    }
}

/// One destination, for prose: the endpoint or the artifact path.
fn describe_destination(d: &newt_core::BackendDestination) -> String {
    d.endpoint
        .clone()
        .or_else(|| d.model_path.clone())
        .unwrap_or_else(|| "(no destination)".into())
}

#[cfg(test)]
#[path = "lib_tests/notice_transition_tests.rs"]
mod notice_transition_tests;

/// The complete INTENT key: two resolutions with equal intent describe the
/// same operator ask, so the established route may carry across a refresh.
fn same_intent(a: &BackendChoice, b: &BackendChoice) -> bool {
    a.name == b.name
        && a.url == b.url
        && a.declared_model == b.declared_model
        && a.requested_model == b.requested_model
        && a.declared_serving == b.declared_serving
        && a.requested_serving == b.requested_serving
        && a.configured_kind == b.configured_kind
        && a.configured_api == b.configured_api
        && a.managed == b.managed
        && a.api_key == b.api_key
}

/// Fold a fresh resolution over the previous choice. On a same-intent no-op
/// ONLY the established route/probe results carry over; the freshly
/// resolved capabilities and resolution error always stand (a config edit
/// must be able to fix a card without a restart); display history always
/// survives. Returns whether adoption needs to (re)run.
fn merge_refresh(prev: &BackendChoice, next: &mut BackendChoice) -> bool {
    next.notices = prev.notices.clone();
    if !same_intent(prev, next) {
        return true;
    }
    next.kind = prev.kind;
    next.kind_needs_probe = prev.kind_needs_probe;
    next.api = prev.api;
    next.api_needs_probe = prev.api_needs_probe;
    // ONLY the live-adopted evidence carries; the fresh resolution's CACHED
    // observation (observed_serving) always stands — a prior offline
    // session's None must never erase a newly written probe cache.
    next.adopted_serving = prev.adopted_serving;
    next.active_model = prev.active_model.clone();
    next.context_window = prev.context_window;
    false
}

/// Adoption inputs from the choice's IMMUTABLE intent: the synthesized view
/// carries the DECLARED model / configured serving / managed mode — never
/// the session override — while the override rides separately as the
/// REQUEST. `adopt()` then owns the policy: an unavailable request falls
/// back to the declaration, and a Managed Shared backend may prefer a warm
/// model.
fn adoption_inputs(choice: &BackendChoice) -> (newt_core::BackendConfig, Option<String>) {
    (
        newt_core::BackendConfig {
            name: choice.name.clone(),
            endpoint: choice.url.clone(),
            model: choice.declared_model.clone(),
            kind: Some(choice.kind),
            serving: choice
                .requested_serving
                .or(choice.observed_serving)
                .or(choice.declared_serving),
            managed: choice.managed,
            ..Default::default()
        },
        choice.requested_model.clone(),
    )
}

/// Adoption mutates ONLY the route: the established serving axis and the
/// active model. Intent fields are never written.
fn apply_adoption(choice: &mut BackendChoice, adoption: &newt_core::backend_probe::Adoption) {
    choice.adopted_serving = Some(adoption.serving);
    // Unconditional: a multiplexer that established NO pick clears the
    // route model — a stale declared/requested label must not survive as
    // the principal a card could activate against.
    choice.active_model = adoption.model.clone().filter(|m| !m.trim().is_empty());
}

/// The session-start ready preamble. Includes the backend wire protocol
/// (`ollama`/`openai`) so it's unambiguous which engine the endpoint speaks —
/// e.g. an Ollama `:11434` vs an OpenAI-compatible (vLLM) endpoint. Pure for
/// testing.
fn ready_line(version: &str, model: &str, url: &str, kind: newt_core::BackendKind) -> String {
    format!(
        "v{version} ready — {model} @ {url} ({}){}  (Ctrl-D or /exit to quit)",
        kind.label(),
        tenacity_indicator(newt_core::tenacity::effective_tenacity())
    )
}

/// The preamble tenacity indicator (#12): shown only when tenacity is ELEVATED
/// above the behaviour-preserving `Standard`, so an operator sees at a glance
/// that action-forcing is dialled up (`· tenacity: relentless`). Empty at
/// `Standard` to keep the default line clean. Pure — unit-tested directly.
fn tenacity_indicator(t: newt_core::Tenacity) -> String {
    if t == newt_core::Tenacity::Standard {
        String::new()
    } else {
        format!(" · tenacity: {}", t.label())
    }
}

fn should_probe_openai_surface(
    kind: newt_core::BackendKind,
    api_needs_probe: bool,
    model: &str,
    url: &str,
    serving: Option<newt_core::Serving>,
) -> bool {
    kind == newt_core::BackendKind::Openai
        && api_needs_probe
        && !model.is_empty()
        && (url.starts_with("https://") || serving == Some(newt_core::Serving::Instance))
}

/// Resolve where the semantic embedder sends requests: `(url, protocol, key)`.
/// An explicit `embeddings_endpoint` (with its protocol; no inherited key)
/// decouples embeddings from chat — point it at a real embeddings host while
/// chat runs on a vLLM coder. Unset → fall back to the active backend
/// (back-compat), now protocol-aware so an OpenAI backend uses `/v1/embeddings`.
/// Pure for testing.
fn resolve_embeddings_target(
    cfg: &newt_core::SemanticConfig,
    inf_url: &str,
    inf_kind: newt_core::BackendKind,
    inf_key: Option<&str>,
) -> (String, newt_core::BackendKind, Option<String>) {
    match cfg.embeddings_endpoint.clone() {
        Some(url) => (
            url,
            cfg.embeddings_api.unwrap_or(newt_core::BackendKind::Ollama),
            None,
        ),
        None => (
            inf_url.to_string(),
            cfg.embeddings_api.unwrap_or(inf_kind),
            inf_key.map(str::to_string),
        ),
    }
}

/// Resolve the backend for the TUI. Precedence (unifies the loadout provider
/// axis with the `/backend` live toggle):
///
/// 1. **`NEWT_PROVIDER`** names a `[backends]` entry — the loadout's `provider`
///    axis (Slice 2). The named backend supplies endpoint/kind/auth; `NEWT_DGX_MODEL`
///    (the loadout's `model`) overrides the backend's default model when set.
/// 2. Legacy env shim: `NEWT_DGX_OLLAMA_URL`/`NEWT_DGX_HOST` synthesize an
///    ollama endpoint (one release, #1126).
/// 3. **`default_backend`** (#1130) names the configured start backend.
/// 4. **`NEWT_BACKEND`** (set by `/backend`) forces the openai-vs-ollama *kind*.
/// 5. A sole backend; then the legacy `[dgx]` node shim; then prefer-openai
///    among several; then the localhost fallback.
///
/// `NEWT_PROVIDER` is the most specific (a named entry); `NEWT_BACKEND` is the
/// coarse kind toggle `/backend` uses. A loadout-sourced provider is hard-error-
/// validated upstream ([`newt_core::Loadout::validate`]); a directly-set
/// `NEWT_PROVIDER` naming no backend falls through to (2).
/// The name of the backend the session currently resolves to, for the `◀ active`
/// marker in `/backends`. Prefers an explicit `NEWT_PROVIDER` pin (the exact name
/// `/backends <name>` sets); otherwise matches the resolved endpoint+kind back to
/// a configured `[[backends]]` entry. `None` when nothing matches (e.g. the
/// historical DGX fallback that isn't itself a named backend).
pub(crate) fn active_backend_name(resolved: &newt_core::ResolvedConfig) -> Option<String> {
    if let Some(name) = std::env::var("NEWT_PROVIDER")
        .ok()
        .filter(|s| !s.is_empty())
    {
        if resolved.backends.iter().any(|b| b.name == name) {
            return Some(name);
        }
    }
    let choice = resolve_backend_choice(resolved).ok()?;
    resolved
        .backends
        .iter()
        .find(|b| b.endpoint == choice.url && (b.kind.is_none() || b.kind == Some(choice.kind)))
        .map(|b| b.name.clone())
}

/// Session-start / backend-switch adoption (#1139 C1b, epic #1126): ask the
/// chosen backend what it ACTUALLY serves (bounded ~1s) and adopt served
/// reality via [`newt_core::backend_probe::adopt`]. Returns status lines to
/// print. Offline/timeout → keep the file-hint model and say so — NEVER a
/// silent failover to another backend. Embedded backends are not probed.
///
/// When the config omitted `kind` (`kind_needs_probe`), race `/api/tags` vs
/// `/v1/models` via [`newt_core::backend_probe::detect_endpoint`] first so a
/// minimal `name`+`endpoint` backend still connects.
fn adopt_backend_choice(choice: &mut BackendChoice, prewarm: Option<Prewarm>) -> Vec<String> {
    use newt_core::backend_probe;
    if choice.kind == newt_core::BackendKind::Embedded && !choice.kind_needs_probe {
        return Vec::new();
    }
    let lines = Vec::new();
    // Splash-first pre-warm: a probe started at splash entry for THIS
    // endpoint means the answer is already in flight (or done) — await it
    // instead of probing cold. Gated on the URL still matching (a first-run
    // wizard may have rewritten the config since the probe was spawned) and
    // fail-soft: a failed/mismatched pre-warm falls through to the cold path.
    let prewarmed = prewarm
        .filter(|pw| prewarm_applies(&choice.url, &pw.url))
        .and_then(|pw| {
            tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(pw.handle))
                .ok()
                .flatten()
        });
    if let Some(probe) = prewarmed {
        let detected = if choice.kind_needs_probe {
            choice.kind = probe.kind;
            choice.kind_needs_probe = false;
            if choice.route_serving().is_none() {
                choice.adopted_serving = Some(probe.serving);
            }
            Some(probe.kind)
        } else {
            None
        };
        return finish_adoption(choice, lines, probe.models, probe.warm, detected);
    }
    let secs = probe_timeout_secs(&choice.url);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(secs))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    // run_chat is sync inside the tokio runtime — bridge like wizard.rs does.
    let fetched = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            if choice.kind_needs_probe {
                match backend_probe::detect_endpoint(
                    &client,
                    &choice.url,
                    choice.api_key.as_deref(),
                )
                .await
                {
                    Ok(probe) => {
                        choice.kind = probe.kind;
                        choice.kind_needs_probe = false;
                        if choice.route_serving().is_none() {
                            choice.adopted_serving = Some(probe.serving);
                        }
                        Ok((probe.models, probe.warm, Some(probe.kind)))
                    }
                    Err(e) => Err(e),
                }
            } else {
                let api = backend_probe::api_for(choice.kind);
                match api
                    .list_models(&client, &choice.url, choice.api_key.as_deref())
                    .await
                {
                    Ok(models) => {
                        // Warmth refines the no-preference fallback in adopt():
                        // a model already resident answers immediately, install
                        // order says nothing. Fail-soft (None → empty).
                        let warm = api
                            .warm_models(&client, &choice.url, choice.api_key.as_deref())
                            .await
                            .unwrap_or_default();
                        Ok((models, warm, None))
                    }
                    Err(e) => Err(e),
                }
            }
        })
    });
    match fetched {
        Ok((models, warm, detected_kind)) => {
            finish_adoption(choice, lines, models, warm, detected_kind)
        }
        Err(e) => offline_adoption(choice, lines, e),
    }
}

/// The shared adopt tail: probe results (live or pre-warmed) → the choice's
/// model/serving/window/api, with the honest status lines.
fn finish_adoption(
    choice: &mut BackendChoice,
    mut lines: Vec<String>,
    models: Vec<String>,
    warm: Vec<String>,
    detected_kind: Option<newt_core::BackendKind>,
) -> Vec<String> {
    use newt_core::backend_probe::{self, Served};
    let secs = probe_timeout_secs(&choice.url);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(secs))
        .build()
    {
        Ok(c) => c,
        Err(_) => return lines,
    };
    {
        {
            if let Some(kind) = detected_kind {
                lines.push(format!(
                    "detected {} at {} — {} model(s)",
                    kind.label(),
                    choice.url,
                    models.len()
                ));
            }
            // Adoption inputs from the choice's IMMUTABLE intent: the
            // declared model rides in the synthesized view, the operator's
            // per-session request rides separately — `adopt()` owns the
            // fallback policy (an unavailable request falls back to the
            // declaration; Managed Shared may prefer a warm model).
            let (synth, requested) = adoption_inputs(choice);
            let adoption =
                backend_probe::adopt(&synth, &Served { models, warm }, requested.as_deref());
            if adoption.requested_unavailable {
                // #1122 fail-soft: a restored/typo'd model must not brick the
                // session — say what happened and what we used instead.
                lines.push(format!(
                    "requested model isn't on {} — falling back (was it a typo, or \
                     removed from the endpoint?); /models to list",
                    choice.url
                ));
            }
            if adoption.model.is_none() {
                lines.push(format!(
                    "{} listed no models — pull one (or start the server with a model), \
                     then /models",
                    choice.url
                ));
            } else if adoption.requested_ignored {
                lines.push(format!(
                    "model is fixed by this {} instance: {} — restart the server \
                     with another model, or /backends to switch endpoints",
                    choice.kind.label(),
                    adoption.model.as_deref().unwrap_or_default()
                ));
            }
            // Adoption mutates ONLY the route — including CLEARING the
            // active model when a live multiplexer established no pick.
            apply_adoption(choice, &adoption);
            // #1199: auto-detect the context window from the SERVER, fresh —
            // vLLM's max_model_len / Ollama's /api/show. Held on the choice and
            // fed to the budget; never read from the persisted cache (which
            // could pin a stale None and starve a 256k model).
            let window_model = choice.active_model.clone().unwrap_or_default();
            choice.context_window = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    backend_probe::api_for(choice.kind)
                        .context_window(
                            &client,
                            &choice.url,
                            &window_model,
                            choice.api_key.as_deref(),
                        )
                        .await
                })
            });
            // OpenAI surface probe: absent `api` → try chat/completions, adopt
            // `responses` when the server says the model is responses-only.
            let mut api_was_probed = false;
            // Avoid forcing a model load on plain-HTTP multiplexers. An HTTPS
            // provider or a fixed-model instance can be capability-probed
            // without reviving the old llama-swap startup timeout.
            if should_probe_openai_surface(
                choice.kind,
                choice.api_needs_probe,
                &window_model,
                &choice.url,
                choice.route_serving(),
            ) {
                match tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        backend_probe::detect_openai_api(
                            &client,
                            &choice.url,
                            &window_model,
                            choice.api_key.as_deref(),
                        )
                        .await
                    })
                }) {
                    Ok(api) => {
                        choice.api = api;
                        choice.api_needs_probe = false;
                        api_was_probed = true;
                        lines.push(format!(
                            "detected api={} for {} @ {}",
                            api.label(),
                            choice.display_model(),
                            choice.url
                        ));
                    }
                    Err(e) => lines.push(format!(
                        "api probe failed ({e:#}) — using chat_completions until it answers"
                    )),
                }
            }
            // Persist what the probe OBSERVED through the typed machine
            // channel — probe_v1 files only; an operator-owned same-name
            // drop-in is skipped byte-for-byte and the skip is VISIBLE.
            if !choice.name.is_empty() && (detected_kind.is_some() || api_was_probed) {
                let serving = match choice.route_serving() {
                    // Only an Instance's model is backend truth; a
                    // multiplexer's pick is per-session and has no field to
                    // persist through.
                    Some(newt_core::Serving::Instance) => newt_core::ProbedServing::Instance {
                        model: choice.active_model.clone(),
                    },
                    Some(newt_core::Serving::Multiplexer) => newt_core::ProbedServing::Multiplexer,
                    None => newt_core::ProbedServing::Unknown,
                };
                let observation = newt_core::ProbeObservation {
                    name: choice.name.clone(),
                    endpoint: choice.url.clone(),
                    kind: Some(choice.kind),
                    api: (choice.kind == newt_core::BackendKind::Openai).then_some(choice.api),
                    serving,
                };
                match newt_core::persist_probe_observation(&observation) {
                    Ok(newt_core::ProbeWriteback::Written(path)) => lines.push(format!(
                        "wrote probed backend → {} (delete to reset)",
                        path.display()
                    )),
                    Ok(newt_core::ProbeWriteback::SkippedOperatorOwned(path)) => {
                        lines.push(format!(
                            "probe results not persisted — {} is operator-owned \
                             (delete it, or claim/edit it, to let probes write)",
                            path.display()
                        ));
                    }
                    Ok(newt_core::ProbeWriteback::NotWritten) => {}
                    Err(e) => lines.push(format!("could not write backend drop-in: {e}")),
                }
            }
            // Card-layer transitions surface HERE — the adoption seam —
            // through the ONE display owner, deduped by typed identity.
            lines.extend(choice.card_notice_lines());
        }
    }
    lines
}

/// The offline tail: the endpoint is unreachable — keep the file-hint model
/// with an honest line; never fail the session.
fn offline_adoption(
    choice: &mut BackendChoice,
    mut lines: Vec<String>,
    e: anyhow::Error,
) -> Vec<String> {
    match choice.active_model.as_deref() {
        None => lines.push(format!(
            "{} is unreachable ({e:#}) and no model is configured — check the \
             endpoint, then /backends",
            choice.url
        )),
        Some(model) => lines.push(format!(
            "{} is unreachable ({e:#}) — using configured model {model} until it answers",
            choice.url
        )),
    }
    lines
}

/// One item per configured backend for `/backends`: the `name · kind · model @
/// endpoint` label plus whether it's the active one. Pure (the active name is
/// passed in) so it unit-tests without touching the environment; the caller
/// renders each via [`newt_core::agentic::print_list_item`] (the default list
/// style — red `▸`/`◀` sigils + green `active` on the live row).
pub(crate) fn backends_list_items(
    cfg: &newt_core::Config,
    active: Option<&str>,
) -> Vec<(String, bool)> {
    cfg.backends
        .iter()
        .map(|b| {
            let label = format!(
                "{} · {} · {} @ {}",
                b.name,
                b.kind_label(),
                b.effective_model().unwrap_or("(server decides)"),
                b.endpoint
            );
            (label, active == Some(b.name.as_str()))
        })
        .collect()
}

pub(crate) fn resolve_backend_choice(
    resolved: &newt_core::ResolvedConfig,
) -> Result<BackendChoice, String> {
    let session_model = || {
        std::env::var("NEWT_DGX_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
    };
    let from_backend = |rb: newt_core::ResolvedBackend<'_>| -> Result<BackendChoice, String> {
        let b = rb.backend;
        let receipt = rb.receipt;
        // The chat driver is HTTP-only: an embedded (model_path) backend —
        // judged on the FLATTENED, core-normalized view, never a stale
        // declared kind — is a typed refusal, exactly as in solve. No
        // silent fallback to another backend, no empty-URL HTTP session.
        if b.endpoint.is_empty() || b.kind == Some(newt_core::BackendKind::Embedded) {
            return Err(format!(
                "backend `{}` is an embedded (model_path) backend — chat drives HTTP \
                 backends only; select an HTTP backend (/backends), or serve the \
                 artifact behind an HTTP endpoint",
                b.name
            ));
        }
        // ONE sidecar construction per choice, from the receipt's BINDING
        // evidence (pre-overlay: a CLI/session model override retargets the
        // session, never the card). An unknown named card is a real config
        // error but choice resolution is infallible by contract (a broken
        // backend list must not crash the TUI) — so the error becomes a
        // VISIBLE notice at the next printing seam and the capabilities
        // fall to the conservative inline-only floor.
        let seed = receipt.binding.clone();
        let pinned = newt_core::Config::pinned_config_path();
        let (caps, card_error) =
            match newt_core::model_card::ResolvedCapabilities::resolve(b, &seed, pinned.as_deref())
            {
                Ok(caps) => (caps, None),
                Err(e) => (
                    newt_core::model_card::ResolvedCapabilities::resolve(
                        b,
                        &newt_core::model_card::CardBindingSeed {
                            card: None,
                            ..seed.clone()
                        },
                        None,
                    )
                    .expect("card-less resolution is infallible"),
                    Some(e),
                ),
            };
        // The probe-cached route, straight from the receipt's typed
        // observation — never re-derived from the flattened backend.
        let (observed_serving, _observed_model) = receipt
            .observation
            .as_ref()
            .map(|o| o.serving_axis())
            .unwrap_or((None, None));
        let request = receipt.request.as_ref();
        // The DECLARED axis carries the core's destination normalization: a
        // model_path route IS Instance even when the declaration omitted
        // the axis.
        let declared_serving = receipt.declaration.serving.or_else(|| {
            receipt
                .declaration
                .destination
                .model_path
                .as_deref()
                .is_some_and(|p| !p.is_empty())
                .then_some(newt_core::Serving::Instance)
        });
        Ok(BackendChoice {
            name: b.name.clone(),
            url: b.endpoint.clone(),
            // Immutable intent, from the receipt's DECLARATION layer.
            declared_model: receipt.declaration.model.clone(),
            // The session override (env) is the most-specific request; the
            // CLI --backend-model rides in the receipt's request layer.
            requested_model: session_model()
                .or_else(|| receipt.request.as_ref().and_then(|r| r.model.clone())),
            declared_serving,
            requested_serving: request.and_then(|r| r.serving),
            configured_kind: request.and_then(|r| r.kind).or(receipt.declaration.kind),
            configured_api: request.and_then(|r| r.api).or(receipt.declaration.api),
            managed: receipt.declaration.managed,
            api_key: b.resolve_api_key(),
            model_path: b.model_path.clone(),
            // The route starts at the EFFECTIVE view (session override >
            // flattened model, which already carries the probe-cached
            // Instance model and any CLI request); adoption replaces it
            // with served reality.
            active_model: session_model().or_else(|| b.effective_model().map(str::to_string)),
            // The cached observation keeps its own PROVENANCE slot — the
            // precedence (live > request > cache > declaration) lives in
            // route_serving, not in field blending.
            observed_serving,
            adopted_serving: None,
            kind: b.kind.unwrap_or(newt_core::BackendKind::Ollama),
            kind_needs_probe: b.needs_kind_probe(),
            api: b.api.unwrap_or_default(),
            api_needs_probe: b.api.is_none(),
            context_window: None,
            capabilities: caps,
            card_resolution_error: card_error,
            notices: CardNotices::default(),
        })
    };
    // 1. Explicit selection ($NEWT_PROVIDER / default_backend) through the
    //    SHARED typed contract: unknown, unroutable, and provider outcomes
    //    are HARD errors — never a silent fallback to some other backend
    //    (the pre-#1819 fall-through was exactly the failure mode the typed
    //    selection exists to kill).
    let explicit_selector = std::env::var("NEWT_PROVIDER")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some()
        || resolved
            .default_backend
            .as_deref()
            .is_some_and(|n| !n.is_empty());
    if explicit_selector {
        use newt_core::config::{SelectedBackend, SelectionOutcome};
        match resolved.select_backend() {
            SelectionOutcome::Selected(SelectedBackend::Configured(_)) => {
                let rb = resolved
                    .selected_backend()
                    .expect("the shared index selector just picked a configured backend");
                return from_backend(rb);
            }
            SelectionOutcome::Selected(SelectedBackend::Provider(p)) => {
                return Err(format!(
                    "the backend selection resolves to provider `{}` — chat drives \
                     [[backends]] only; point $NEWT_PROVIDER/default_backend at a \
                     backend (or unset them)",
                    p.name
                ));
            }
            SelectionOutcome::UnknownNamed(name) => {
                return Err(format!(
                    "$NEWT_PROVIDER/default_backend names `{name}`, which matches \
                     nothing configured — fix the selector (chat will not silently \
                     run another backend)"
                ));
            }
            SelectionOutcome::UnroutableNamed(name) => {
                return Err(format!(
                    "$NEWT_PROVIDER/default_backend names `{name}`, which has neither \
                     an endpoint nor a model_path — give it a destination (chat will \
                     not silently run another backend)"
                ));
            }
            // No explicit selector fired inside the contract (e.g. the env
            // var empty) — fall through to the env shims + preference.
            SelectionOutcome::Unset => {}
        }
    }
    // 1.5 Codex-compatible environment (bug/steering-regressions #9): honor
    //     OPENAI_BASE_URL / OPENAI_API_KEY / OPENAI_MODEL the way Codex does,
    //     so `OPENAI_API_KEY=… newt` just works with zero config and
    //     `OPENAI_BASE_URL` can redirect a session at any local-compatible
    //     server. Local-first guard: a bare key only fires when NO backends
    //     are configured — a stray OPENAI_API_KEY in the shell must never
    //     silently reroute a configured setup (and its cost) to OpenAI; an
    //     explicit OPENAI_BASE_URL is a deliberate redirect and always wins.
    {
        let base = std::env::var("OPENAI_BASE_URL").ok();
        let key = std::env::var("OPENAI_API_KEY").ok();
        let model = std::env::var("OPENAI_MODEL").ok();
        if let Some(choice) = codex_env_backend(
            base.as_deref(),
            key.as_deref(),
            model.as_deref(),
            session_model(),
            !resolved.backends.is_empty(),
        ) {
            let detected = [
                base.as_ref().map(|_| "OPENAI_BASE_URL"),
                key.as_ref().map(|_| "OPENAI_API_KEY"),
                model.as_ref().map(|_| "OPENAI_MODEL"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
            if codex_env_allowed(&detected) {
                return Ok(choice);
            }
        }
    }
    // 2. Legacy env shim (one release, #1126): explicit NEWT_DGX_OLLAMA_URL /
    //    NEWT_DGX_HOST synthesize an ollama endpoint, exactly as before.
    let env_url = std::env::var("NEWT_DGX_OLLAMA_URL").ok().or_else(|| {
        std::env::var("NEWT_DGX_HOST").ok().map(|h| {
            let scheme = std::env::var("NEWT_DGX_SCHEME").unwrap_or_else(|_| "http".into());
            let port = std::env::var("NEWT_DGX_OLLAMA_PORT").unwrap_or_else(|_| "11434".into());
            format!("{scheme}://{h}:{port}")
        })
    });
    if let Some(url) = env_url {
        let requested = session_model();
        return Ok(BackendChoice {
            requested_model: requested.clone(),
            ..BackendChoice::synthesized(
                "",
                url,
                newt_core::BackendKind::Ollama,
                requested.or_else(|| Some("llama3.1:8b".into())),
            )
        });
    }
    // 3. NEWT_BACKEND forces the wire kind (`/backend openai|ollama`).
    if let Some(force) = std::env::var("NEWT_BACKEND").ok().filter(|s| !s.is_empty()) {
        let want = if force.eq_ignore_ascii_case("openai") {
            newt_core::BackendKind::Openai
        } else {
            newt_core::BackendKind::Ollama
        };
        if let Some(rb) = resolved.backends().find(|rb| rb.backend.kind == Some(want)) {
            return from_backend(rb);
        }
    }
    // 4. Everything configured — sole / prefer-OpenAI / first ROUTABLE /
    //    provider — delegates to the SHARED typed selection contract, so
    //    chat's fallback can never diverge from solve/worker: a
    //    destination-less first entry is skipped for a later routable one
    //    (the core preference filters by routability), and a provider-only
    //    config is a TYPED refusal, never a silent localhost.
    {
        use newt_core::config::{SelectedBackend, SelectionOutcome};
        match resolved.select_backend() {
            SelectionOutcome::Selected(SelectedBackend::Configured(_)) => {
                let rb = resolved
                    .selected_backend()
                    .expect("the shared index selector just picked a configured backend");
                return from_backend(rb);
            }
            SelectionOutcome::Selected(SelectedBackend::Provider(p)) => {
                return Err(format!(
                    "the backend selection resolves to provider `{}` — chat drives \
                     [[backends]] only; configure a backend (or run the worker)",
                    p.name
                ));
            }
            // Explicit-selector errors were handled in rung 1; if the env
            // changed between the two reads, surface the same typed refusal
            // rather than falling through.
            SelectionOutcome::UnknownNamed(name) => {
                return Err(format!(
                    "$NEWT_PROVIDER/default_backend names `{name}`, which matches \
                     nothing configured — fix the selector"
                ));
            }
            SelectionOutcome::UnroutableNamed(name) => {
                return Err(format!(
                    "$NEWT_PROVIDER/default_backend names `{name}`, which has neither \
                     an endpoint nor a model_path — give it a destination"
                ));
            }
            // Nothing configured qualifies: the chat-only legacy shims below.
            SelectionOutcome::Unset => {}
        }
    }
    // 5. Legacy [dgx] node (one-release shim): configs written by the old
    //    wizard resolve their dgx endpoint + active_model as before.
    if let Some((url, model)) = resolved.dgx.as_ref().and_then(|d| {
        d.nodes.first().and_then(|n| n.ollama.clone()).map(|url| {
            let model = session_model()
                .or_else(|| d.active_model.clone())
                .unwrap_or_else(|| "llama3.1:8b".into());
            (url, model)
        })
    }) {
        let requested = session_model();
        return Ok(BackendChoice {
            requested_model: requested,
            ..BackendChoice::synthesized("", url, newt_core::BackendKind::Ollama, Some(model))
        });
    }
    // 6. Bare fallback: localhost ollama (Config::resolve normally restores
    //    this backend already; this is the belt-and-braces path).
    let requested = session_model();
    Ok(BackendChoice {
        requested_model: requested.clone(),
        ..BackendChoice::synthesized(
            "",
            "http://localhost:11434".into(),
            newt_core::BackendKind::Ollama,
            requested.or_else(|| Some("llama3.1:8b".into())),
        )
    })
}

/// The TUI's config resolution, receipts kept and NOTHING published:
/// process-global settings land only at an accepted-session boundary
/// (startup after the typed backend choice validates; a refresh after it
/// accepts) — a refused session must not publish globals from a route it
/// never accepted. A resolution failure is warned (typed, not silent) and
/// falls back to the AS-IS default config so command surfaces still
/// render; the chat startup path prints its own visible line.
pub(crate) fn resolve_runtime_or_default() -> newt_core::ResolvedConfig {
    newt_core::Config::resolve_runtime_unpublished().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "config resolution failed — running on built-in defaults");
        newt_core::ResolvedConfig::unrequested(newt_core::Config::default())
    })
}

/// Surface the resolved OpenAI API surface to the agent loop via
/// `NEWT_OPENAI_API` (read by the agent loop to route to the Responses path).
/// Called whenever the session (re)resolves its backend, so a `/backends`
/// switch to a `responses` backend takes effect on the next message.
///
/// Public so the headless surfaces (`newt solve` / worker) that reuse the same
/// loop surface `api = "responses"` too — otherwise a responses-only model
/// (gpt-5.6-sol, gpt-5-codex) is driven over `/v1/chat/completions` and 400s on
/// function tools.
pub fn apply_openai_api_env(api: newt_core::OpenAiApi) {
    // Published under the process-env lock (#1850). "Single-threaded session
    // setup" was true of the REPL and false under `cargo test`, which drives
    // this from parallel test threads.
    newt_core::process_env::set_or_remove(
        "NEWT_OPENAI_API",
        match api {
            newt_core::OpenAiApi::Responses => Some("responses"),
            newt_core::OpenAiApi::ChatCompletions => None,
        },
    );
}

/// Re-resolve the active backend from `cfg` + env into the session's live wire
/// locals — adopting served reality when the endpoint or model changed, and
/// republishing the OpenAI api surface. The single owner of "repoint the session
/// to the current backend choice": shared by the post-slash-command refresh and
/// by persona backend routing. Returns whether the endpoint URL changed, so the
/// caller re-probes DGX telemetry only when it matters.
#[allow(clippy::too_many_arguments)]
pub(crate) fn refresh_backend(
    resolved: &newt_core::ResolvedConfig,
    choice: &mut BackendChoice,
    inf_url: &mut String,
    inf_model: &mut String,
    inf_kind: &mut newt_core::BackendKind,
    inf_key: &mut Option<String>,
    inf_context_window: &mut Option<u32>,
    color: bool,
    verbose: bool,
) -> bool {
    let prev_url = inf_url.clone();
    // The typed selection contract can REFUSE (unknown/unroutable/provider
    // explicit selector): print the refusal and keep the current choice — a
    // live session never silently reroutes, and never crashes mid-flight.
    let mut next = match resolve_backend_choice(resolved) {
        Ok(next) => next,
        Err(refusal) => {
            // Typed refusal: keep the current choice AND the current
            // process-globals — a refused route publishes nothing.
            print_newt(&refusal, color, verbose);
            return false;
        }
    };
    // The route validated — THIS is the accepted-session boundary where the
    // re-resolved configuration's process-global settings publish.
    resolved.publish_runtime_settings();
    // Fold the fresh resolution over the previous choice: on a same-intent
    // no-op the established route/probe results carry; fresh capabilities
    // and resolution errors always stand; display history always survives.
    let _intent_changed = merge_refresh(choice, &mut next);
    *choice = next;
    // Adopt served reality only when the ROUTE actually moved (endpoint or
    // model) — the historical trigger: a plain slash command must not
    // re-probe every time, and two same-endpoint backends can swap without
    // a network round-trip (the pin/tab tiers depend on that). A
    // same-intent refresh carried the established route above, so it
    // compares equal here and stays quiet.
    if choice.url != prev_url || choice.active_model.clone().unwrap_or_default() != *inf_model {
        for line in adopt_backend_choice(choice, None) {
            print_newt(&line, color, verbose);
        }
    }
    // Card-layer notices surface HERE — the seam every mid-session
    // backend/model change flows through — via the ONE display owner,
    // deduped by typed identity (an unchanged state stays quiet, every
    // transition is visible, nothing is destructively taken).
    for line in choice.card_notice_lines() {
        print_newt(&line, color, verbose);
    }
    *inf_url = choice.url.clone();
    *inf_model = choice.active_model.clone().unwrap_or_default();
    *inf_kind = choice.kind;
    *inf_key = choice.api_key.clone();
    *inf_context_window = choice.context_window;
    apply_openai_api_env(choice.api);
    // #1139: this is the ONE seam every mid-session model change flows through —
    // `/backends`, `/model`, and persona routing (`apply_persona_backend`) all land
    // here — so attribute the model's FAMILY in one place. TYPED: the family is
    // the resolved card's declared metadata, under the same association gates as
    // the capability decision — never inferred from the model name (the
    // anti-substring law). No associated card family ⇒ no family (the
    // per-family default simply does not engage).
    newt_core::tenacity::set_active_model_family(
        choice
            .capabilities
            .family_for_route(&choice.route_destination(), choice.principal())
            .map(str::to_string),
    );
    *inf_url != prev_url
}

/// The `(NEWT_PROVIDER, NEWT_DGX_MODEL)` a persona's backend routing wants, or
/// `None` when the persona declares no `backend:` (leave the session backend
/// untouched). A persona's `backend` NAMES a `[[backends]]` entry — exactly what
/// `NEWT_PROVIDER` selects; its `model` (if any) maps to the session-model
/// override, else `None` so the backend's own default model applies (clearing
/// the override, as `/backends` does). Pure — the env mutation + re-resolve is
/// the caller's job.
pub(crate) fn persona_provider_env(
    profile: Option<&newt_core::RoleProfile>,
) -> Option<(String, Option<String>)> {
    let backend = profile.and_then(|p| p.backend.as_deref())?;
    let model = profile.and_then(|p| p.model.as_deref()).map(str::to_string);
    Some((backend.to_string(), model))
}

/// Decide a persona's backend route — the pure, validated core of
/// [`apply_persona_backend`]:
/// - `Ok(Some((provider, model)))` — the persona declares a `backend:` that IS in
///   `configured`; set these env values.
/// - `Ok(None)` — the persona declares no backend (or was cleared); revert to the
///   pre-persona baseline.
/// - `Err(name)` — the persona names a backend NOT in `configured`: refuse, so a
///   typo'd / non-portable persona can't silently reroute the session to a
///   fallback (the silent-cost-reroute class the resolver's `NEWT_PROVIDER` rung
///   guards against — it validates before setting the env, and so must we).
pub(crate) fn persona_backend_route(
    profile: Option<&newt_core::RoleProfile>,
    configured: &[&str],
) -> Result<Option<(String, Option<String>)>, String> {
    match persona_provider_env(profile) {
        Some((backend, model)) if configured.contains(&backend.as_str()) => {
            Ok(Some((backend, model)))
        }
        Some((backend, _)) => Err(backend),
        None => Ok(None),
    }
}

/// Persona backend auto-route: repoint the session's wire target to the active
/// persona's `backend:` — validated against `cfg.backends`, exactly as
/// `/backends <name>` would (an unknown name is refused, not silently rerouted).
/// A persona that declares NO backend (or a cleared persona → `None`) REVERTS to
/// the pre-persona `baseline` (`base_provider`, `base_model`), so routing is
/// symmetric: loading a persona repoints, clearing it repoints back. Sets
/// `NEWT_PROVIDER`/`NEWT_DGX_MODEL`, re-resolves via [`refresh_backend`], and
/// prints a line. Returns whether the URL changed (caller re-probes DGX).
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_persona_backend(
    persona: Option<&Persona>,
    base_provider: &Option<String>,
    base_model: &Option<String>,
    cfg: &newt_core::ResolvedConfig,
    choice: &mut BackendChoice,
    inf_url: &mut String,
    inf_model: &mut String,
    inf_kind: &mut newt_core::BackendKind,
    inf_key: &mut Option<String>,
    inf_context_window: &mut Option<u32>,
    color: bool,
    verbose: bool,
) -> bool {
    let configured: Vec<&str> = cfg.backends.iter().map(|b| b.name.as_str()).collect();
    let has_backend = persona.and_then(|p| p.profile.backend.as_deref()).is_some();
    let (provider, model) = match persona_backend_route(persona.map(|p| &p.profile), &configured) {
        Ok(Some((backend, model))) => (Some(backend), model),
        // Revert to the pre-persona baseline (persona declares no backend / cleared).
        Ok(None) => (base_provider.clone(), base_model.clone()),
        Err(unknown) => {
            print_newt(
                &format!(
                    "persona names unknown backend '{unknown}' — leaving backend unchanged. configured: {}",
                    if configured.is_empty() { "(none)".to_string() } else { configured.join(", ") }
                ),
                color,
                verbose,
            );
            return false;
        }
    };
    // Publish the pair under ONE hold of the process-env lock (#1850), so no
    // guarded reader can observe a half-applied route. The old justification
    // here — "single-threaded REPL" — is true of the REPL and false under
    // `cargo test`, which runs tests as threads of one process.
    {
        let _env = newt_core::process_env::lock();
        newt_core::process_env::set_or_remove("NEWT_PROVIDER", provider.as_deref());
        newt_core::process_env::set_or_remove("NEWT_DGX_MODEL", model.as_deref());
    }
    // Track the backend by NAME across the re-resolve: two backends can share an
    // endpoint (e.g. `sol` and `openai` both on api.openai.com), so the URL alone
    // can't tell a route/revert happened — the name can.
    let prev_name = choice.name.clone();
    let url_changed = refresh_backend(
        cfg,
        choice,
        inf_url,
        inf_model,
        inf_kind,
        inf_key,
        inf_context_window,
        color,
        verbose,
    );
    if has_backend {
        print_newt(
            &format!(
                "persona backend → {} (model {})",
                choice.name,
                inf_model.as_str()
            ),
            color,
            verbose,
        );
    } else if choice.name != prev_name {
        // A cleared persona reverted the session to its pre-persona backend.
        print_newt(
            &format!("backend reverted to {} (persona cleared)", choice.name),
            color,
            verbose,
        );
    }
    url_changed
}

/// #1668: the posture this INVOCATION started with — the session's own
/// backend/model baseline plus the dial overrides its CLI flags installed,
/// snapshotted ONCE in `run_chat` after the flags land and before any
/// conversation pin is applied.
///
/// Every conversation switch resets to this before layering the incoming
/// conversation's own pin, which is what keeps posture *per conversation*: the
/// 2026-08-13 review (finding 2) showed that without a reset, applying one
/// conversation's pin left its backend and dials installed in the session
/// globals, and every conversation the session visited afterwards silently ran
/// — and, under the old ambient capture, was durably pinned — to it.
///
/// An axis a pin does not mention resolves to this baseline, so "unpinned"
/// means "whatever this invocation was launched with", never "whatever the
/// previous conversation left behind".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreferenceBaseline {
    /// `NEWT_PROVIDER` as the invocation resolved it (flag / loadout / sticky).
    pub provider: Option<String>,
    /// `NEWT_DGX_MODEL` as the invocation resolved it.
    pub model: Option<String>,
    /// The `/cognition` override the invocation started with.
    pub cognition: newt_core::cognition::CognitionOverride,
    /// The `/tenacity` override the invocation started with.
    pub tenacity: Option<newt_core::Tenacity>,
}

impl PreferenceBaseline {
    /// Snapshot the live dials beside the operator backend baseline the caller
    /// already resolved (`base_provider` / `base_model`).
    pub(crate) fn snapshot(provider: Option<String>, model: Option<String>) -> Self {
        Self {
            provider,
            model,
            cognition: newt_core::cognition::cli_cognition(),
            tenacity: newt_core::tenacity::cli_tenacity(),
        }
    }
}

/// Everything a conversation switch needs to re-seat the session posture
/// (#1668) — one record instead of a positional list, because these travel
/// together to all five switch sites (session-start resume, `/resume`,
/// `/conversation restore`/`new`, `/roadmap next`, `/new`).
pub(crate) struct ConversationPreferenceSwitch<'a> {
    /// The conversation store, or `None` in an ephemeral session (which has no
    /// pins at all — the switch then only resets to the baseline).
    pub store: Option<&'a newt_core::ConversationStore>,
    /// The conversation the session is switching TO.
    pub conversation_id: &'a str,
    /// The invocation baseline every unpinned axis resolves to.
    pub baseline: &'a PreferenceBaseline,
    /// The active persona, whose declared `backend:` outranks the baseline
    /// (but not the pin) for the backend axis.
    pub persona: Option<&'a Persona>,
    /// Posture actions still awaiting a durable row — dropped here, because
    /// they belonged to the conversation being left.
    pub pending: &'a mut newt_core::PreferenceActions,
    /// The operator backend baseline locals, reset to `baseline`. NEVER the
    /// pin: adopting a pin here is what let it propagate (review finding 2).
    pub base_provider: &'a mut Option<String>,
    pub base_model: &'a mut Option<String>,
    pub cfg: &'a newt_core::ResolvedConfig,
    pub choice: &'a mut BackendChoice,
    pub inf_url: &'a mut String,
    pub inf_model: &'a mut String,
    pub inf_kind: &'a mut newt_core::BackendKind,
    pub inf_key: &'a mut Option<String>,
    pub inf_context_window: &'a mut Option<u32>,
    pub color: bool,
    pub verbose: bool,
}

/// The result of applying a conversation's preference pin.
///
/// #1669 PR-A / ADR blocker 4: this used to be a bare `bool` (did the endpoint
/// move?), so a pin that could NOT be applied — a backend the config no longer
/// defines, a pin row that would not read — printed a notice and the session
/// carried on at baseline. That is exactly the silent-wrong-posture case the
/// ADR forbids: the tab says it is pinned to one backend and the next turn runs
/// somewhere else.
pub(crate) struct PinRestore {
    /// Whether the backend endpoint moved, so the caller re-probes telemetry.
    pub url_changed: bool,
    /// `Some` when the pin could not be fully established. The session is at a
    /// known baseline; the caller marks the tab degraded and refuses turns.
    pub degraded: Option<PinDegraded>,
}

/// A pin that could not be established, and enough to retry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PinDegraded {
    /// Operator-facing reasons, verbatim from the apply plan.
    pub reasons: Vec<String>,
    /// The pin as stored — retained so a retry has something to retry once the
    /// operator fixes what was missing (usually a `[[backends]]` entry).
    pub pin: newt_core::OperatorPreferencePin,
}

impl PinDegraded {
    /// One line for the footer/list and for the turn refusal.
    pub fn summary(&self) -> String {
        format!("!pin — {}", self.reasons.join("; "))
    }
}

/// How this launch's conversation actually resolved — the input to "may the
/// startup preference pin apply?".
///
/// #1668 review-2 finding 6: this used to be a bare `resumed_at_start &&
/// !claim_refused` expression inline in `run_chat`, which no test could reach.
/// The test for the rule therefore hand-modelled the ordering by passing the
/// replacement id it had computed itself, so re-introducing the bug — applying
/// the HELD conversation's pin after a refused claim — would not have failed
/// it. Making the outcome a value lets `run_chat` and the test drive the same
/// gate, in [`apply_startup_preference_pin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupConversation {
    /// Minted fresh this launch — there is no stored pin to restore.
    Fresh,
    /// Resumed AND the claim was granted: this session holds the conversation.
    ResumedHeld,
    /// Resume refused (another live newt holds it); the session dropped onto a
    /// fresh replacement conversation instead.
    ResumedRefused,
}

impl StartupConversation {
    /// A pin applies only for a conversation this session actually HOLDS.
    ///
    /// A refused resume is the load-bearing case: the operator asked for a
    /// conversation someone else has open, so the session is now on a fresh
    /// replacement they never pinned. Applying the held one's posture there
    /// would push one operator's backend, model, and dials into another
    /// operator's session.
    pub(crate) fn applies_pin(self) -> bool {
        matches!(self, Self::ResumedHeld)
    }
}

/// The startup half of the pin restore: apply `sw` only when this session holds
/// the conversation the pin belongs to.
///
/// Exists so the gate is a seam rather than an inline conjunction — see
/// [`StartupConversation`]. It refuses on the outcome alone, so it is safe even
/// against a caller that passes the held conversation's id after a refusal.
pub(crate) fn apply_startup_preference_pin(
    outcome: StartupConversation,
    sw: ConversationPreferenceSwitch<'_>,
) -> bool {
    if !outcome.applies_pin() {
        return false;
    }
    restore_preference_pin(sw).url_changed
}

/// #1668: re-seat the session posture for the conversation being switched to —
/// the one step every conversation switch runs (session-start resume,
/// `/resume`, `/conversation restore`, `/roadmap next`, and `/new`).
///
/// Two halves, in order:
///
/// 1. **Reset to the invocation baseline.** The dials, the operator backend
///    baseline, and the routing env go back to what this process launched
///    with, so nothing the *previous* conversation's pin installed survives the
///    switch (review finding 2). A persona's declared `backend:` still routes:
///    it is session state, not the outgoing conversation's.
/// 2. **Apply the incoming conversation's own pin** through the SAME setters
///    the live commands use — `set_cli_cognition` / `set_cli_tenacity`, and the
///    `NEWT_PROVIDER` / `NEWT_DGX_MODEL` env pair + [`refresh_backend`] with
///    `/backends`' clear-the-model-override semantics — validated against
///    `cfg.backends` first, and skipping any axis this invocation's explicit
///    flags own ([`newt_core::runtime::cli_preference_axes`], review findings 4
///    and 9). An unknown pinned backend or an unparseable dial prints one line
///    and applies nothing for that axis; because capture is action-only, the
///    stored row survives that fail-open verbatim (review finding 3).
///
/// The pin is deliberately NOT adopted into `base_provider`/`base_model`: the
/// baseline answers "what does `/persona clear` revert to", which is a property
/// of the invocation and the operator's live choices, not of whichever
/// conversation is open.
///
/// Returns whether the endpoint URL changed, so the caller re-probes DGX
/// telemetry only when it matters (same contract as [`apply_persona_backend`]).
pub(crate) fn restore_preference_pin(sw: ConversationPreferenceSwitch<'_>) -> PinRestore {
    let ConversationPreferenceSwitch {
        store,
        conversation_id,
        baseline,
        persona,
        pending,
        base_provider,
        base_model,
        cfg,
        choice,
        inf_url,
        inf_model,
        inf_kind,
        inf_key,
        inf_context_window,
        color,
        verbose,
    } = sw;
    // Actions marked but not yet written belonged to the OUTGOING conversation
    // (it had no durable row); they must never land on the incoming one.
    *pending = newt_core::PreferenceActions::default();
    // Fail-open on a bad read: a corrupt pin must never block a resume, and
    // must never leave the session on the previous conversation's posture —
    // the baseline reset below still runs.
    let mut pin_read_failed = false;
    let pin = match store.map(|s| s.preference_pin(conversation_id)) {
        Some(Ok(Some(pin))) => pin,
        None | Some(Ok(None)) => newt_core::OperatorPreferencePin::default(),
        Some(Err(e)) => {
            print_newt(
                &format!(
                    "warning: could not read the conversation preference pin ({e}) — \
                     using this run's baseline preferences"
                ),
                color,
                verbose,
            );
            pin_read_failed = true;
            newt_core::OperatorPreferencePin::default()
        }
    };
    let owned = newt_core::runtime::cli_preference_axes();
    let configured: Vec<&str> = cfg.backends.iter().map(|b| b.name.as_str()).collect();
    let plan = pin.apply_plan(&configured, owned);
    for notice in &plan.notices {
        print_newt(notice, color, verbose);
    }
    // ADR blocker 4: a notice means the pin asked for something this process
    // could not establish. The baseline reset below still runs — so the session
    // lands somewhere KNOWN — but the caller must be told, because "at baseline
    // while claiming to be pinned" is a posture the operator did not choose.
    let mut degraded = (!plan.notices.is_empty() || pin_read_failed).then(|| PinDegraded {
        reasons: plan.notices.clone(),
        pin: pin.clone(),
    });
    if let Some(d) = degraded.as_mut() {
        if d.reasons.is_empty() {
            d.reasons
                .push("the stored preference pin could not be read".to_string());
        }
    }

    // ---- 1. reset the dials to the invocation baseline ----------------
    newt_core::cognition::set_cli_cognition(baseline.cognition);
    match baseline.tenacity {
        Some(t) => newt_core::tenacity::set_cli_tenacity(t),
        None => newt_core::tenacity::clear_cli_tenacity(),
    }
    *base_provider = baseline.provider.clone();
    *base_model = baseline.model.clone();

    // ---- 2. layer the pin's own axes over it --------------------------
    let mut applied: Vec<String> = Vec::new();
    if let Some(o) = plan.cognition {
        newt_core::cognition::set_cli_cognition(o);
        applied.push(format!(
            "cognition {}",
            pin.cognition.as_deref().unwrap_or("?")
        ));
    }
    if let Some(t) = plan.tenacity {
        newt_core::tenacity::set_cli_tenacity(t);
        applied.push(format!("tenacity {}", t.label()));
    }
    // The backend axis the session should route on once the switch settles:
    // the pin if it names one, else the active persona's declared route, else
    // the invocation baseline. Computed as a target and compared against the
    // live env, so an unchanged target costs no re-resolve (and no probe).
    let (mut provider, mut model) =
        match persona_backend_route(persona.map(|p| &p.profile), &configured) {
            // An unknown persona backend was already reported when the persona
            // activated; fall back to the baseline rather than repeat it here.
            Ok(Some((backend, model))) => (Some(backend), model),
            Ok(None) | Err(_) => (baseline.provider.clone(), baseline.model.clone()),
        };
    match plan.backend_axis {
        newt_core::BackendAxisAction::Leave => {}
        newt_core::BackendAxisAction::Route {
            provider: pinned,
            model: pinned_model,
        } => {
            applied.push(match &pinned_model {
                newt_core::RouteModel::Set(m) => format!("backend {pinned} (model {m})"),
                newt_core::RouteModel::Clear => format!("backend {pinned}"),
                // Say so out loud: the operator's own model survived a pinned
                // backend, which is the precedence rule doing its job.
                newt_core::RouteModel::Keep => {
                    format!("backend {pinned} (this run's model kept)")
                }
            });
            provider = Some(pinned);
            match pinned_model {
                // A backend pin that names a model installs it.
                newt_core::RouteModel::Set(m) => model = Some(m),
                // A backend pin with no model clears the override so the
                // backend's own default applies — the `/backends <name>` rule.
                newt_core::RouteModel::Clear => model = None,
                // This invocation owns the model axis: leave what the operator
                // supplied exactly as it is (review-2 finding 2).
                newt_core::RouteModel::Keep => {}
            }
        }
        newt_core::BackendAxisAction::ModelOnly(pinned_model) => {
            applied.push(format!("model {pinned_model}"));
            model = Some(pinned_model);
        }
    }
    let mut url_changed = false;
    let live = (
        std::env::var("NEWT_PROVIDER").ok(),
        std::env::var("NEWT_DGX_MODEL").ok(),
    );
    if live != (provider.clone(), model.clone()) {
        // One hold of the process-env lock for the pair — same discipline as
        // apply_persona_backend (#1850).
        {
            let _env = newt_core::process_env::lock();
            newt_core::process_env::set_or_remove("NEWT_PROVIDER", provider.as_deref());
            newt_core::process_env::set_or_remove("NEWT_DGX_MODEL", model.as_deref());
        }
        url_changed = refresh_backend(
            cfg,
            choice,
            inf_url,
            inf_model,
            inf_kind,
            inf_key,
            inf_context_window,
            color,
            verbose,
        );
    }
    // One line of operator visibility: silent dial changes would be invisible
    // until the next /psyche. Nothing prints when the pin applied nothing.
    if !applied.is_empty() {
        print_newt(
            &format!("session preferences restored: {}", applied.join(" · ")),
            color,
            verbose,
        );
    }
    // And one line when a pin was deliberately NOT applied because this run's
    // flags own the axis — otherwise the operator has no way to tell the pin
    // from the flag (review finding 9: the precedence must be visible).
    let suppressed = newt_core::PreferenceAxes {
        backend: owned.backend && pin.backend.is_some(),
        model: owned.model && pin.model.is_some(),
        cognition: owned.cognition && pin.cognition.is_some(),
        tenacity: owned.tenacity && pin.tenacity.is_some(),
    };
    if !suppressed.is_empty() {
        print_newt(
            &format!(
                "session preferences: this run's explicit {} beats the pin (kept for the next run)",
                suppressed.labels().join(" · ")
            ),
            color,
            verbose,
        );
    }
    PinRestore {
        url_changed,
        degraded,
    }
}

/// #1668: fold the operator posture ACTIONS marked since the last drain into
/// the session's operator baseline and the active conversation's stored pin.
/// The chat loop calls this ONCE per iteration — the single persistence site
/// for the pin, and the single owner of `base_provider` / `base_model` updates.
///
/// Actions accumulate in a process global because the commands that produce
/// them (`commands::model`, `commands::settings`, the psyche panel) cannot
/// reach session state; marking happens where SUCCESS is known, so a listing,
/// a refused pick, or a persona route contributes nothing.
///
/// A conversation whose durable row does not exist yet (the normal state of a
/// fresh session until its first saved turn) keeps its actions PENDING rather
/// than losing them — the write lands on the iteration after the row appears.
/// An ephemeral session has no store, so nothing is ever written: it leaves no
/// trace, exactly as `/backends`' #545 persistence rule already promises.
///
/// `Err` carries a one-line warning for the caller to print; the session is
/// never blocked by a posture write.
/// A tab's display label: the conversation's title, else `#<short-id>`.
///
/// #1669 PR-A, factored from the #1671 footer rule (`chat.rs:1997-2007`) so the
/// bar, the `/tab` list, and the footer cannot disagree. **Labels are never
/// stored** — computed fresh at every read — so `/rename` honesty is free and a
/// title can never go stale in a list.
pub(crate) fn tab_label(store: &newt_core::ConversationStore, conversation_id: &str) -> String {
    store
        .title(conversation_id)
        .ok()
        .flatten()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| format!("#{}", short_conversation_id(conversation_id)))
}

/// Retitle `conversation_id`, creating its row if it has none yet.
///
/// #1669 PR-A, extracted from the `/rename` arm so it can target a tab that is
/// NOT the active conversation — the `/tab rename [n] <title>` case.
///
/// **The #1030 guard is preserved verbatim and is the reason this matches on
/// `exists()` rather than using `unwrap_or(false)`:** a transient store error
/// (SQLITE_BUSY, or NFS IO under concurrent-newt contention) must never read as
/// "absent" and route into `create_with_id`, whose `INSERT OR REPLACE` would
/// destroy the live conversation and CASCADE-drop its turns. An error stays an
/// error.
pub(crate) fn rename_conversation(
    store: &newt_core::ConversationStore,
    conversation_id: &str,
    title: &str,
    persona: Option<&str>,
) -> anyhow::Result<()> {
    let title = title.trim();
    if title.is_empty() {
        anyhow::bail!("a title cannot be empty");
    }
    match store.exists(conversation_id)? {
        true => store.rename(conversation_id, title),
        false => store.create_with_id(conversation_id, title, persona),
    }
}

pub(crate) fn persist_preference_actions(
    store: Option<&newt_core::ConversationStore>,
    conversation_id: &str,
    pending: &mut newt_core::PreferenceActions,
    base_provider: &mut Option<String>,
    base_model: &mut Option<String>,
) -> Result<(), String> {
    let fresh = newt_core::runtime::drain_preference_actions();
    // The operator baseline follows the operator's own acted axes — never the
    // ambient env, which a persona route or an applied pin may have rewritten
    // (review findings 1 and 7).
    if let Some(provider) = &fresh.backend {
        base_provider.clone_from(provider);
    }
    if let Some(model) = &fresh.model {
        base_model.clone_from(model);
    }
    pending.merge(fresh);
    if pending.is_empty() {
        return Ok(());
    }
    let Some(store) = store else {
        // Ephemeral: no store, no pin, nothing held.
        *pending = newt_core::PreferenceActions::default();
        return Ok(());
    };
    let stored = match store.preference_pin(conversation_id) {
        // No durable row yet — hold the actions until there is one.
        Ok(None) => return Ok(()),
        Ok(Some(pin)) => pin,
        Err(e) => {
            *pending = newt_core::PreferenceActions::default();
            return Err(format!(
                "could not read the conversation preference pin ({e}) — \
                 this change applies to the session but is not pinned"
            ));
        }
    };
    let merged = stored.merged(pending);
    *pending = newt_core::PreferenceActions::default();
    if merged == stored {
        return Ok(());
    }
    store
        .update_preference_pin(conversation_id, &merged)
        .map_err(|e| format!("could not persist the conversation preference pin: {e}"))
}

#[cfg(test)]
#[path = "lib_tests/resumed_preference_tests.rs"]
mod resumed_preference_tests;

#[cfg(test)]
#[path = "lib_tests/persona_backend_tests.rs"]
mod persona_backend_tests;

/// Build a system prompt with workspace context so the model knows the project.
// build_system_prompt_with_soul is used directly now; this wrapper kept for tests.
#[allow(dead_code)]
fn build_system_prompt(workspace: &str, plan_path: &str) -> String {
    build_system_prompt_with_soul(workspace, None, plan_path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Persona {
    name: String,
    prompt: String,
    path: std::path::PathBuf,
    /// The parsed role profile. For a plain prompt-only persona (no `+++`
    /// front-matter) every field except `prompt` is `None`, so behavior is
    /// identical to before role profiles existed. When the file carries
    /// front-matter, this surfaces the role's tool allow-list, caveat profile,
    /// and model/tier router policy.
    profile: newt_core::RoleProfile,
}

impl Persona {
    fn description(&self) -> String {
        self.prompt
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .trim_start_matches('#')
            .trim()
            .chars()
            .take(96)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersonaSummary {
    name: String,
    description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersonaStore {
    dir: std::path::PathBuf,
}

/// Why a [`PersonaStore::save`] did not write — mapped by the config-panel caller
/// to `config_panel::SaveResult` for a visible status line. Rich-tui only (the
/// panel is its only caller).
#[cfg(feature = "rich-tui")]
#[derive(Debug)]
enum PersonaSaveError {
    /// A persona with this name already exists and overwrite was not requested.
    Exists,
    /// The name is not a valid persona file stem.
    InvalidName(String),
    /// The filesystem write failed.
    Io(String),
}

impl PersonaStore {
    const DEFAULT_NAME: &'static str = "coder";

    fn default_dir() -> std::path::PathBuf {
        newt_core::Config::user_config_path()
            .map(|p| p.with_file_name("personas"))
            .unwrap_or_else(|| std::path::PathBuf::from("personas"))
    }

    fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn default() -> Self {
        Self::new(Self::default_dir())
    }

    /// Atomically write persona `<name>.md`. With `overwrite == false`, refuses an
    /// existing persona ([`PersonaSaveError::Exists`]); with `true`, replaces it
    /// atomically — writes to a temp file in the destination directory then renames
    /// it over the target, so a failed or partial write never truncates or corrupts
    /// the existing persona (review-3 §1). Used only by the config panel's save
    /// action, so it is rich-tui-gated alongside the panel.
    #[cfg(feature = "rich-tui")]
    fn save(
        &self,
        name: &str,
        content: &str,
        overwrite: bool,
    ) -> Result<std::path::PathBuf, PersonaSaveError> {
        self.save_with(name, content, overwrite, |p, c| std::fs::write(p, c))
    }

    /// [`Self::save`] with an injectable byte-writer, so the atomicity guarantee
    /// (a failed write preserves the original) is unit-testable without needing to
    /// provoke a real I/O error.
    #[cfg(feature = "rich-tui")]
    fn save_with<W>(
        &self,
        name: &str,
        content: &str,
        overwrite: bool,
        write_bytes: W,
    ) -> Result<std::path::PathBuf, PersonaSaveError>
    where
        W: Fn(&std::path::Path, &str) -> std::io::Result<()>,
    {
        let name = normalize_persona_name(name)
            .map_err(|e| PersonaSaveError::InvalidName(e.to_string()))?;
        let path = self.dir.join(format!("{name}.md"));
        if !overwrite && path.exists() {
            return Err(PersonaSaveError::Exists);
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| PersonaSaveError::Io(e.to_string()))?;
        // Atomic replace: write a temp file in the SAME dir, then rename over the
        // target. If either step fails we remove the temp and leave the original
        // untouched — never a truncating in-place write.
        let tmp = self
            .dir
            .join(format!(".{name}.md.tmp.{}", std::process::id()));
        if let Err(e) = write_bytes(&tmp, content) {
            let _ = std::fs::remove_file(&tmp);
            return Err(PersonaSaveError::Io(e.to_string()));
        }
        match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(path),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(PersonaSaveError::Io(e.to_string()))
            }
        }
    }

    fn load(&self, name: &str) -> anyhow::Result<Persona> {
        self.ensure_defaults()?;
        let name = normalize_persona_name(name)?;
        let path = self.dir.join(format!("{name}.md"));
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => anyhow::bail!("unknown persona `{name}`\n{}", self.list_message()?),
        };
        // Parse optional `+++` front-matter into a role profile. A plain `.md`
        // with no front-matter yields a prompt-only profile (backward
        // compatible). The injected `prompt` is the markdown BODY, so
        // front-matter never leaks into the system prompt.
        let profile = newt_core::RoleProfile::parse(&raw)
            .map_err(|e| anyhow::anyhow!("persona `{name}`: {e}"))?;
        if profile.prompt.is_empty() {
            anyhow::bail!("persona `{name}` is empty: {}", path.display());
        }
        Ok(Persona {
            name,
            prompt: profile.prompt.clone(),
            path,
            profile,
        })
    }

    fn list(&self) -> anyhow::Result<Vec<PersonaSummary>> {
        self.ensure_defaults()?;
        let mut personas = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            if raw.trim().is_empty() {
                continue;
            }
            // Skip files whose front-matter is malformed in the listing rather
            // than failing the whole list; `load` surfaces the error on use.
            let Ok(profile) = newt_core::RoleProfile::parse(&raw) else {
                continue;
            };
            let persona = Persona {
                name: name.to_string(),
                prompt: profile.prompt.clone(),
                path: path.clone(),
                profile,
            };
            let description = persona.description();
            personas.push(PersonaSummary {
                name: persona.name,
                description,
            });
        }
        personas.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(personas)
    }

    fn list_message(&self) -> anyhow::Result<String> {
        let personas = self.list()?;
        let mut out = format!("Available personas in {}:", self.dir.display());
        if personas.is_empty() {
            out.push_str("\n  (none)");
        } else {
            for p in personas {
                out.push_str(&format!("\n  {} - {}", p.name, p.description));
            }
        }
        Ok(out)
    }

    /// Shipped default personas, seeded per-file-idempotently (FR-16, #1000):
    /// each MISSING one is written on any store access, so an upgrade that
    /// predates a new default still receives it (the old empty-dir gate would
    /// strand it forever). `coder` is the doer identity (DEFAULT_SOUL); `coach`
    /// is the read-only advise-first persona — its `[caveats]` are enforced by
    /// FR-1 and its `altitude = "coach"` swaps in COACH_SOUL via FR-5.
    /// `personal-assistant` (#1021, FR-PA-3) is `coach`'s domain-specific
    /// specialization for enterprise routine automation — its `skills:`
    /// binding (FR-4, #1041) preloads `gila-personal-assistant`, and its
    /// `tools:` allow-list (already enforced by FR-1) restricts it to that
    /// skill's `modulex__*` MCP tools plus infra tools; FR-PA-4 needed no new
    /// code since `filter_advertised_tools` already does this. A user who
    /// deletes a default gets it back next launch; empty the file to suppress
    /// it.
    const DEFAULT_PERSONAS: &'static [(&'static str, &'static str)] = &[
        (Self::DEFAULT_NAME, newt_core::DEFAULT_SOUL),
        ("coach", COACH_PERSONA),
        ("personal-assistant", PERSONAL_ASSISTANT_PERSONA),
    ];

    fn ensure_defaults(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        for &(name, body) in Self::DEFAULT_PERSONAS {
            let path = self.dir.join(format!("{name}.md"));
            if !path.exists() {
                std::fs::write(&path, body)?;
            }
        }
        Ok(())
    }
}

/// The read-only coach persona shipped as a repo template (FR-16, #1000), so
/// `shipped_role_templates_parse` type-checks its front-matter beside the others.
const COACH_PERSONA: &str = include_str!("../../personas/coach.md");

/// The Personal Assistant persona shipped as a repo template (#1021, FR-PA-3),
/// so `shipped_role_templates_parse` type-checks its front-matter beside the
/// others.
const PERSONAL_ASSISTANT_PERSONA: &str = include_str!("../../personas/personal-assistant.md");

/// FR-4 (#1041): names in `skills` that do NOT resolve to a `SKILL.md` under
/// `search_dirs` — a persona's declared-but-missing skill bindings. Empty when
/// every declared skill resolves (or none are declared). Checked eagerly at
/// activation (not just on the model's first `use_skill` call) so a
/// misconfigured persona surfaces the problem immediately.
fn missing_bound_skills(skills: &[String], search_dirs: &[std::path::PathBuf]) -> Vec<String> {
    if skills.is_empty() {
        return Vec::new();
    }
    let available: std::collections::HashSet<String> = newt_skills::discover_paths(search_dirs)
        .into_iter()
        .map(|s| s.name)
        .collect();
    skills
        .iter()
        .filter(|name| !available.contains(*name))
        .cloned()
        .collect()
}

/// FR-4 (#1041): warn (non-fatal — matches `PersonaStore::list`'s soft-fail
/// style for bad data) when an active persona's `skills:` front-matter names a
/// skill that doesn't resolve under `search_dirs`.
fn warn_on_missing_bound_skills(
    persona: Option<&Persona>,
    search_dirs: &[std::path::PathBuf],
    color: bool,
    verbose: bool,
) {
    let Some(persona) = persona else { return };
    let Some(skills) = &persona.profile.skills else {
        return;
    };
    let missing = missing_bound_skills(skills, search_dirs);
    if !missing.is_empty() {
        print_newt(
            &format!(
                "warning: persona `{}` declares skill(s) not found in any skill search dir: {}",
                persona.name,
                missing.join(", ")
            ),
            color,
            verbose,
        );
    }
}

/// The shipped `gila-personal-assistant` skill (#1021) — coaches on a
/// `modulex` MCP routine report. Seeded into the default skill dir
/// per-file-idempotently, the same shipped-template pattern
/// `PersonaStore::DEFAULT_PERSONAS` uses (FR-16, #1000), so a
/// `personal-assistant` persona's declared `skills:` binding resolves out of
/// the box with no manual `[skills] bundled_dir` opt-in required.
///
/// Sourced from `newt-tui/assets/`, NOT the repo-local `.newt/` config dir:
/// compiled-in assets must never live under a `.newt/` directory, because
/// operators legitimately move `.newt` dirs aside (e.g. to simulate a fresh
/// unboxing) and that must never break `cargo build`.
const GILA_SKILL: &str = include_str!("../assets/bundled-skills/gila-personal-assistant/SKILL.md");

/// Seed [`GILA_SKILL`] into the default skill directory
/// (`newt_skills::default_skills_dir()`, i.e. `~/.newt/skills`) if missing. A
/// `None` default dir (unresolvable `$HOME`) is a silent no-op, matching how
/// `current_host_boot`-style host lookups degrade elsewhere in this codebase.
fn ensure_default_skills() -> anyhow::Result<()> {
    match newt_skills::default_skills_dir() {
        Some(dir) => seed_gila_skill(&dir),
        None => Ok(()),
    }
}

/// Write [`GILA_SKILL`] to `<skills_root>/gila-personal-assistant/SKILL.md`
/// if missing. Per-file-idempotent like `PersonaStore::ensure_defaults`: a
/// user who deletes it gets it back next launch; an empty file suppresses it.
/// Split out from [`ensure_default_skills`] so the write itself is testable
/// against an explicit temp dir rather than the real `$HOME`.
fn seed_gila_skill(skills_root: &std::path::Path) -> anyhow::Result<()> {
    let skill_dir = skills_root.join("gila-personal-assistant");
    let path = skill_dir.join("SKILL.md");
    if !path.exists() {
        std::fs::create_dir_all(&skill_dir)?;
        std::fs::write(&path, GILA_SKILL)?;
    }
    Ok(())
}

fn normalize_persona_name(name: &str) -> anyhow::Result<String> {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("persona names may only contain letters, numbers, '-' and '_'");
    }
    Ok(name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PersonaCommand {
    List,
    Show,
    Clear,
    /// Swap to persona `name`. `keep_context` (the `--keep-context` flag)
    /// swaps WITHOUT resetting the conversation, per the persistent-actor
    /// principle; the default (`false`) preserves today's reset-on-swap
    /// behavior.
    Set {
        name: String,
        keep_context: bool,
    },
}

#[cfg(test)]
impl PersonaCommand {
    /// Test-only constructor for the common `Set { keep_context: false }` case.
    fn set(name: impl Into<String>) -> Self {
        Self::Set {
            name: name.into(),
            keep_context: false,
        }
    }
}

fn parse_persona_command(input: &str) -> anyhow::Result<PersonaCommand> {
    let body = input.trim().trim_start_matches('/').trim();
    let mut parts = body.split_whitespace();
    match parts.next() {
        Some("persona") => {}
        _ => anyhow::bail!("not a persona command"),
    }

    // Pull `--keep-context` from anywhere in the remaining tokens; collect the
    // rest as positional args.
    let mut keep_context = false;
    let positional: Vec<&str> = parts
        .filter(|tok| {
            if *tok == "--keep-context" {
                keep_context = true;
                false
            } else {
                true
            }
        })
        .collect();
    let mut positional = positional.into_iter();

    match positional.next() {
        None | Some("show") => Ok(PersonaCommand::Show),
        Some("list") => Ok(PersonaCommand::List),
        Some("clear" | "off") => Ok(PersonaCommand::Clear),
        Some("default") => Ok(PersonaCommand::Set {
            name: PersonaStore::DEFAULT_NAME.into(),
            keep_context,
        }),
        // FR-PA-1 (#1021): `switch` is a discoverable alias for `set` — same
        // handling, same usage shape. `/persona <name>` (the bare fallthrough
        // below) already does this too; `switch` just names the verb
        // explicitly for an operator reaching for it by that word.
        Some(verb @ ("set" | "switch")) => match positional.next() {
            Some(name) => Ok(PersonaCommand::Set {
                name: name.to_string(),
                keep_context,
            }),
            None => anyhow::bail!("usage: /persona {verb} <name> [--keep-context]"),
        },
        Some(name) => Ok(PersonaCommand::Set {
            name: name.to_string(),
            keep_context,
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConversationCommand {
    List,
    Show(String),
    Restore(String),
    Rename { id: String, title: String },
    Delete(String),
}

/// The conversation a `/conversation …` command would SELECT, if any.
///
/// #1669 PR-A blocker 2: `/conversation restore` is a conversation-selection
/// path, so it must consult the tab-aware adoption seam before it runs —
/// otherwise it can point a second tab at a conversation another tab holds.
/// Parsing is pure and cheap, so the caller asks first and dispatches after.
pub(crate) fn conversation_command_target(input: &str) -> Option<String> {
    match parse_conversation_command(input).ok()? {
        ConversationCommand::Restore(id) => Some(id),
        _ => None,
    }
}

/// Which verb an operator typed to reach the conversation ops.
///
/// #2009 PR6b folded them into `/resume`. The ops are identical either way —
/// the same parser, the same handler — but the VERB decides what a retired
/// mutator is allowed to do: a retired READ still reads, and a retired MUTATOR
/// redirects without mutating (the rule PR3 wrote down, applied per
/// subcommand because `/conversation` is both).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConversationDoor {
    /// `/resume <sub>` — the replacement, which performs.
    Resume,
    /// `/conversation <sub>` — retired; reads still read.
    Retired,
}

impl ConversationCommand {
    /// Whether this op CHANGES something. A retired verb may still serve the
    /// reads and must not serve these.
    pub(crate) fn mutates(&self) -> bool {
        !matches!(self, Self::List | Self::Show(_))
    }
}

/// Parse a conversation op typed at EITHER door, reporting which one.
fn parse_conversation_command_at(
    input: &str,
) -> anyhow::Result<(ConversationDoor, ConversationCommand)> {
    let body = input.trim().trim_start_matches('/').trim();
    let mut parts = body.split_whitespace();
    let door = match parts.next() {
        Some("conversation") => ConversationDoor::Retired,
        Some("resume") => ConversationDoor::Resume,
        _ => anyhow::bail!("not a conversation command"),
    };
    parse_conversation_rest(door, parts)
}

fn parse_conversation_command(input: &str) -> anyhow::Result<ConversationCommand> {
    Ok(parse_conversation_command_at(input)?.1)
}

/// The named conversation subcommands, reached through `/resume`.
///
/// Bare `/resume` and `/resume <query>` are BROWSE and SEARCH and belong to
/// the resume arm; only these five words route to the conversation ops. The
/// list is data so the predicate, the parser and the help cannot disagree
/// about what `/resume` accepts.
const CONVERSATION_SUBCOMMANDS: &[&str] = &["list", "show", "restore", "rename", "delete", "rm"];

/// Whether `body` is `/resume <one of the conversation subcommands>`.
pub(crate) fn resume_conversation_subcommand(body: &str) -> bool {
    let Some(rest) = body.strip_prefix("resume") else {
        return false;
    };
    // `/resumex list` is not `/resume list`; bare `/resume` is browse. Same
    // whole-word shape as every other parser in this fold, deliberately.
    let rest = match rest.chars().next() {
        None => return false,
        Some(c) if c.is_whitespace() => rest.trim_start(),
        Some(_) => return false,
    };
    // `/resume restored` is a SEARCH, not `restore` with a suffix — the word
    // has to match whole, which `split_whitespace` gives for free.
    rest.split_whitespace()
        .next()
        .is_some_and(|word| CONVERSATION_SUBCOMMANDS.contains(&word))
}

/// What the session loop should DO with a conversation op.
///
/// Split out of the arm so the retirement rule is one decision in one place
/// with its own tests, rather than a shape of nested `if`s inside a 200-line
/// match in `run_chat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConversationOpPlan {
    /// Perform it.
    Run,
    /// Ask first, then perform. Carries the question and the id it names.
    Confirm { prompt: String, id: String },
    /// A retired MUTATOR: print this and change nothing.
    Redirect(String),
}

/// Decide how a conversation-op line should be handled (#2009 PR6b).
///
/// Two rules meet here:
///
/// - **A retired mutator must not mutate.** `/conversation restore|rename|
///   delete` redirect and do nothing, the way `/thinking` does, so the shim
///   gets to die instead of half-working forever.
/// - **A retired read may still read.** `/conversation list|show` keep
///   printing, because §3.3 requires reads to work on a pipe and scripts read
///   them today.
/// - **Every destructive op asks first**, at EITHER door — the operator's
///   standing rule for this sweep. `/resume delete` is not exempt for being
///   the new spelling.
pub(crate) fn conversation_op_plan(input: &str) -> anyhow::Result<ConversationOpPlan> {
    let (door, command) = parse_conversation_command_at(input)?;
    if door == ConversationDoor::Retired && command.mutates() {
        let replacement = match &command {
            ConversationCommand::Restore(id) => format!("/resume restore {id}"),
            ConversationCommand::Rename { id, title } => format!("/resume rename {id} {title}"),
            ConversationCommand::Delete(id) => format!("/resume delete {id}"),
            ConversationCommand::List | ConversationCommand::Show(_) => unreachable!(
                "List and Show do not mutate; `mutates()` is the one predicate that says so"
            ),
        };
        return Ok(ConversationOpPlan::Redirect(format!(
            "/conversation is retired — use `{replacement}` (nothing changed)"
        )));
    }
    if let ConversationCommand::Delete(id) = &command {
        return Ok(ConversationOpPlan::Confirm {
            prompt: format!(
                "delete conversation `{id}` and every turn in it? this cannot be undone"
            ),
            id: id.clone(),
        });
    }
    Ok(ConversationOpPlan::Run)
}

/// Ask the delete question through the surface's own seam.
///
/// `true` ONLY for an explicit yes. A cancelled ask — a pipe, EOF, Esc — is a
/// no: §3.3 is explicit that those are outcomes and never an implied answer,
/// and for a destructive op that is the only safe reading.
pub(crate) fn confirm_conversation_delete(ask: SlashAsk<'_>, prompt: &str) -> bool {
    let definition = newt_core::interaction_form::confirm(
        prompt.to_string(),
        "this removes the conversation and its turns",
        "delete it",
        "keep it",
    );
    let interaction =
        newt_core::interaction_surface::SurfaceInteraction::blocking(definition.clone());
    match ask(&interaction) {
        newt_core::HumanQuestionOutcome::Answer(answer) => {
            newt_core::interaction_form::resolve(&definition, answer.trim())
                .is_some_and(|id| id.as_str() == newt_core::interaction_form::YES)
        }
        _ => false,
    }
}

/// The subcommand half, shared by both doors so they cannot diverge on what
/// `rename <id> <title>` means.
fn parse_conversation_rest<'a>(
    door: ConversationDoor,
    mut parts: impl Iterator<Item = &'a str>,
) -> anyhow::Result<(ConversationDoor, ConversationCommand)> {
    let verb = match door {
        ConversationDoor::Resume => "resume",
        ConversationDoor::Retired => "conversation",
    };
    let command = match parts.next() {
        // Bare `/resume` is BROWSE, not a list — the caller handles it before
        // reaching here. Bare `/conversation` is its list, as it always was.
        None | Some("list") => ConversationCommand::List,
        Some("show") => match parts.next() {
            Some(id) => ConversationCommand::Show(id.to_string()),
            None => anyhow::bail!("usage: /{verb} show <id>"),
        },
        Some("restore") => match parts.next() {
            Some(id) => ConversationCommand::Restore(id.to_string()),
            None => anyhow::bail!("usage: /{verb} restore <id>"),
        },
        Some("rename") => {
            let Some(id) = parts.next() else {
                anyhow::bail!("usage: /{verb} rename <id> <title>");
            };
            let title = parts.collect::<Vec<_>>().join(" ");
            if title.trim().is_empty() {
                anyhow::bail!("usage: /{verb} rename <id> <title>");
            }
            ConversationCommand::Rename {
                id: id.to_string(),
                title,
            }
        }
        Some("delete" | "rm") => match parts.next() {
            Some(id) => ConversationCommand::Delete(id.to_string()),
            None => anyhow::bail!("usage: /{verb} delete <id>"),
        },
        Some(other) => anyhow::bail!("unknown conversation command `{other}`"),
    };
    Ok((door, command))
}

/// Build the progressive-disclosure skills index block for the system prompt
/// from the skills found across `skills_dirs` (names + descriptions only, never
/// bodies), or `None` when no skills are installed. Uses the same ordered,
/// first-directory-wins union as discovery. Split out from
/// [`build_system_prompt_with_soul`] so the injection can be unit-tested against
/// controlled directories without mutating the process-wide `$HOME`.
fn skills_index_for_prompt(skills_dirs: &[std::path::PathBuf]) -> Option<String> {
    newt_skills::index_block(&newt_skills::discover_paths(skills_dirs))
}

fn build_system_prompt_with_soul(workspace: &str, soul: Option<&str>, plan_path: &str) -> String {
    build_system_prompt_with_persona(workspace, soul, None, plan_path)
}

/// FR-5 (#999): the minimal persona that carries ONLY an altitude, used when
/// `--altitude` is passed without a `--persona`. No role overlay — just the
/// altitude that selects the base identity — so `--altitude coach` installs
/// COACH_SOUL and nothing else, and `/persona show` names the active altitude.
fn synthetic_altitude_persona(altitude: newt_core::Altitude) -> Persona {
    let name = match altitude {
        newt_core::Altitude::Coach => "coach",
        newt_core::Altitude::Doer => "doer",
    };
    Persona {
        name: name.to_string(),
        prompt: String::new(),
        path: std::path::PathBuf::new(),
        profile: newt_core::RoleProfile {
            altitude: Some(altitude),
            ..Default::default()
        },
    }
}

fn build_system_prompt_with_persona(
    workspace: &str,
    soul: Option<&str>,
    persona: Option<&Persona>,
    plan_path: &str,
) -> String {
    // FR-5 (#999): the active persona's ALTITUDE selects the base identity. A
    // `Coach` altitude REPLACES the identity with COACH_SOUL (and overrides a
    // doer-flavored soul.md), so a coaching persona's overlay no longer sits on
    // top of a contradictory doer soul. Doer (the default, and every persona
    // that doesn't declare an altitude) keeps today's behavior: the resolved
    // soul.md, else DEFAULT_SOUL. The `--altitude` flag rides here too — it
    // seeds/overrides `active_persona`'s altitude at startup (see `run_code`),
    // so it needs no separate plumbing through the session-management helpers.
    let effective_altitude = persona.and_then(|p| p.profile.altitude).unwrap_or_default();
    let identity = match effective_altitude {
        newt_core::Altitude::Coach => newt_core::COACH_SOUL,
        newt_core::Altitude::Doer => soul.unwrap_or(newt_core::DEFAULT_SOUL),
    };
    let mut ctx = format!("{identity}\n\nWorkspace: {workspace}\n");
    if let Some(persona) = persona {
        ctx.push_str(&format!(
            "\nActive persona: {}\n{}\n",
            persona.name, persona.prompt
        ));
    }

    // Per-session plan instruction (issue #220). Injected here, with the
    // resolved per-session path, rather than baked into DEFAULT_SOUL — so the
    // path is dynamic AND custom soul.md users still get the guidance. The path
    // is unique to this conversation, so concurrent newt instances in the same
    // repo never clobber each other's plan.
    ctx.push_str(&format!(
        "\n**Plan before coding.** For any task requiring more than one file \
         change, write a plan to `{plan_path}` first (create it if it does not \
         exist). List the concrete steps and check them off as you complete \
         each one. This plan file is unique to this session — read it when \
         resuming so you can pick up exactly where you left off without \
         re-reading the whole codebase.\n"
    ));

    // Progressive disclosure: inject ONLY the skills index (one
    // `name: description (when to use: …)` line per installed skill) — never
    // the bodies. Bodies load on demand when the model calls the `use_skill`
    // tool. Skills come from the configured search path (`[skills].search`,
    // default `~/.newt/skills`); a missing dir contributes nothing.
    // `with_bundled_default` fills an unset `[skills].bundled_dir` from the
    // repo's `.newt/bundled-skills` when running inside a checkout, so bundled
    // skills are surfaced to the model out-of-the-box.
    let skills_dirs = newt_core::Config::resolve()
        .map(|c| c.with_bundled_default().skill_search_dirs())
        .unwrap_or_default();
    if let Some(index) = skills_index_for_prompt(&skills_dirs) {
        ctx.push('\n');
        ctx.push_str(&index);
    }

    // Directory listing (top-level, no hidden files)
    if let Ok(mut entries) = std::fs::read_dir(workspace) {
        let mut names: Vec<String> = entries
            .by_ref()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    None
                } else {
                    Some(name)
                }
            })
            .collect();
        names.sort();
        ctx.push_str("\nFiles:\n");
        for name in names.iter().take(40) {
            ctx.push_str(&format!("  {name}\n"));
        }
    }

    // README (truncated)
    for readme in ["README.md", "readme.md", "README.txt"] {
        let path = std::path::Path::new(workspace).join(readme);
        if let Ok(text) = std::fs::read_to_string(&path) {
            let excerpt: String = text.chars().take(3000).collect();
            ctx.push_str(&format!("\n{readme}:\n{excerpt}\n"));
            if text.len() > 3000 {
                ctx.push_str("...[truncated]\n");
            }
            break;
        }
    }

    // Recent git log (confused-deputy-safe, step-7.4: the workspace may be hostile)
    let log = newt_core::git_hardening::hardened_git(
        std::path::Path::new(workspace),
        &["log", "--oneline", "-10"],
    )
    .output()
    .ok()
    .and_then(|o| String::from_utf8(o.stdout).ok())
    .unwrap_or_default();
    if !log.is_empty() {
        ctx.push_str(&format!("\nRecent commits:\n{log}"));
    }

    ctx
}

// ---------------------------------------------------------------------------
// Tool-use loop — the real agentic core
// ---------------------------------------------------------------------------

fn rebuild_system_prompt(
    workspace: &str,
    memory: &newt_core::MemoryManager,
    persona: Option<&Persona>,
    conversation_id: &str,
) -> String {
    let soul_additions = memory.build_system_prompt_additions();
    let soul_text = if soul_additions.is_empty() {
        None
    } else {
        Some(soul_additions.as_str())
    };
    // Per-session plan path, keyed on the durable conversation id (issue #220).
    let plan_path = newt_core::session_plan_path(conversation_id);
    build_system_prompt_with_persona(workspace, soul_text, persona, &plan_path.to_string_lossy())
}

fn handle_operating_mode_command(
    arg: &str,
    active_mode: &mut OperatingMode,
    mode_states: &ConversationModeStates,
    color: bool,
    verbose: bool,
) {
    match operating_mode_command_lines(arg, active_mode) {
        Ok(mut lines) => {
            let normalized = arg.trim().to_ascii_lowercase();
            if !matches!(normalized.as_str(), "" | "list" | "show" | "status") {
                // An explicit human selection supersedes any stale
                // model-entered plan phase. `/mode plan` has its own
                // read-only enforcement and does not need the legacy flag.
                mode_states.plan.clear();
                // A successful explicit human selection also supersedes any
                // model-selected Auto style. Listing, status, and invalid
                // commands leave both pieces of state untouched.
                mode_states.auto.clear();
            }
            // #2009 PR4b: one writer for the value. `/mode` and
            // `/settings mode` both land in `set_session_operating_mode`, so
            // the verb and the field cannot select different styles.
            newt_core::operating_mode::set_session_operating_mode(*active_mode);
            if let Some(first) = lines.first() {
                print_newt(first, color, verbose);
            }
            for line in lines.drain(1..) {
                println!("{line}");
            }
        }
        Err(error) => print_newt(&error, color, verbose),
    }
}

fn posture_status_lines(
    cfg: &newt_core::Config,
    active_posture: Option<&ActivePosture>,
    include_available: bool,
) -> Vec<String> {
    let active = match active_posture {
        Some(posture) if posture.permission_clamp().is_none() => {
            format!(
                "active permission posture: {} (no permission clamp)",
                posture.name
            )
        }
        Some(posture) => format!(
            "active permission posture: {} — preset '{}' floor: {}",
            posture.name, posture.preset_name, posture.clamp_summary
        ),
        None => "no active permission posture".to_string(),
    };
    let mut lines = vec![active];
    if include_available {
        let names: Vec<&str> = cfg.modes.keys().map(String::as_str).collect();
        lines.push(if names.is_empty() {
            "available permission postures: (none configured — define [modes.<name>] in your newt config)"
                .to_string()
        } else {
            format!("available permission postures: {}", names.join(", "))
        });
    }
    lines
}

/// #307: handle `/posture <name>`. Atomically preloads any named skill body,
/// applies an optional named permission preset as an authority floor, and
/// carries framing into the live per-turn prompt. Mutates `active_posture` only
/// after every configured component resolves successfully.
fn handle_posture_command(
    arg: &str,
    cfg: &newt_core::Config,
    active_posture: &mut Option<ActivePosture>,
    color: bool,
    verbose: bool,
) {
    // Bare `/posture` and `/posture list` are discovery surfaces. `show` and
    // `status` keep the concise active-only form.
    if matches!(arg, "" | "list" | "show" | "status") {
        let include_available = matches!(arg, "" | "list");
        let mut lines =
            posture_status_lines(cfg, active_posture.as_ref(), include_available).into_iter();
        if let Some(first) = lines.next() {
            print_newt(&first, color, verbose);
        }
        for line in lines {
            println!("{line}");
        }
        return;
    }

    // Drop the clamp and its prompt guidance for the rest of the session.
    if matches!(arg, "off" | "clear" | "reset") {
        if active_posture.take().is_some() {
            print_newt(
                "permission posture cleared — authority returns to the session base",
                color,
                verbose,
            );
        } else {
            print_newt("no active permission posture to clear", color, verbose);
        }
        return;
    }

    // Resolve + validate WITHOUT mutating. The skill loader reuses the SAME
    // `use_skill` path (`load_body_from` over the configured search dirs) —
    // skill dirs are config-rooted, exactly as the `use_skill` tool resolves.
    let skills_dirs = cfg.skill_search_dirs();
    let posture = build_posture(arg, cfg, |skill_name| {
        newt_skills::load_body_from(&skills_dirs, skill_name)
    });
    let posture = match posture {
        Ok(posture) => posture,
        Err(e) => {
            print_newt(&format!("error: {e}"), color, verbose);
            return;
        }
    };

    if let Some(body) = &posture.skill_body {
        print_newt(
            &format!("loaded skill for permission posture '{arg}':\n{body}"),
            color,
            verbose,
        );
    }
    let report = if posture.permission_clamp().is_none() {
        format!(
            "permission posture '{}' active (no permission clamp)",
            posture.name
        )
    } else {
        format!(
            "permission posture '{}' active — preset '{}' clamps authority (floor): {}",
            posture.name, posture.preset_name, posture.clamp_summary
        )
    };
    *active_posture = Some(posture);
    print_newt(&report, color, verbose);
}

fn persona_status(active: Option<&Persona>) -> String {
    let Some(persona) = active else {
        return "No active persona.".to_string();
    };
    let mut out = format!(
        "Active persona: {} - {} ({})",
        persona.name,
        persona.description(),
        persona.path.display()
    );
    let profile = &persona.profile;
    if let Some(role) = &profile.role {
        out.push_str(&format!("\n  role: {role}"));
    }
    match &profile.tools {
        Some(tools) if !tools.is_empty() => {
            out.push_str(&format!("\n  tools: {}", tools.join(", ")));
        }
        Some(_) => out.push_str("\n  tools: (none)"),
        None => out.push_str("\n  tools: (unconstrained)"),
    }
    // FR-4 (#1041): list the persona's bound skill names, same shape as `tools`.
    if let Some(skills) = &profile.skills {
        if skills.is_empty() {
            out.push_str("\n  skills: (none)");
        } else {
            out.push_str(&format!("\n  skills: {}", skills.join(", ")));
        }
    }
    if let Some(caveats) = &profile.caveats {
        out.push_str(&format!("\n  caveats: {}", caveats.summary()));
    }
    match (&profile.model, &profile.tier) {
        (Some(m), Some(t)) => out.push_str(&format!("\n  router: model={m} tier={t:?}")),
        (Some(m), None) => out.push_str(&format!("\n  router: model={m}")),
        (None, Some(t)) => out.push_str(&format!("\n  router: tier={t:?}")),
        (None, None) => {}
    }
    // The psyche dials (backend + cognition/tenacity/crew), shown only when set.
    if let Some(b) = &profile.backend {
        out.push_str(&format!("\n  backend: {b}"));
    }
    if let Some(c) = profile.cognition {
        out.push_str(&format!("\n  cognition: {}", c.label()));
    }
    if let Some(t) = profile.tenacity {
        // P1#3: this is now an APPLIED resolution layer (set_persona_tenacity),
        // not just rendered — it agrees with /psyche's effective_tenacity.
        out.push_str(&format!("\n  tenacity: {}", t.label()));
    }
    if profile.crew == Some(true) {
        // P1#3: crew is a startup gate (NEWT_TEAM builds the crew runner once at
        // launch), so a declaration can't engage it live. Label it honestly so
        // this status can't claim a control is active while the engine ignores it.
        let runtime = if std::env::var("NEWT_TEAM").is_ok() {
            "on"
        } else {
            "off — a launch gate; start with `newt --obsessive` / NEWT_TEAM to engage"
        };
        out.push_str(&format!("\n  crew: declared on · runtime {runtime}"));
    }
    if !profile.is_role_bound() {
        out.push_str("\n  (prompt-only persona — no role bindings)");
    }
    out
}

fn persona_list(store: &PersonaStore) -> anyhow::Result<String> {
    store.list_message()
}

struct ConversationResetContext<'a> {
    memory: &'a mut newt_core::MemoryManager,
    system: &'a mut String,
    conversation_id: &'a mut String,
    mode_states: &'a ConversationModeStates,
}

fn reset_conversation(
    workspace: &str,
    active_persona: Option<&Persona>,
    ctx: &mut ConversationResetContext<'_>,
) {
    // Every caller of this helper crosses a conversation boundary (`/new`,
    // persona clear, or a persona swap without `--keep-context`). Legacy
    // model-entered plan state belongs to the old conversation and must not
    // survive any of those paths.
    ctx.mode_states.clear();
    ctx.memory.reset_all();
    *ctx.system = rebuild_system_prompt(workspace, ctx.memory, active_persona, ctx.conversation_id);
}

fn new_conversation_message(active_persona: Option<&Persona>) -> String {
    match active_persona {
        Some(persona) => format!(
            "Started a new conversation with persona `{}`.",
            persona.name
        ),
        None => "Started a new conversation.".to_string(),
    }
}

/// The line printed after a `/new` · `/end` · `/restart` · `/start` rotation
/// (#1030). `started` is [`new_conversation_message`]. `/new` keeps its bare
/// historical message; the finalizers (`/end`, `/restart`) note the old
/// conversation is saved and `/resume`-able; `/start` notes it stays OPEN. The
/// old "won't resume next launch" wording is retired — with fresh-on-launch
/// nothing auto-resumes, so `/resume` is how you get back either way.
///
/// `outgoing_durable` is true when a durable conversation row exists: an
/// accepted prompt receipt, a completed turn, or an explicitly titled
/// `/start`. A prompt-only failed/cancelled turn is therefore resumable. It is
/// false only when nothing was recorded or the session is ephemeral.
pub(crate) fn close_out_message(reason: &str, started: &str, outgoing_durable: bool) -> String {
    // #1165/#1170: `/end` must LEAD with the ending regardless of whether the
    // outgoing conversation was durable — the operator typed "end" and
    // expects ending language, not "Started a new conversation." A truly
    // untouched conversation had nothing to save, so drop the "/resume to
    // reopen" that it cannot honor; prompt-only durable content keeps it.
    if reason == "end" {
        return if outgoing_durable {
            "Conversation ended and saved — /resume to reopen it, /exit to leave newt. \
             (A fresh conversation is now open.)"
                .to_string()
        } else {
            "Conversation ended — /exit to leave newt. (A fresh conversation is now open.)"
                .to_string()
        };
    }
    if !outgoing_durable {
        return started.to_string();
    }
    match reason {
        "start" => {
            format!("{started} The previous conversation stays open — /resume to return to it.")
        }
        "new" => started.to_string(),
        // "restart"
        _ => format!("{started} The previous conversation is saved — /resume to reopen it."),
    }
}

/// The per-CONVERSATION live state a boundary must clear.
///
/// #1669 PR-A. Deliberately not the whole session: the semantic index, the
/// experiential ledger, and the nav warmup are **shared across tabs on
/// purpose** (ADR, "Global (shared across tabs)"), so they are task-scoped
/// concerns that `/new` handles and `/tab new` must NOT touch — clearing them
/// would let opening a tab throw away work another tab is relying on.
pub(crate) struct ConversationScopedState<'a> {
    pub scratchpad: &'a dyn newt_core::ScratchpadStore,
    pub step_ledger: &'a dyn newt_core::StepLedger,
    pub active_prompt_context: &'a mut Option<newt_core::TurnPromptContext>,
}

impl ConversationScopedState<'_> {
    /// Drop everything that belonged to the outgoing conversation.
    ///
    /// Step 26.4 (#583) scratchpad, Step 26.6b (#586) plan ledger, and the
    /// prompt receipt. One place, so `/new` and `/tab new` cannot drift: a
    /// fresh tab that inherited any of these would be a new conversation
    /// wearing the previous one's working memory.
    pub fn clear(&mut self) {
        self.scratchpad.clear();
        self.step_ledger.clear();
        *self.active_prompt_context = None;
    }
}

fn handle_new_conversation(
    workspace: &str,
    active_persona: Option<&Persona>,
    ctx: &mut ConversationResetContext<'_>,
    compress_state: &mut newt_core::CompressState,
    session_opted_fresh: &mut bool,
    scoped: &mut ConversationScopedState<'_>,
) -> String {
    // A new conversation gets a fresh id, which rotates the per-session plan
    // path to a new `.scratch/sessions/<id>/` dir (issue #220).
    *ctx.conversation_id = newt_core::new_conversation_id();
    // Re-arm compression anti-thrash (F4): the disable notice promises
    // "start a new conversation to reset" — this is what makes that true.
    compress_state.reset();
    // 17.7: an explicit /new opts this session out of auto-resume for good
    // (`should_auto_resume` consults the flag) — resume never undoes /new.
    *session_opted_fresh = true;
    reset_conversation(workspace, active_persona, ctx);
    scoped.clear();
    new_conversation_message(active_persona)
}

// ---------------------------------------------------------------------------
// Session-start conversation resolution (Step 17.7, issue #246)
// ---------------------------------------------------------------------------

/// What `/conversation` and `/recall` answer in an ephemeral session, and the
/// one-time startup notice — same wording so the mode is unmistakable.
const EPHEMERAL_SESSION_NOTICE: &str =
    "ephemeral session — conversation persistence is off (nothing saved, nothing resumed)";

/// How this session treats conversation persistence. Resolved ONCE at
/// session start by [`resolve_session_start`]; precedence:
/// `--ephemeral`/`NEWT_EPHEMERAL` > `NEWT_CONVERSATION_ID` >
/// `[conversations] resume` (#1030: default FALSE = fresh-on-launch;
/// `resume = true` opts back into auto-resuming the folder's latest).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionStart {
    /// No persistence at all: no store handle is constructed, so no
    /// conversation row can be created, no turn appended, and no past
    /// conversation read.
    Ephemeral,
    /// Resume exactly this conversation id (`NEWT_CONVERSATION_ID`). The id
    /// must exist in THIS workspace — the 17.1b workspace fence applies.
    ResumeExact(String),
    /// #1671: resume by NAME (`newt --resume <name>` / `NEWT_RESUME`) — a
    /// title (or id/prefix) resolved against this workspace's conversations
    /// once the store is open. Like `ResumeExact`, errors are hard: a name
    /// that matches nothing (or several) must not silently start fresh.
    ResumeNamed(String),
    /// Auto-resume the workspace's most recently active conversation —
    /// highest §6 activity tick, never a timestamp comparison.
    ResumeLatest,
    /// Persist turns as usual, but start a fresh conversation
    /// (`[conversations] resume = false`).
    Fresh,
}

/// Pure precedence chain (17.7): ephemeral wins outright, then an explicit
/// conversation id, then a name (#1671), then the config default. A blank
/// `NEWT_CONVERSATION_ID` / `NEWT_RESUME` reads as unset rather than as a
/// target that can never exist.
fn resolve_session_start(
    ephemeral: bool,
    forced_id: Option<String>,
    resume_name: Option<String>,
    resume_config: bool,
) -> SessionStart {
    if ephemeral {
        return SessionStart::Ephemeral;
    }
    if let Some(id) = forced_id {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return SessionStart::ResumeExact(id);
        }
    }
    if let Some(name) = resume_name {
        let name = name.trim().to_string();
        if !name.is_empty() {
            return SessionStart::ResumeNamed(name);
        }
    }
    if resume_config {
        SessionStart::ResumeLatest
    } else {
        SessionStart::Fresh
    }
}

/// Structured title-match result — the ONE pure core behind both
/// `resolve_conversation_by_name` (startup `--resume <name>`) and the
/// consolidated `resolve_resume_target` (in-chat `/resume <thing>`), so the
/// title-matching rules can never drift between the two front doors.
///
/// Titles are matched case-insensitively: a unique EXACT match wins; otherwise
/// a unique substring match; several matches are `Ambiguous`; none is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TitleMatch<'a> {
    One(&'a newt_core::ConversationSummary),
    Ambiguous(Vec<&'a newt_core::ConversationSummary>),
    None,
}

fn title_match<'a>(
    summaries: &'a [newt_core::ConversationSummary],
    needle: &str,
) -> TitleMatch<'a> {
    let needle = needle.trim().to_lowercase();
    let title_of = |s: &newt_core::ConversationSummary| s.title.trim().to_lowercase();
    let exact: Vec<&newt_core::ConversationSummary> =
        summaries.iter().filter(|s| title_of(s) == needle).collect();
    let hits = if exact.is_empty() {
        summaries
            .iter()
            .filter(|s| title_of(s).contains(needle.as_str()))
            .collect::<Vec<_>>()
    } else {
        exact
    };
    match hits.as_slice() {
        [one] => TitleMatch::One(one),
        [] => TitleMatch::None,
        many => TitleMatch::Ambiguous(many.to_vec()),
    }
}

/// The structured title-resolution error — the two failure modes of
/// `resolve_conversation_by_name`, carried as data so the consolidated
/// `resolve_resume_target` can branch on them (FTS fallback vs. candidate
/// listing) instead of parsing a string. `Display` reproduces the exact
/// human-facing messages the slash-command path always printed, so existing
/// `unwrap_err().contains(...)` regression coverage carries over unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TitleResolveError {
    /// No title matched the query.
    NotFound { query: String },
    /// Several titles matched — name the candidates so the human can pick.
    Ambiguous {
        query: String,
        candidates: Vec<(String, String)>,
    },
}

impl std::fmt::Display for TitleResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { query } => write!(
                f,
                "no conversation titled \"{query}\" in this workspace — run newt and /resume to browse"
            ),
            Self::Ambiguous { query, candidates } => write!(
                f,
                "\"{query}\" matches {} conversations — use an id: {}",
                candidates.len(),
                candidates
                    .iter()
                    .map(|(id, title)| format!("{} \"{title}\"", short_conversation_id(id)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::error::Error for TitleResolveError {}

/// #1671: resolve `--resume <name>` against this workspace's conversations by
/// TITLE — pure, so the matching rules are unit-testable without a store. A
/// unique exact (case-insensitive) title wins, then a unique substring;
/// ambiguity and misses are hard errors that NAME the candidates (an ambiguous
/// or missing name must never silently open the wrong conversation). Ids are
/// not this function's job — the consolidated `resolve_resume_target` tries
/// id/prefix first, then delegates the title step here.
fn resolve_conversation_by_name(
    summaries: &[newt_core::ConversationSummary],
    name: &str,
) -> Result<String, TitleResolveError> {
    match title_match(summaries, name) {
        TitleMatch::One(one) => Ok(one.id.clone()),
        TitleMatch::None => Err(TitleResolveError::NotFound {
            query: name.trim().to_string(),
        }),
        TitleMatch::Ambiguous(many) => Err(TitleResolveError::Ambiguous {
            query: name.trim().to_string(),
            candidates: many
                .iter()
                .map(|s| (s.id.clone(), s.title.clone()))
                .collect(),
        }),
    }
}

/// The consolidated resume resolver (#1030/#1671): ONE precedence chain
/// shared by startup `--resume <name>` and in-chat `/resume <thing>`, so the
/// two front doors never drift into different matching rules. Pure — it
/// matches against the workspace's conversation summaries, no store needed.
///
/// Precedence:
///   1. exact conversation id
///   2. unique id prefix (byte-case-exact, like `ConversationStore::resolve_id`)
///   3. exact (case-insensitive) title
///   4. unique (case-insensitive) title substring
///   5. ambiguous title match → `Ambiguous` (the caller renders a numbered
///      listing so a follow-up `/resume <n>` selects one)
///   6. nothing matched → `NotFound` (the caller falls back to FTS search)
///
/// Full-text search is deliberately NOT here: it is the listing fallback the
/// in-chat caller renders (and a `/resume <n>` then selects from). Startup
/// `--resume <name>` has no listing to show, so it hard-errors on
/// `Ambiguous`/`NotFound` instead. Keeping FTS out leaves this pure and
/// unit-testable without a store.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResumeNameResolve {
    /// A single conversation resolved — open it.
    Resolved(String),
    /// Several title matches — present them for numbered selection.
    Ambiguous(Vec<(String, String)>),
    /// Nothing matched by id or title — the caller may fall back to FTS.
    NotFound,
}

fn resolve_resume_target(
    summaries: &[newt_core::ConversationSummary],
    token: &str,
) -> ResumeNameResolve {
    let token = token.trim();
    // 1. exact conversation id.
    if let Some(found) = summaries.iter().find(|s| s.id == token) {
        return ResumeNameResolve::Resolved(found.id.clone());
    }
    // 2. unique id prefix (byte-case-exact; ids are validated ASCII so byte
    //    and char positions coincide, matching `ConversationStore::resolve_id`).
    let prefix: Vec<&newt_core::ConversationSummary> = summaries
        .iter()
        .filter(|s| s.id.starts_with(token))
        .collect();
    if let [one] = prefix.as_slice() {
        return ResumeNameResolve::Resolved(one.id.clone());
    }
    // 3-5. exact title / unique substring / ambiguous — DELEGATED to the
    //      title-only resolver so the matching rules live in ONE place and
    //      the two front doors (startup `--resume <name>` and in-chat
    //      `/resume <thing>`) can never drift.
    match resolve_conversation_by_name(summaries, token) {
        Ok(id) => ResumeNameResolve::Resolved(id),
        Err(TitleResolveError::Ambiguous { candidates, .. }) => {
            ResumeNameResolve::Ambiguous(candidates)
        }
        Err(TitleResolveError::NotFound { .. }) => ResumeNameResolve::NotFound,
    }
}

/// The single gate every auto-resume consult goes through: config-driven
/// resume happens only when the session has NOT explicitly opted fresh via
/// `/new` — auto-resume never undoes an explicit /new (17.7).
fn should_auto_resume(start: &SessionStart, session_opted_fresh: bool) -> bool {
    matches!(start, SessionStart::ResumeLatest) && !session_opted_fresh
}

fn conversation_root_dir() -> std::path::PathBuf {
    newt_core::Config::user_config_path()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| std::path::PathBuf::from(".newt"))
}

fn conversation_store_for(
    workspace: &str,
    cfg: &newt_core::Config,
) -> anyhow::Result<newt_core::ConversationStore> {
    let max_per_workspace = cfg
        .conversations
        .clone()
        .unwrap_or_default()
        .max_per_workspace;
    newt_core::ConversationStore::new(conversation_root_dir(), workspace, max_per_workspace)
}

fn conversation_title_from_task(task: &str) -> String {
    let title: String = task
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Untitled conversation")
        .chars()
        .take(80)
        .collect();
    if title.trim().is_empty() {
        "Untitled conversation".to_string()
    } else {
        title
    }
}

/// What the per-turn persistence seam knows after attempting a save.
///
/// A reply row is appended before scratchpad/plan/claim bookkeeping. Those
/// later updates are useful but must not erase the fact that the transcript is
/// already durable; callers use this distinction to append digest-only
/// provenance honestly.
#[derive(Debug)]
enum TurnSaveState {
    /// The reply and all ancillary session state were saved durably.
    Durable,
    /// The reply is durable, but a later ancillary update failed.
    DurableWithAncillaryWarning(anyhow::Error),
    /// `--ephemeral`: no SQLite row exists, but session-local state is live.
    Ephemeral,
}

#[allow(clippy::too_many_arguments)]
fn save_successful_conversation_turn(
    store: &newt_core::ConversationStore,
    conversation_id: &str,
    active_persona: Option<&Persona>,
    task: &str,
    reply: &str,
    events: &[newt_core::ToolEvent],
    phantom_reaches: &[newt_core::PhantomReach],
    usage: Option<newt_core::TokenUsage>,
    compaction: Option<String>,
    scratchpad: &std::collections::BTreeMap<String, String>,
    plan: &newt_core::PlanSnapshot,
) -> anyhow::Result<TurnSaveState> {
    save_successful_conversation_turn_with_ancillary(
        store,
        conversation_id,
        active_persona,
        task,
        reply,
        events,
        phantom_reaches,
        usage,
        compaction,
        scratchpad,
        plan,
        |store, conversation_id, scratchpad, plan| {
            // #713: snapshot the live scratchpad <state> onto the conversation
            // row so a later interrupt + auto-resume can re-hydrate it.
            store
                .update_scratchpad(conversation_id, scratchpad)
                .context("reply persisted but scratchpad snapshot could not be updated")?;
            // #715: snapshot the live plan ledger onto the conversation row,
            // alongside the scratchpad and under the same discipline.
            store
                .update_plan_snapshot(conversation_id, plan)
                .context("reply persisted but plan snapshot could not be updated")?;
            // #1030: refresh this process's live-owner heartbeat every turn.
            store
                .heartbeat(conversation_id)
                .context("reply persisted but conversation heartbeat could not be refreshed")
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn save_successful_conversation_turn_with_ancillary<F>(
    store: &newt_core::ConversationStore,
    conversation_id: &str,
    active_persona: Option<&Persona>,
    task: &str,
    reply: &str,
    events: &[newt_core::ToolEvent],
    phantom_reaches: &[newt_core::PhantomReach],
    usage: Option<newt_core::TokenUsage>,
    compaction: Option<String>,
    scratchpad: &std::collections::BTreeMap<String, String>,
    plan: &newt_core::PlanSnapshot,
    ancillary: F,
) -> anyhow::Result<TurnSaveState>
where
    F: FnOnce(
        &newt_core::ConversationStore,
        &str,
        &std::collections::BTreeMap<String, String>,
        &newt_core::PlanSnapshot,
    ) -> anyhow::Result<()>,
{
    // The id is pre-assigned for the whole session (issue #220), so the first
    // turn creates the record with that id; later turns just append.
    // `exists()?` — an error must NOT read as "absent": routing a transient
    // failure into create_with_id would overwrite a live conversation.
    if !store.exists(conversation_id)? {
        store.create_with_id(
            conversation_id,
            &conversation_title_from_task(task),
            active_persona.map(|p| p.name.as_str()),
        )?;
    }
    // 18.5 (#247): a compaction summary minted while syncing THIS turn is
    // persisted as its own turn record (user = the marked message, assistant
    // empty, token columns NULL — it is not a backend-measured turn), and it
    // goes in BEFORE the triggering turn: the live boundary's last-user
    // anchor guarantees the triggering turn survived compression, so restore
    // rebuilds `[summary] + [turns from the trigger on]` — the same working
    // set the live session kept.
    if let Some(summary) = compaction {
        // Sources stay empty until the producer plumbing lands (#1786 spec
        // §8, Phase C): ids flow from the store post-save, one cycle late.
        store.append_turn_full(conversation_id, &summary, "", &[], &[], &[], None, None)?;
    }
    // 17.6: persist the turn's tool events and the backend-reported token
    // actuals. `usage` is what `chat_complete` returned — input = largest
    // single prompt of the turn, output = sum across rounds (Step 18.1
    // semantics); `None` (backend reported nothing) is stored as NULL.
    // #717: phantom reaches persist alongside the events column.
    store.append_turn_full(
        conversation_id,
        task,
        reply,
        events,
        phantom_reaches,
        // A model turn is witnessed, never derived: empty sources always.
        &[],
        usage.map(|u| u.input_tokens),
        usage.map(|u| u.output_tokens),
    )?;
    match ancillary(store, conversation_id, scratchpad, plan) {
        Ok(()) => Ok(TurnSaveState::Durable),
        Err(error) => Ok(TurnSaveState::DurableWithAncillaryWarning(error)),
    }
}

/// The run loop's per-turn save seam (17.7): persistent sessions route to
/// [`save_successful_conversation_turn`]; an ephemeral session has no store
/// (`None`) and this makes no SQLite write. Its caller still receives
/// [`TurnSaveState::Ephemeral`] so it can maintain the process-local prompt
/// artifact ledger, including a compaction checkpoint when appropriate.
#[allow(clippy::too_many_arguments)] // mirrors save_successful_conversation_turn
fn save_turn_if_persistent(
    store: Option<&newt_core::ConversationStore>,
    conversation_id: &str,
    active_persona: Option<&Persona>,
    task: &str,
    reply: &str,
    events: &[newt_core::ToolEvent],
    phantom_reaches: &[newt_core::PhantomReach],
    usage: Option<newt_core::TokenUsage>,
    compaction: Option<String>,
    scratchpad: &std::collections::BTreeMap<String, String>,
    plan: &newt_core::PlanSnapshot,
) -> anyhow::Result<TurnSaveState> {
    match store {
        Some(store) => save_successful_conversation_turn(
            store,
            conversation_id,
            active_persona,
            task,
            reply,
            events,
            phantom_reaches,
            usage,
            compaction,
            scratchpad,
            plan,
        ),
        None => Ok(TurnSaveState::Ephemeral),
    }
}

fn conversation_list_message(store: &newt_core::ConversationStore) -> anyhow::Result<String> {
    let summaries = store.list()?;
    if summaries.is_empty() {
        return Ok("No saved conversations for this workspace.".to_string());
    }

    let mut out = "Saved conversations:".to_string();
    for summary in summaries {
        let persona = summary
            .persona
            .as_deref()
            .map(|p| format!(" persona={p}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n  {}  {} ({} turns{})",
            summary.id, summary.title, summary.turn_count, persona
        ));
    }
    Ok(out)
}

fn conversation_show_message(record: &newt_core::ConversationRecord) -> String {
    let persona = record
        .persona
        .as_deref()
        .map(|p| format!("persona: {p}"))
        .unwrap_or_else(|| "persona: none".to_string());
    let mut out = format!(
        "Conversation {}: {}\n{}\nturns: {}",
        record.id,
        record.title,
        persona,
        record.turns.len()
    );
    for (idx, turn) in record.turns.iter().enumerate() {
        out.push_str(&format!(
            "\n\n{}. user:\n{}\n\nassistant:\n{}",
            idx + 1,
            turn.user,
            turn.assistant
        ));
    }
    out
}

struct ConversationCommandContext<'a> {
    store: &'a newt_core::ConversationStore,
    persona_store: &'a PersonaStore,
    workspace: &'a str,
    memory: &'a mut newt_core::MemoryManager,
    system: &'a mut String,
    active_persona: &'a mut Option<Persona>,
    active_conversation_id: &'a mut String,
    /// Session compression anti-thrash state — reset on `/conversation
    /// restore`, which is a conversation boundary like `/new` (F4).
    compress_state: &'a mut newt_core::CompressState,
    /// The live scratchpad `<state>` store (#713). Re-hydrated from
    /// `record.scratchpad` on restore so a resumed `state_get("current_task")`
    /// returns the saved value instead of "no such key".
    scratchpad: &'a dyn newt_core::ScratchpadStore,
    /// The live plan ledger (#715). Re-hydrated from `record.plan` on restore so
    /// a resumed `<plan>` block / `plan_get` returns the saved plan — with the
    /// correct active step and done statuses — instead of an empty ledger.
    step_ledger: &'a dyn newt_core::StepLedger,
    /// Latest submitted prompt metadata for the active conversation. Restore
    /// rehydrates it for addressability, but never turns it into queued input:
    /// resuming a prompt-only conversation still waits for the operator.
    active_prompt_context: &'a mut Option<newt_core::TurnPromptContext>,
    /// A model-selected Auto style is one-shot conversation state. Every
    /// successful restore is a hard boundary and clears it before adopting
    /// the restored conversation, including A→B→A switches.
    mode_states: &'a ConversationModeStates,
}

fn handle_conversation_command(
    input: &str,
    ctx: &mut ConversationCommandContext<'_>,
) -> anyhow::Result<String> {
    match parse_conversation_command(input)? {
        ConversationCommand::List => conversation_list_message(ctx.store),
        ConversationCommand::Show(id) => {
            let record = ctx.store.load(&id)?;
            Ok(conversation_show_message(&record))
        }
        ConversationCommand::Restore(id) => {
            let (record, warning) = restore_conversation_into_session(ctx, &id)?;
            // #2085 PR-E2: the three mutating conversation ops journal HERE,
            // after the store call succeeded, because this is the one place
            // both doors converge — a future door cannot reach the mutation
            // without passing the record. `record.id` is the resolved id, not
            // the prefix an operator typed.
            crate::event_receipt::conversation_op(input, "restore", &record.id);
            let mut message = format!(
                "Restored conversation `{}` ({} turns).",
                record.title,
                record.turns.len()
            );
            if let Some(warning) = warning {
                message.push_str(&format!("\nwarning: {warning}"));
            }
            Ok(message)
        }
        ConversationCommand::Rename { id, title } => {
            let resolved_id = ctx.store.resolve_id(&id)?;
            ctx.store.rename(&resolved_id, &title)?;
            crate::event_receipt::conversation_op(input, "rename", &resolved_id);
            Ok(format!("Renamed conversation `{resolved_id}`."))
        }
        ConversationCommand::Delete(id) => {
            let resolved_id = ctx.store.resolve_id(&id)?;
            if *ctx.active_conversation_id == resolved_id {
                anyhow::bail!("cannot delete the active conversation; use /new first");
            }
            ctx.store.delete(&resolved_id)?;
            crate::event_receipt::conversation_op(input, "delete", &resolved_id);
            Ok(format!("Deleted conversation `{resolved_id}`."))
        }
    }
}

/// THE restore implementation — `/conversation restore`, startup auto-resume
/// (17.7), and the `NEWT_CONVERSATION_ID` override all run through here, so
/// there is exactly one way a stored conversation becomes the live session:
/// history turns replace the session history, the saved persona is restored
/// (cleared with a warning when unavailable), compression anti-thrash
/// re-arms (a restore is a conversation boundary like `/new`, F4), and the
/// conversation's id is adopted BEFORE the system prompt rebuild so the
/// prompt names that conversation's plan file (issue #220) — resuming a
/// conversation resumes its plan.
/// Everything a restore needs, already read and validated — so applying it
/// cannot fail.
///
/// #1669 PR-A (P0): restore used to be one function whose fallible store reads
/// happened to precede its first mutation. That is a correct ORDER but not a
/// transaction: a caller could only preflight what it knew to preflight, and a
/// tab switch preflighted `exists` + `load` while restore also validated the
/// prompt receipt and its context. A conversation whose row loaded but whose
/// receipt did not would therefore fail AFTER the outgoing tab had been
/// deactivated and the active pointer moved.
///
/// Splitting it makes the transaction explicit: PREPARE performs every fallible
/// read and mutates nothing; COMMIT consumes this and performs no fallible
/// store read at all, so it cannot fail and there is no error for a caller to
/// swallow.
pub(crate) struct PreparedConversationRestore {
    record: newt_core::ConversationRecord,
    /// Validated prompt lineage, resolved during prepare.
    prompt_context: Option<newt_core::TurnPromptContext>,
    /// Resolved during prepare too — the persona store is a second fallible
    /// source, and leaving it in commit would keep a read on the apply path.
    persona: Option<Persona>,
    /// Set when the record NAMES a persona that could not be loaded. Not an
    /// error: the restore proceeds with no persona and the caller reports it.
    persona_warning: Option<String>,
}

/// Forces a post-`load` prepare failure for one conversation, on one test
/// thread. Compiled out of the shipped binary.
///
/// Exists because the P0 hazard cannot be reached by corrupting a row: it needs
/// the row to load cleanly and a SUBSEQUENT read — the prompt receipt or its
/// context — to fail. Deterministic, no sleeps, no filesystem surgery.
#[cfg(test)]
pub(crate) mod restore_prepare_seam {
    use std::cell::RefCell;

    thread_local! {
        static FAILING: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// Make preparing `id` fail AFTER its row loads.
    pub(crate) fn fail_after_load_for(id: &str) {
        FAILING.with(|f| *f.borrow_mut() = Some(id.to_string()));
    }

    pub(crate) fn clear() {
        FAILING.with(|f| *f.borrow_mut() = None);
    }

    pub(crate) fn forced_failure(id: &str) -> Option<String> {
        FAILING.with(|f| {
            f.borrow()
                .as_deref()
                .filter(|failing| *failing == id)
                .map(|_| "prompt receipt context failed to validate (test seam)".to_string())
        })
    }
}

/// PREPARE — every fallible read for restoring `id`, mutating zero live state.
///
/// Callers that must not half-apply (a tab switch, an adoption) call this
/// BEFORE they deactivate anything. A failure here means nothing has moved.
pub(crate) fn prepare_conversation_restore(
    store: &newt_core::ConversationStore,
    persona_store: &PersonaStore,
    id: &str,
) -> anyhow::Result<PreparedConversationRestore> {
    // Integrity gate (#1785): the record that becomes the model's context is
    // verified and materialized by ONE store operation, from one SQLite read
    // snapshot — `load_verified`, never verify-then-load. Two separate calls
    // would verify one database state and hand back another: a legitimate
    // concurrent append between them reads as corruption, and a real
    // corruption between them reads as clean. Restore is the moment history
    // is about to become the model's context, which is exactly when silently
    // trusting an unverified record would do its damage; it fails the restore
    // while the session is still entirely on the outgoing conversation, the
    // same contract the prompt receipts already follow below.
    //
    // A failure REFUSES the restore and nothing else: no repair, no re-chain,
    // no deletion — the rows stay readable and the error carries the
    // per-turn or tip-witness diagnosis so the damage can be examined rather
    // than papered over.
    let record = store.load_verified(id)?;
    // Test seam modelling the exact P0 hazard: the ROW loads, and a LATER
    // validation fails. Placed after `load` on purpose — a seam before it would
    // only re-prove what a load-only preflight already caught.
    #[cfg(test)]
    if let Some(e) = restore_prepare_seam::forced_failure(id) {
        anyhow::bail!(e);
    }
    // Prompt receipts are a parallel immutable log, not presentation history.
    // Resolving them HERE is the whole point of the split: a corrupt receipt
    // must fail the restore while the session is still entirely on the
    // outgoing conversation.
    let prompt_context = match store.latest_prompt(&record.id)? {
        Some(receipt) => store.turn_prompt_context(&record.id, receipt.id())?,
        None => None,
    };
    let (persona, persona_warning) = match record.persona.as_deref() {
        Some(name) => match persona_store.load(name) {
            Ok(persona) => (Some(persona), None),
            Err(e) => (None, Some(format!("persona `{name}` unavailable: {e}"))),
        },
        None => (None, None),
    };
    Ok(PreparedConversationRestore {
        record,
        prompt_context,
        persona,
        persona_warning,
    })
}

/// What COMMIT is allowed to touch.
///
/// #1669 PR-A. Deliberately **has no `store` and no `persona_store`**. That is
/// the whole guard: the prepare/commit split is only a real transaction while
/// commit performs no fallible read, and a comment saying so decays the first
/// time someone needs "just one more lookup" on the apply path. With no handle
/// to read from, that edit does not compile.
///
/// Every field here is one `ConversationCommandContext` already owns; this is a
/// narrowing reborrow, not a second source of truth.
pub(crate) struct CommitContext<'a> {
    pub workspace: &'a str,
    pub memory: &'a mut newt_core::MemoryManager,
    pub system: &'a mut String,
    pub active_persona: &'a mut Option<Persona>,
    pub active_conversation_id: &'a mut String,
    pub compress_state: &'a mut newt_core::CompressState,
    pub scratchpad: &'a dyn newt_core::ScratchpadStore,
    pub step_ledger: &'a dyn newt_core::StepLedger,
    pub active_prompt_context: &'a mut Option<newt_core::TurnPromptContext>,
    pub mode_states: &'a ConversationModeStates,
}

impl<'a> ConversationCommandContext<'a> {
    /// Narrow to what commit may touch — dropping the two stores.
    pub(crate) fn commit_context(&mut self) -> CommitContext<'_> {
        CommitContext {
            workspace: self.workspace,
            memory: self.memory,
            system: self.system,
            active_persona: self.active_persona,
            active_conversation_id: self.active_conversation_id,
            compress_state: self.compress_state,
            scratchpad: self.scratchpad,
            step_ledger: self.step_ledger,
            active_prompt_context: self.active_prompt_context,
            mode_states: self.mode_states,
        }
    }
}

/// COMMIT — install a prepared restore. Infallible by construction.
///
/// Performs no store read: everything it needs was resolved by
/// [`prepare_conversation_restore`]. That is what lets a caller order
/// prepare → deactivate → switch → commit and know the commit cannot strand it
/// half-way.
pub(crate) fn commit_conversation_restore(
    ctx: &mut CommitContext<'_>,
    prepared: PreparedConversationRestore,
) -> (newt_core::ConversationRecord, Option<String>) {
    let PreparedConversationRestore {
        record,
        prompt_context,
        persona,
        persona_warning,
    } = prepared;
    commit_prepared(ctx, record, prompt_context, persona, persona_warning)
}

fn restore_conversation_into_session(
    ctx: &mut ConversationCommandContext<'_>,
    id: &str,
) -> anyhow::Result<(newt_core::ConversationRecord, Option<String>)> {
    // The un-split entry point every non-tab caller still uses: prepare then
    // commit, so there is exactly ONE restore implementation.
    let prepared = prepare_conversation_restore(ctx.store, ctx.persona_store, id)?;
    Ok(commit_conversation_restore(
        &mut ctx.commit_context(),
        prepared,
    ))
}

fn commit_prepared(
    ctx: &mut CommitContext<'_>,
    record: newt_core::ConversationRecord,
    restored_prompt_context: Option<newt_core::TurnPromptContext>,
    prepared_persona: Option<Persona>,
    warning: Option<String>,
) -> (newt_core::ConversationRecord, Option<String>) {
    // Restore is a conversation boundary. Clear the legacy model-entered plan
    // clamp only after the record and prompt receipt validate, so a failed
    // restore cannot partially mutate the live session.
    ctx.mode_states.clear();
    ctx.memory.restore_turns(&record.turns);
    // #713: re-hydrate the live scratchpad <state> store from the restored
    // record, right next to `restore_turns`, so an interrupt + auto-resume gives
    // the model back its structured working memory — `state_get("current_task")`
    // survives instead of returning "no such key". A restore is a conversation
    // boundary, so clear first (the live store may hold a prior conversation's
    // keys) and then replay the snapshot.
    ctx.scratchpad.clear();
    for (key, value) in &record.scratchpad {
        ctx.scratchpad.set(key, value.clone());
    }
    // #715: re-hydrate the live plan ledger from the restored record, right next
    // to the scratchpad re-hydration, so a resumed `<plan>` block / `plan_get`
    // returns the saved plan instead of an empty ledger. `restore` is a full
    // clear + replace, so it both drops any prior conversation's steps (a
    // restore is a conversation boundary) and reinstates the saved active step +
    // done statuses verbatim.
    ctx.step_ledger.restore(&record.plan);
    // Rehydrate the verified metadata so prompt handles remain resolvable,
    // while leaving the input queue untouched (no auto-execution).
    *ctx.active_prompt_context = restored_prompt_context;
    // Resolved during PREPARE — no fallible read on the apply path.
    *ctx.active_persona = prepared_persona;
    // #1668 review-2 finding 5: `active_persona` is only half of activating a
    // persona — `handle_persona_command` also re-seats the PERSONA COGNITION
    // LAYER that `effective_cognition` ranks beneath the CLI layer. A restore
    // that swapped the struct but left that layer alone carried the OUTGOING
    // conversation's persona cognition into the incoming one, so resuming a
    // plain conversation from a `contemplating` persona kept contemplating with
    // nothing on screen naming a persona to explain it.
    //
    // Derived from the persona now installed rather than from `record.persona`,
    // so the load-failure arm (persona named but unavailable) clears the layer
    // instead of leaving a stale one behind a persona that is not active.
    newt_core::cognition::set_persona_cognition(
        ctx.active_persona
            .as_ref()
            .and_then(|p| p.profile.cognition),
    );
    ctx.compress_state.reset();
    *ctx.active_conversation_id = record.id.clone();
    *ctx.system = rebuild_system_prompt(
        ctx.workspace,
        ctx.memory,
        ctx.active_persona.as_ref(),
        ctx.active_conversation_id,
    );
    (record, warning)
}

/// Resume `id` into the session and return the 17.7 resume banner.
fn resume_session_conversation(
    ctx: &mut ConversationCommandContext<'_>,
    id: &str,
) -> anyhow::Result<String> {
    let (record, warning) = restore_conversation_into_session(ctx, id)?;
    let title = recall_display_title(ctx.store, &record.id, &record.title);
    Ok(auto_resume_banner(&record, &title, warning.as_deref()))
}

/// Auto-resume this workspace's most recently active conversation: highest
/// §6 activity tick — `list()` is tick-ascending, so its LAST entry; never
/// a timestamp comparison. `Ok(None)` when the workspace has no saved
/// conversations yet (fresh start, no banner).
fn auto_resume_latest(ctx: &mut ConversationCommandContext<'_>) -> anyhow::Result<Option<String>> {
    // `latest_open` skips conversations ended via `/end` · `/restart` · `:wq`
    // (their `end_reason` is set) so an explicitly closed-out conversation is
    // never silently re-entered — yet it stays in `list()` for `/recall`. With
    // no open conversation left, the session starts fresh (`Ok(None)`).
    let Some(latest) = ctx.store.latest_open()? else {
        return Ok(None);
    };
    resume_session_conversation(ctx, &latest.id).map(Some)
}

/// `NEWT_CONVERSATION_ID` exact resume (17.7). Exact means exact: no prefix
/// resolution, and the id must belong to THIS workspace — `exists()` is
/// workspace-fenced (17.1b), so a foreign workspace's conversation reads as
/// absent here and can neither be inspected nor resumed across the fence.
fn resume_exact_conversation(
    ctx: &mut ConversationCommandContext<'_>,
    id: &str,
) -> anyhow::Result<String> {
    if !ctx.store.exists(id)? {
        anyhow::bail!(
            "NEWT_CONVERSATION_ID: conversation `{id}` does not exist in this \
             workspace (the workspace fence applies — a conversation belonging \
             to another workspace cannot be resumed here)"
        );
    }
    resume_session_conversation(ctx, id)
}

/// The 17.7 resume banner, printed through the startup notice path. The
/// wall-clock "last active" renders as a display *claim* (`~`-prefixed,
/// the 17.4 convention) — ordering came from the activity tick, never from
/// this timestamp (§6).
fn auto_resume_banner(
    record: &newt_core::ConversationRecord,
    display_title: &str,
    warning: Option<&str>,
) -> String {
    let mut banner = format!(
        "resumed conversation {}  {}  ({} turns, last active ~{}) — /new starts fresh",
        short_conversation_id(&record.id),
        display_title,
        record.turns.len(),
        claim_timestamp(record.updated_at_unix_nanos),
    );
    // #713: tell the model its scratchpad <state> came back, so it reads its
    // task instead of blind-probing `state_get("current_task")` on a store it
    // assumes is empty. Silent when nothing was restored (the OFF/empty case).
    let restored_keys = record.scratchpad.len();
    if restored_keys > 0 {
        banner.push_str(&format!(
            " — restored {restored_keys} <state> key{}",
            if restored_keys == 1 { "" } else { "s" }
        ));
        // #718: an actionable pointer beats a bare key count. If the restored
        // scratchpad carries `current_task`, surface its value so the resumed
        // model reads "where we left off" instead of blind-probing for it.
        if let Some(task) = record.scratchpad.get("current_task") {
            let task = task.trim();
            if !task.is_empty() {
                const MAX: usize = 80;
                let shown: String = task.chars().take(MAX).collect();
                let ellipsis = if task.chars().count() > MAX {
                    "…"
                } else {
                    ""
                };
                banner.push_str(&format!(" — last task: {shown}{ellipsis}"));
            }
        }
    }
    // #715: note a restored plan so the model knows its <plan> / plan_get came
    // back across the resume. Silent when the plan is empty (the OFF/empty case).
    let restored_steps = record.plan.len();
    if restored_steps > 0 {
        banner.push_str(&format!(
            " — restored plan ({restored_steps} step{})",
            if restored_steps == 1 { "" } else { "s" }
        ));
    }
    if let Some(warning) = warning {
        banner.push_str(&format!("\nwarning: {warning}"));
    }
    banner
}

// ---------------------------------------------------------------------------
// /recall — cross-session conversation recall (Step 17.4, issue #246)
// ---------------------------------------------------------------------------

/// Both `/recall` modes show at most this many rows. Browse truncates to the
/// most recent and says so; search passes it as the FTS5 `LIMIT`.
const RECALL_LIMIT: usize = 10;

// `RecallCommand` / `parse_recall_command` / `handle_recall_command` were
// deleted in #2009 PR6. `/recall` retired into `/resume find`, and
// `parse_resume_command` reads both — a second parser kept "for the old verb"
// is how the two doors come to disagree about what `/recall foo bar` means.
// The RENDERERS below are untouched and still do the work; only the parse and
// the dispatch hop are gone.

/// Browse mode: the workspace's conversations, most recently active first.
///
/// "Recently active" is the §6 activity tick — `list()` returns ascending
/// tick, so the reversal here is the whole ordering story; the timestamp
/// shown per row is the display *claim* only (never an ordering key). Zero
/// cost by design: one `list()` query, no FTS work, and the title fallback
/// below only loads a record for the (abnormal) empty-title case.
fn recall_browse_message(store: &newt_core::ConversationStore) -> anyhow::Result<String> {
    let mut summaries = store.list()?;
    if summaries.is_empty() {
        return Ok("No saved conversations for this workspace.".to_string());
    }
    summaries.reverse(); // list() is least-recently-active first (§6 tick).
    let total = summaries.len();
    let mut out = String::from("Recent conversations (most recent first):");
    for summary in summaries.iter().take(RECALL_LIMIT) {
        out.push_str(&format!(
            "\n  {}  {}  ({} turns, last active ~{})",
            short_conversation_id(&summary.id),
            recall_display_title(store, &summary.id, &summary.title),
            summary.turn_count,
            claim_timestamp(summary.updated_at_unix_nanos),
        ));
    }
    if total > RECALL_LIMIT {
        out.push_str(&format!(
            "\n  … {} more — /conversation list shows all.",
            total - RECALL_LIMIT
        ));
    }
    out.push_str("\nRestore with /conversation restore <id>.");
    Ok(out)
}

/// Search mode: FTS5 snippets, best match first (bm25 — the store's order).
///
/// Each hit renders as a `short-id  title  ·  seq N` header with the snippet
/// indented under it. A query the sanitizer rejects ("reduced to nothing" —
/// all operators/punctuation) is a *user* outcome, not an error: it returns
/// a friendly hint instead of bubbling up through the `error:` path.
fn recall_search_message(
    store: &newt_core::ConversationStore,
    query: &str,
) -> anyhow::Result<String> {
    // Pre-flight the sanitizer so its rejection renders friendly; real
    // database errors from search() below still propagate as errors.
    if newt_core::sanitize_fts5_query(query).is_err() {
        return Ok(format!(
            "Nothing searchable in `{query}` — every term was FTS5 syntax or \
             punctuation. Try plain keywords, e.g. /recall tokio panic."
        ));
    }
    let hits = store.search(query, RECALL_LIMIT)?;
    if hits.is_empty() {
        return Ok(format!(
            "No matches for `{query}` in this workspace's conversations."
        ));
    }
    let mut out = format!("Recall matches for `{query}`:");
    for hit in &hits {
        out.push_str(&format!(
            "\n  {}  {}  ·  seq {}\n      {}",
            short_conversation_id(&hit.conversation_id),
            recall_display_title(store, &hit.conversation_id, &hit.title),
            hit.seq,
            readable_snippet(&hit.snippet),
        ));
    }
    out.push_str("\nRestore with /conversation restore <id>.");
    Ok(out)
}

// ── #1030 /resume — find and reopen a past conversation, listed by liveness ──

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResumeCommand {
    /// Bare `/resume` — browse recent conversations, annotated by liveness.
    Browse,
    /// `/resume <n>` — reopen the n-th row from the last listing.
    Select(usize),
    /// `/resume <token>` — an id/prefix to reopen, else an FTS5 search query.
    Query(String),
    /// `/resume find [query]` — search (or browse) and SHOW, never reopen.
    ///
    /// This is what `/recall` was, folded in (#2009 PR6). It is not the same
    /// as `Query`: a bare token that happens to resolve as an id reopens that
    /// conversation, which is the right default for "take me back to work"
    /// and the wrong one for "what do I have about auth?". Keeping the
    /// read-only half addressable is why the fold is a subcommand rather than
    /// an alias.
    Find(String),
}

fn parse_resume_command(input: &str) -> ResumeCommand {
    let body = input.trim().trim_start_matches('/').trim();
    // **The retired verb parses as its replacement.** `/recall` and
    // `/recall <query>` ARE `/resume find`, so they are read here rather than
    // kept as a second arm that could drift from it (#2009 PR6). A retired
    // READ keeps reading — the rule PR3 wrote down — and this is where it
    // keeps reading from.
    // Whole-word, carried over from the parser this replaces: `/recallx` is
    // not `/recall x`. Enforced HERE rather than only at the dispatch guard,
    // so the parser cannot be called into a wrong answer from somewhere else.
    if let Some(rest) = body.strip_prefix("recall") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return ResumeCommand::Find(rest.trim().to_string());
        }
    }
    let rest = body.strip_prefix("resume").map(str::trim).unwrap_or("");
    if rest.is_empty() {
        return ResumeCommand::Browse;
    }
    if let Some(query) = rest.strip_prefix("find") {
        // `/resume findings` is a SEARCH for "findings", not an empty find.
        if query.is_empty() || query.starts_with(char::is_whitespace) {
            return ResumeCommand::Find(query.trim().to_string());
        }
    }
    if let Ok(n) = rest.parse::<usize>() {
        // Only a plausible ROW number (1..=RECALL_LIMIT, the most rows a listing
        // shows) is a selection. A larger all-digits token is an id PREFIX — the
        // short id shown per row is the ~19-digit unix-nanos head of the id — so
        // it must fall through to Query/resolve_id, or the displayed handle would
        // be un-typeable (#1030).
        if (1..=RECALL_LIMIT).contains(&n) {
            return ResumeCommand::Select(n);
        }
    }
    ResumeCommand::Query(rest.to_string())
}

/// The liveness marker for a conversation in a `/resume` listing (#1030):
/// `▶` this session's current conversation, `●` open in ANOTHER live newt
/// (reopening it would mix turns — `/resume` refuses), `○` resumable.
fn resume_liveness_marker(
    store: &newt_core::ConversationStore,
    id: &str,
    active_id: &str,
) -> &'static str {
    if id == active_id {
        return "▶";
    }
    match store.live_owner(id) {
        Ok(Some(owner)) if store.is_owner_live(&owner) => "●",
        _ => "○",
    }
}

const RESUME_LEGEND: &str = "\n  ▶ current · ● open in another newt · ○ resumable — \
     reopen with /resume <n> or /resume <id>.";

/// Browse: the workspace's conversations, most recent first, numbered and
/// liveness-annotated. Returns the rendered text plus the ordered ids so
/// `/resume <n>` selects by the number shown.
fn resume_browse_message(
    store: &newt_core::ConversationStore,
    active_id: &str,
) -> anyhow::Result<(String, Vec<String>)> {
    let mut summaries = store.list()?;
    if summaries.is_empty() {
        return Ok((
            "No saved conversations for this workspace.".to_string(),
            Vec::new(),
        ));
    }
    summaries.reverse(); // list() is least-recently-active first (§6 tick).
    let total = summaries.len();
    let mut out = String::from("Conversations (most recent first):");
    let mut ids = Vec::new();
    for (i, s) in summaries.iter().take(RECALL_LIMIT).enumerate() {
        out.push_str(&format!(
            "\n  {:>2}. {}  {}  {}  ({} turns, ~{})",
            i + 1,
            resume_liveness_marker(store, &s.id, active_id),
            short_conversation_id(&s.id),
            recall_display_title(store, &s.id, &s.title),
            s.turn_count,
            claim_timestamp(s.updated_at_unix_nanos),
        ));
        ids.push(s.id.clone());
    }
    if total > RECALL_LIMIT {
        out.push_str(&format!(
            "\n  … {} more — refine with /resume <query>.",
            total - RECALL_LIMIT
        ));
    }
    out.push_str(RESUME_LEGEND);
    Ok((out, ids))
}

/// Search: FTS5 over this workspace's turns, numbered + liveness-annotated,
/// with match snippets. One row per conversation (its first, best hit) so a
/// `/resume <n>` maps to a conversation. Returns the text plus the ordered ids.
fn resume_search_message(
    store: &newt_core::ConversationStore,
    query: &str,
    active_id: &str,
) -> anyhow::Result<(String, Vec<String>)> {
    if newt_core::sanitize_fts5_query(query).is_err() {
        return Ok((
            format!(
                "Nothing searchable in `{query}` — every term was FTS5 syntax or \
                 punctuation. Try plain keywords, e.g. /resume tokio panic."
            ),
            Vec::new(),
        ));
    }
    let hits = store.search(query, RECALL_LIMIT)?;
    if hits.is_empty() {
        return Ok((
            format!("No matches for `{query}` in this workspace's conversations."),
            Vec::new(),
        ));
    }
    let mut out = format!("Matches for `{query}`:");
    let mut ids: Vec<String> = Vec::new();
    for hit in &hits {
        if ids.iter().any(|existing| existing == &hit.conversation_id) {
            continue; // a conversation can match on several turns — show it once
        }
        out.push_str(&format!(
            "\n  {:>2}. {}  {}  {}\n        {}",
            ids.len() + 1,
            resume_liveness_marker(store, &hit.conversation_id, active_id),
            short_conversation_id(&hit.conversation_id),
            recall_display_title(store, &hit.conversation_id, &hit.title),
            readable_snippet(&hit.snippet),
        ));
        ids.push(hit.conversation_id.clone());
    }
    out.push_str(RESUME_LEGEND);
    Ok((out, ids))
}

/// Ambiguous title match — the "candidate selection when ambiguous" step of
/// the shared resolver (#1030/#1671): render the candidates as a numbered,
/// liveness-annotated listing so a follow-up `/resume <n>` picks one, instead
/// of a bare error. Mirrors `resume_browse_message` / `resume_search_message`.
fn resume_ambiguous_message(
    store: &newt_core::ConversationStore,
    query: &str,
    candidates: &[(String, String)],
    active_id: &str,
) -> anyhow::Result<(String, Vec<String>)> {
    let mut out = format!(
        "\"{query}\" matches {} conversations — pick one:",
        candidates.len()
    );
    let mut ids = Vec::new();
    for (i, (id, title)) in candidates.iter().enumerate() {
        out.push_str(&format!(
            "\n  {:>2}. {}  {}  {}",
            i + 1,
            resume_liveness_marker(store, id, active_id),
            short_conversation_id(id),
            recall_display_title(store, id, title),
        ));
        ids.push(id.clone());
    }
    out.push_str(RESUME_LEGEND);
    Ok((out, ids))
}

/// Make a raw FTS5 snippet readable in the TUI: collapse internal whitespace
/// (turn text is multi-line; each snippet must stay on its own row) and
/// replace the store's `>>>`/`<<<` match markers with `«`/`»`, which read as
/// highlights in plain text and survive no-color terminals. Cosmetic edge,
/// accepted: literal `>>>`/`<<<` inside the user's own content is rewritten
/// too — FTS5 marks matches with those exact strings, so they can't be told
/// apart after the fact.
fn readable_snippet(snippet: &str) -> String {
    snippet
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(">>>", "«")
        .replace("<<<", "»")
}

/// First 12 characters of a conversation id (`{unix_nanos}-{uuid}`), enough
/// nanos digits for 10ms granularity — distinct for any two conversations
/// created more than 10ms apart. `resolve_id` accepts any unique prefix, so
/// the short id pastes straight into `/conversation restore`; in the rare
/// ambiguous case, restore lists the full matching ids.
fn short_conversation_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// Render a wall-clock display claim (§6: a *claim*, never an ordering key —
/// hence the `~`-prefix at the call sites). UTC, minute precision.
fn claim_timestamp(unix_nanos: u128) -> String {
    i64::try_from(unix_nanos / 1_000_000_000)
        .ok()
        .and_then(|secs| chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0))
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Render-time title fallback (17.4): conversations created through the TUI
/// always get a heuristic title (`conversation_title_from_task`), but a
/// record created elsewhere can carry an empty one. Fall back to the first
/// user turn's first 60 chars (whitespace-flattened), else `(untitled)` —
/// display only, no schema or stored-title change. The extra `load()` runs
/// only for the empty-title case, so the browse path stays zero-cost.
fn recall_display_title(store: &newt_core::ConversationStore, id: &str, title: &str) -> String {
    let trimmed = title.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    if let Ok(record) = store.load(id) {
        if let Some(turn) = record.turns.first() {
            let fallback: String = turn
                .user
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(60)
                .collect();
            if !fallback.is_empty() {
                return fallback;
            }
        }
    }
    "(untitled)".to_string()
}

// ---------------------------------------------------------------------------
// /compress — manual context compression (Step 18.6, issue #247)
// ---------------------------------------------------------------------------

/// Parse `/compress [focus]`: `Ok(None)` for the bare command, `Ok(Some)`
/// with the free-text focus topic otherwise. `/compressx` is some other
/// (unknown) command, not `/compress x` — the `/recall` parsing contract.
/// The focus is an opaque string here; redaction happens in the pipeline
/// (a user can type a secret into the focus).
fn parse_compress_command(input: &str) -> anyhow::Result<Option<String>> {
    let body = input.trim().trim_start_matches('/').trim();
    // `/compact` is the Claude-Code-parity alias for `/compress`.
    let Some(rest) = body
        .strip_prefix("compress")
        .or_else(|| body.strip_prefix("compact"))
    else {
        anyhow::bail!("not a compress command");
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        anyhow::bail!("not a compress command");
    }
    let focus = rest.trim();
    Ok((!focus.is_empty()).then(|| focus.to_string()))
}

/// The wire view of the current session history — exactly what the loop
/// would send (system prompt + provider history), minus the trailing task
/// slot `build_messages` appends: `/compress` is maintenance, not a turn.
fn session_wire_view(memory: &newt_core::MemoryManager, system: &str) -> Vec<serde_json::Value> {
    let mut wire: Vec<serde_json::Value> = memory
        .build_messages(system, "")
        .iter()
        .map(|m| serde_json::json!({"role": m.role.as_str(), "content": m.content}))
        .collect();
    if wire
        .last()
        .is_some_and(|m| m["role"] == "user" && m["content"] == "")
    {
        wire.pop();
    }
    wire
}

/// Rebuild provider-shaped turns from the pipeline's assembled wire messages
/// so the compressed working set can be applied back through the existing
/// in-memory replace seam (`MemoryManager::restore_turns` — the same one
/// `/conversation restore` uses). A compaction message (and any unpaired
/// side) becomes a lone-sided turn; the system message is dropped — the TUI
/// owns the system prompt and providers re-add it at build time. Token
/// columns stay `None`: after compression these turns are no longer
/// backend-measured (an estimate is never presented as a measurement).
fn wire_messages_to_turns(messages: &[serde_json::Value]) -> Vec<newt_core::ConversationTurn> {
    let mut out: Vec<newt_core::ConversationTurn> = Vec::new();
    for m in messages {
        let content = m["content"].as_str().unwrap_or_default();
        match m["role"].as_str() {
            Some("user") => out.push(newt_core::ConversationTurn::new(content, "")),
            Some("assistant") => match out.last_mut() {
                Some(last)
                    if last.assistant.is_empty()
                        && !last.user.is_empty()
                        && !last.user.starts_with(newt_core::agentic::SUMMARY_PREFIX) =>
                {
                    last.assistant = content.to_string();
                }
                _ => out.push(newt_core::ConversationTurn::new("", content)),
            },
            // `system` is the TUI's own prompt; `tool` roles cannot occur in
            // a provider wire view (text pairs only) — skip defensively.
            _ => {}
        }
    }
    out
}

/// The `/compress` honesty feedback (18.6): never claim savings that didn't
/// happen. Unchanged pipeline output reports "no compression possible"; a
/// fired run reports the REAL before → after counts and how (LLM summary vs
/// static marker vs structural prune), with hermes's "fewer messages can
/// still raise the estimate" note when token savings didn't materialize.
fn compress_feedback_message(outcome: &newt_core::ManualCompressOutcome) -> String {
    if !outcome.fired {
        return format!(
            "no compression possible — {} message(s), ~{} est. tokens are already \
             protected head/tail or unprunable",
            outcome.messages_before, outcome.tokens_before
        );
    }
    let mut msg = format!(
        "context compressed: {} → {} messages, ~{} → ~{} est. tokens ({})",
        outcome.messages_before,
        outcome.messages_after,
        outcome.tokens_before,
        outcome.tokens_after,
        outcome.how
    );
    if outcome.tokens_after >= outcome.tokens_before {
        msg.push_str(
            "\nnote: no token savings — fewer messages can still raise the \
             estimate (the transcript was rewritten into denser summaries)",
        );
    }
    msg
}

/// The `/memory` anti-thrash section (18.6): read-only surfacing of the
/// session [`newt_core::CompressCounters`]. The "/new resets it" hint shows
/// only on the latched line — and it IS true: `handle_new_conversation`
/// calls `CompressState::reset` (F4, #267).
fn memory_compress_section(c: &newt_core::CompressCounters) -> String {
    let mut line = format!("  compressions this session: {}", c.compressions);
    if let Some(reclaim) = c.last_reclaim {
        // A negative reclaim (the pass GREW the estimate) renders as what it
        // is — clamping to "0%" would claim savings that didn't happen.
        if reclaim >= 0.0 {
            line.push_str(&format!(" (last reclaimed {:.0}%)", reclaim * 100.0));
        } else {
            line.push_str(&format!(
                " (last pass grew the estimate {:.0}%)",
                -reclaim * 100.0
            ));
        }
    }
    let strikes = format!(
        "  ineffective-pass strikes: {}/2 (two latch the disable)",
        c.strikes
    );
    let status = if c.disabled {
        "  auto-compression: disabled — anti-thrash latched; /new resets it"
    } else {
        "  auto-compression: enabled"
    };
    format!("{line}\n{strikes}\n{status}")
}

/// The user-facing line for [`newt_core::ConversationStore::wal_fallback_notice`]
/// (N7, #261 review): `None` when WAL is healthy, `Some(message)` when the
/// store fell back to `journal_mode=DELETE` (typical for `~/.newt` on NFS).
fn wal_fallback_startup_notice(notice: Option<&str>) -> Option<String> {
    notice.map(|cause| {
        format!(
            "conversation store: SQLite WAL unavailable, using the journal_mode=DELETE \
             fallback (typical for NFS homes; concurrent newts may wait on locks). \
             Cause: {cause}"
        )
    })
}

fn handle_persona_command(
    input: &str,
    workspace: &str,
    store: &PersonaStore,
    active_persona: &mut Option<Persona>,
    ctx: &mut ConversationResetContext<'_>,
) -> anyhow::Result<String> {
    match parse_persona_command(input)? {
        PersonaCommand::List => persona_list(store),
        PersonaCommand::Show => Ok(persona_status(active_persona.as_ref())),
        PersonaCommand::Clear => {
            *active_persona = None;
            // P1#3: no persona → no persona-declared tenacity / cognition layer.
            newt_core::tenacity::set_persona_tenacity(None);
            newt_core::cognition::set_persona_cognition(None);
            // Clearing the persona starts a new conversation → fresh id + plan.
            *ctx.conversation_id = newt_core::new_conversation_id();
            reset_conversation(workspace, active_persona.as_ref(), ctx);
            Ok("Started a new conversation with no active persona.".to_string())
        }
        PersonaCommand::Set { name, keep_context } => {
            let persona = store.load(&name)?;
            // P1#3 / review-2: install the persona's declared tenacity + cognition
            // as real resolution layers, so `/persona show`, `/psyche`, and the
            // panel all agree and the loop obeys them.
            newt_core::tenacity::set_persona_tenacity(persona.profile.tenacity);
            newt_core::cognition::set_persona_cognition(persona.profile.cognition);
            *active_persona = Some(persona);
            if keep_context {
                // Persistent-actor swap: rebuild the system prompt for the new
                // role WITHOUT discarding the conversation history — same
                // conversation, same plan file.
                *ctx.system = rebuild_system_prompt(
                    workspace,
                    ctx.memory,
                    active_persona.as_ref(),
                    ctx.conversation_id,
                );
                Ok(persona_swap_kept_context_message(active_persona.as_ref()))
            } else {
                // Swapping without keeping context starts a new conversation.
                *ctx.conversation_id = newt_core::new_conversation_id();
                reset_conversation(workspace, active_persona.as_ref(), ctx);
                Ok(new_conversation_message(active_persona.as_ref()))
            }
        }
    }
}

fn persona_swap_kept_context_message(active_persona: Option<&Persona>) -> String {
    match active_persona {
        Some(persona) => format!(
            "Switched to persona `{}` (kept conversation context).",
            persona.name
        ),
        None => "Switched persona (kept conversation context).".to_string(),
    }
}

/// Resolve the token-based mid-loop trim trigger (issue #223): the per-model
/// override wins over the global `[tui].mid_loop_trim_tokens`. A configured
/// ZERO (from either source) means DISABLED — the old `trim_to_token_budget`
/// zero-is-noop contract, enforced both here and in the trigger itself (F3).
fn effective_mid_loop_trim_tokens(
    model_override: Option<usize>,
    global: Option<usize>,
) -> Option<usize> {
    model_override.or(global).filter(|&t| t > 0)
}

/// Read the legacy pre-execution preview limit (default 20, 0 = unlimited).
/// Completed results use `[tui] spill_lines`.
fn tool_output_lines(cfg: &newt_core::Config) -> usize {
    cfg.tui.as_ref().map(|t| t.tool_output_lines).unwrap_or(20)
}

/// #1235: the spill-view height (`[tui] spill_lines`, default 3 — keep in
/// sync with `default_spill_lines` in newt-core config).
fn spill_lines(cfg: &newt_core::Config) -> usize {
    cfg.tui.as_ref().map(|t| t.spill_lines).unwrap_or(3)
}

/// Seconds between committed `[HH:MM]` transcript markers — mirrors
/// [`spill_lines`], including its "config absent means the documented default"
/// shape.
fn time_marker_secs(cfg: &newt_core::Config) -> u64 {
    cfg.tui.as_ref().map(|t| t.time_marker_secs).unwrap_or(300)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpillCommand {
    Status,
    Set(usize),
    Reset,
    /// #1640 Layer 1: collapse spilled tool results to a one-line marker.
    Summary,
    /// #1640 Layer 1: restore the multi-row excerpt.
    Excerpt,
    /// Open the newest retained completed result.
    Last,
    /// Open one stable, session-local retained result.
    Open(u64),
}

/// Apply a parsed `/spill` command to the two session overrides. Pure and
/// separately pinned (review fix on #1663): `Reset` must clear BOTH knobs —
/// the row override AND the summary/excerpt mode — returning the session to
/// its surface defaults; nothing else touches the knob it doesn't own.
fn apply_spill_command(
    cmd: SpillCommand,
    lines_override: &mut Option<usize>,
    summary_override: &mut Option<bool>,
) {
    match cmd {
        SpillCommand::Status => {}
        SpillCommand::Set(rows) => *lines_override = Some(rows),
        SpillCommand::Summary => *summary_override = Some(true),
        SpillCommand::Excerpt => *summary_override = Some(false),
        SpillCommand::Last | SpillCommand::Open(_) => {}
        SpillCommand::Reset => {
            *lines_override = None;
            *summary_override = None;
        }
    }
}

fn parse_spill_command(input: &str) -> anyhow::Result<SpillCommand> {
    let body = input.trim().trim_start_matches('/').trim();
    let Some(rest) = body.strip_prefix("spill") else {
        anyhow::bail!("not a spill command");
    };
    if !rest.is_empty()
        && !rest
            .chars()
            .next()
            .map(char::is_whitespace)
            .unwrap_or(false)
    {
        anyhow::bail!("not a spill command");
    }

    let arg = rest.trim();
    let normalized = arg.to_ascii_lowercase();
    if let Some(value) = normalized.strip_prefix("open ") {
        let id = value
            .trim()
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("/spill open needs a positive result id"))?;
        if id == 0 || value.split_whitespace().count() != 1 {
            anyhow::bail!("/spill open needs one positive result id");
        }
        return Ok(SpillCommand::Open(id));
    }
    match normalized.as_str() {
        "" | "show" | "status" => Ok(SpillCommand::Status),
        "reset" | "default" | "config" | "auto" => Ok(SpillCommand::Reset),
        "summary" | "collapse" => Ok(SpillCommand::Summary),
        "excerpt" | "expand" | "rows" => Ok(SpillCommand::Excerpt),
        "last" => Ok(SpillCommand::Last),
        _ => arg
            .parse::<usize>()
            .map(SpillCommand::Set)
            .map_err(|_| anyhow::anyhow!("unknown /spill argument '{arg}'")),
    }
}

/// #1387 Phase 1 — `/search` cockpit verbs (pure parse; chat owns execution).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SearchCommand {
    /// Run a semantic query (the remainder of the line).
    Query(String),
    Preview(usize),
    Model,
    Rejects,
    Pin(usize),
    Exclude(usize),
    Status,
    Clear,
    Help,
}

fn parse_search_command(input: &str) -> anyhow::Result<SearchCommand> {
    let body = input.trim().trim_start_matches('/').trim();
    let Some(rest) = body.strip_prefix("search") else {
        anyhow::bail!("not a search command");
    };
    if !rest.is_empty()
        && !rest
            .chars()
            .next()
            .map(char::is_whitespace)
            .unwrap_or(false)
    {
        anyhow::bail!("not a search command");
    }
    let arg = rest.trim();
    if arg.is_empty() || matches!(arg, "help" | "--help" | "-h") {
        return Ok(SearchCommand::Help);
    }
    let (verb, tail) = match arg.split_once(char::is_whitespace) {
        Some((v, t)) => (v, t.trim()),
        None => (arg, ""),
    };
    match verb {
        "preview" => Ok(SearchCommand::Preview(if tail.is_empty() {
            1
        } else {
            tail.parse()
                .map_err(|_| anyhow::anyhow!("usage: /search preview [N]"))?
        })),
        "model" | "packet" => Ok(SearchCommand::Model),
        "rejects" | "reject" | "ledger" => Ok(SearchCommand::Rejects),
        "pin" => {
            let n: usize = tail
                .parse()
                .map_err(|_| anyhow::anyhow!("usage: /search pin <N>"))?;
            Ok(SearchCommand::Pin(n))
        }
        "exclude" | "x" => {
            let n: usize = tail
                .parse()
                .map_err(|_| anyhow::anyhow!("usage: /search exclude <N>"))?;
            Ok(SearchCommand::Exclude(n))
        }
        "status" => Ok(SearchCommand::Status),
        "clear" => Ok(SearchCommand::Clear),
        _ => Ok(SearchCommand::Query(arg.to_string())),
    }
}

fn search_help_text() -> &'static str {
    "/search <query>     — semantic search (shared with model code_search)\n\
     /search preview [N] — preview hit N (default 1)\n\
     /search model       — exact <code_evidence> packet the model would see\n\
     /search rejects     — reject ledger (below top_k / budget / excluded)\n\
     /search pin N       — pin hit N into the next inject + tool retrieve\n\
     /search exclude N   — exclude hit N's path from automatic retrieval\n\
     /search status      — index generation, completeness, git HEAD/dirty\n\
     /search clear       — clear session pins/exclusions"
}

/// Best-effort HEAD + dirty bit for lightweight index status (#1387). Runtime
/// glue (not unit-tier) — absence is reported honestly as unavailable.
fn lightweight_git_meta(workspace: &str) -> (Option<String>, Option<bool>) {
    // Confused-deputy-safe (step-7.4): the workspace may be a hostile repo.
    let head = newt_core::git_hardening::hardened_git(
        std::path::Path::new(workspace),
        &["rev-parse", "HEAD"],
    )
    .output()
    .ok()
    .filter(|o| o.status.success())
    .and_then(|o| String::from_utf8(o.stdout).ok())
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());
    let dirty = newt_core::git_hardening::hardened_git(
        std::path::Path::new(workspace),
        &["status", "--porcelain"],
    )
    .output()
    .ok()
    .filter(|o| o.status.success())
    .map(|o| !o.stdout.is_empty());
    (head, dirty)
}

fn effective_spill_lines(configured: usize, session_override: Option<usize>) -> usize {
    session_override.unwrap_or(configured)
}

/// #1640: how many rows the *committed* tool-output excerpt keeps for THIS
/// surface. The excerpt is only worth truncating when there is an interactive
/// viewport to recover the hidden lines — i.e. the RICH surface. The LEAN
/// surface (`--plain` / `-n` / piped / headless / a non-`rich-tui` build) has
/// no scrollable regions at all (decision: the plain scroller), so its excerpt
/// IS the whole durable record and must be shown in full. Collapsing it there
/// only hides output behind a `/spill N` re-render the lean surface cannot even
/// scroll — the exact regression #1640 reports.
///
/// So: pass the configured height through on rich; force `0` (unbounded) on
/// lean. `0` is `spill_view_lines`' existing "print every line" contract, so no
/// new code path is introduced — this only chooses the argument.
fn committed_spill_lines(surface_is_rich: bool, configured: usize) -> usize {
    if surface_is_rich {
        configured
    } else {
        0
    }
}

/// #1640 Layer 1: whether committed tool results collapse to a one-line
/// summary marker on THIS surface. The default is the surface itself — rich
/// collapses (its viewport + `/spill` vocabulary can recover the detail), lean
/// never does (it shows FULL output, the same reasoning as
/// [`committed_spill_lines`]) — and `/spill summary` / `/spill excerpt` set the
/// session override on the same single-knob pattern as `spill_lines_override`.
fn effective_spill_summary(surface_is_rich: bool, session_override: Option<bool>) -> bool {
    session_override.unwrap_or(surface_is_rich)
}

/// #1434: `--trace` SEEDS the session detail knob instead of sitting beside it.
///
/// newt already has a session-wide detail level — `SPILL_LINES`, config-seeded
/// per turn and runtime-mutable via `/spill N`, with `0` meaning unbounded. What
/// it lacked was a connection to the launch flag, which is exactly the bug pi
/// shipped: `getStartupExpansionState()` is `options.verbose || toolOutputExpanded`
/// and nothing initializes the latter from the former, so **after `pi --verbose`
/// the first toggle press expands instead of collapsing**.
///
/// A launch flag that sits *beside* a runtime toggle has two sources of truth
/// and therefore a phase. Seeding gives one variable and no phase.
fn initial_spill_override(trace: bool) -> Option<usize> {
    // `--trace` means "show me everything"; 0 is this knob's unbounded.
    trace.then_some(0)
}

/// #1434: flip the session detail level between unbounded and the configured
/// height. The single action behind `/detail` (and, once #294's action table
/// exists, behind a chord).
///
/// Returns the new override. Deliberately expressed over the SAME
/// `Option<usize>` the `/spill` command already mutates, so there is no second
/// piece of detail state to drift — see `initial_spill_override`.
fn toggle_spill_detail(current: Option<usize>, configured: usize) -> Option<usize> {
    if effective_spill_lines(configured, current) == 0 {
        // Unbounded → back to the configured height. `max(1)` so a configured
        // 0 cannot make the toggle a no-op that looks broken.
        Some(configured.max(1))
    } else {
        Some(0)
    }
}

/// Why the live spill viewport is, or is not, available right now (#1412).
///
/// The predicate used to be a bare `bool`, so `/spill` could say only
/// "unavailable" — it could not distinguish "stdout is not a TTY" from
/// "`TERM=dumb`" from "the feature was not compiled in". That silence has a
/// measured cost: it is why a **stale install** was reported as a vanished
/// feature. The binary on the reporting machine predated live-spill entirely
/// and contained no `/spill` command at all, but nothing the product could say
/// would have distinguished that from a misconfiguration.
///
/// A refusing gate should name itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpillEligibility {
    /// Every gate passed; the live viewport can render.
    Available,
    /// Not a Unix-family platform (the viewport needs POSIX terminal control).
    UnsupportedPlatform,
    /// Built without the `live-spill` cargo feature — the lean/wyvern tier.
    FeatureDisabled,
    /// stdin is a pipe or file, so no keystrokes can drive the viewport.
    StdinNotTty,
    /// stdout is redirected, so there is no frame to draw into.
    StdoutNotTty,
    /// `TERM=dumb` — the terminal disclaims cursor addressing.
    TermDumb,
    /// The cockpit owns the terminal, so the viewport is deliberately not
    /// built: it paints with cursor motion the presenter drops by design.
    ///
    /// Last in the ladder because it is the one refusal that says nothing is
    /// WRONG — the platform, the build and the terminal are all fine, and the
    /// operator is simply on the surface that does its own drawing. Reporting
    /// it after the others keeps the fixable causes first.
    TerminalOwnedByCockpit,
}

impl SpillEligibility {
    /// Operator-facing phrase for `/spill`. Names the gate AND, where there is
    /// one, the lever that changes it — an unavailability the operator cannot
    /// act on is only marginally better than a bare "unavailable".
    fn explain(self) -> &'static str {
        match self {
            Self::Available => "live interaction available",
            Self::UnsupportedPlatform => {
                "live interaction unavailable: unsupported platform (needs a POSIX terminal)"
            }
            Self::FeatureDisabled => {
                "live interaction unavailable: built without the `live-spill` feature \
                 (this is the lean/wyvern build)"
            }
            Self::StdinNotTty => {
                "live interaction unavailable: stdin is not a terminal (piped or redirected)"
            }
            Self::StdoutNotTty => {
                "live interaction unavailable: stdout is not a terminal (piped or redirected)"
            }
            Self::TermDumb => "live interaction unavailable: TERM=dumb disclaims cursor control",
            Self::TerminalOwnedByCockpit => {
                "live interaction unavailable: the cockpit owns the terminal \
                 (committed results stay full; /spill open <id> reopens one)"
            }
        }
    }
}

fn spill_status(
    configured: usize,
    session_override: Option<usize>,
    summary: bool,
    surface_is_rich: bool,
    eligibility: SpillEligibility,
) -> String {
    let effective = effective_spill_lines(configured, session_override);
    // Review fix (#1663): status must describe what rendering will DO, not
    // just echo the knobs — on lean the committed record is forced full, and
    // collapse cannot engage when the committed view is unbounded (lean, or
    // rich after `/spill 0`), so the mode clause is guarded the same way the
    // renderer is.
    let committed = committed_spill_lines(surface_is_rich, effective);
    let rows = if effective == 0 {
        "unbounded".to_string()
    } else {
        effective.to_string()
    };
    let source = match session_override {
        Some(_) => format!(" this session (config default {configured}"),
        None => " (config default".to_string(),
    };
    // Row count is checked here rather than inside `SpillEligibility` on
    // purpose: zero rows disables the *viewport*, but the terminal is still
    // perfectly capable — and the mouse tier keys off capability, not rows.
    // Folding the two would make `/spill 0` look like a broken terminal.
    let live = if effective == 0 {
        "live viewport disabled: spill_lines is 0 (/spill <n> raises it)"
    } else {
        eligibility.explain()
    };
    let lean_note = if !surface_is_rich && effective != 0 {
        "; committed results always print in full on this surface"
    } else {
        ""
    };
    // #1640 Layer 1: name the committed-result mode so `/spill status` answers
    // "why is my tool output one line" (or "why is it five").
    let mode = if summary && committed != 0 {
        "; results collapse to a summary line (/spill excerpt restores rows)"
    } else {
        ""
    };
    format!("spill rows: {rows}{source}; {live}{lean_note}{mode})")
}

/// Whether the committed-collapse default can honestly engage this session:
/// the rich surface with the completed-spill viewport buildable.
///
/// The old comment claimed sharing this with `/spill status` kept the status
/// line and the turn-head seeding from drifting, and treated "the renderer
/// additionally has to construct" as a harmless remainder. It was not
/// harmless. Under the cockpit — the DEFAULT rich surface — `run_chat`
/// declines to build the renderer at all, so the turn seeded collapse OFF and
/// committed excerpts, while this said the collapse was engaged. `/spill`
/// answered "results collapse to a summary line" and the very next tool result
/// printed a five-row excerpt.
///
/// `terminal_owns_turn` is now part of the eligibility ladder, so both answers
/// come from one walk of it and the remainder is only the genuine
/// constructor-failed case.
fn summary_recovery_available(surface_is_rich: bool, terminal_owns_turn: bool) -> bool {
    // `live_spill_capable` itself only exists under live-spill, so the whole
    // conjunction is cfg-split rather than short-circuited with `cfg!`.
    #[cfg(all(feature = "rich-tui", feature = "live-spill"))]
    {
        surface_is_rich && live_spill_capable(terminal_owns_turn)
    }
    #[cfg(not(all(feature = "rich-tui", feature = "live-spill")))]
    {
        let _ = surface_is_rich;
        let _ = terminal_owns_turn;
        false
    }
}

fn live_spill_eligibility(terminal_owns_turn: bool) -> SpillEligibility {
    let term = std::env::var("TERM").ok();
    spill_eligibility_for(
        cfg!(unix),
        cfg!(feature = "live-spill"),
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        term.as_deref(),
        terminal_owns_turn,
    )
}

/// Yes/no form for the turn-level gate in `chat.rs`, which is itself
/// `#[cfg(feature = "live-spill")]` — hence the matching gate here. Before
/// #1412 this stayed alive in the lean build only because `/spill status`
/// called it; that call now wants the *reason*, so the lean build would
/// otherwise carry it as dead code.
///
/// Not `test`-gated: it probes the real stdin/stdout, so the unit tier drives
/// the injected-bool [`live_spill_capable_for`] instead.
#[cfg(feature = "live-spill")]
fn live_spill_capable(terminal_owns_turn: bool) -> bool {
    live_spill_eligibility(terminal_owns_turn) == SpillEligibility::Available
}

/// Same five gates the old boolean ANDed, but reporting *which* one refused.
///
/// Precedence is deliberate and tested: the gates the operator cannot change
/// without a different binary come first (platform, then cargo feature), then
/// the ones a different invocation fixes (stdin, stdout), then the terminal's
/// own declaration. Reporting the most fundamental refusal first stops
/// `/spill` from sending someone to fix their `TERM` when the feature was
/// never compiled in.
fn spill_eligibility_for(
    platform_supported: bool,
    feature_enabled: bool,
    stdin_terminal: bool,
    stdout_terminal: bool,
    term: Option<&str>,
    terminal_owns_turn: bool,
) -> SpillEligibility {
    if !platform_supported {
        SpillEligibility::UnsupportedPlatform
    } else if !feature_enabled {
        SpillEligibility::FeatureDisabled
    } else if !stdin_terminal {
        SpillEligibility::StdinNotTty
    } else if !stdout_terminal {
        SpillEligibility::StdoutNotTty
    } else if term == Some("dumb") {
        SpillEligibility::TermDumb
    } else if terminal_owns_turn {
        SpillEligibility::TerminalOwnedByCockpit
    } else {
        SpillEligibility::Available
    }
}

/// Boolean façade over [`spill_eligibility_for`], kept because the mouse tier
/// and its acceptance tests ask a yes/no question and should not have to care
/// about the reason. Gated to match its only non-test caller,
/// `mouse_capable_for`, which is `#[cfg(feature = "live-spill")]`.
#[cfg(any(feature = "live-spill", test))]
fn live_spill_capable_for(
    platform_supported: bool,
    feature_enabled: bool,
    stdin_terminal: bool,
    stdout_terminal: bool,
    term: Option<&str>,
    terminal_owns_turn: bool,
) -> bool {
    spill_eligibility_for(
        platform_supported,
        feature_enabled,
        stdin_terminal,
        stdout_terminal,
        term,
        terminal_owns_turn,
    ) == SpillEligibility::Available
}

/// #1303: the mouse-tier opt-in — config-first (`[tui] mouse_viewport`, default
/// false) with a `NEWT_MOUSE` env override (the `NEWT_*` convention, and the
/// seam the acceptance test uses to force the opt-in on while proving the TTY
/// gate still refuses). Mirrors [`spill_lines`]; the config field is always
/// compiled, but this accessor and the capability gate only exist under
/// `live-spill` (the mouse tier rides the spill viewport's own feature, stripped
/// from the wyvern build).
#[cfg(feature = "live-spill")]
fn mouse_viewport(cfg: &newt_core::Config) -> bool {
    if let Ok(v) = std::env::var("NEWT_MOUSE") {
        return matches!(v.as_str(), "1" | "true" | "on" | "yes");
    }
    cfg.tui.as_ref().map(|t| t.mouse_viewport).unwrap_or(false)
}

/// #1303: the mouse-tier capability gate — layered strictly ON TOP of
/// `live_spill_capable()` plus an explicit opt-in. Mouse events arrive on
/// **stdin**, so both stdin and stdout must be terminals (a stdin-piped session
/// never enables capture even when stdout is a TTY).
#[cfg(feature = "live-spill")]
fn mouse_capable(opt_in: bool) -> bool {
    let term = std::env::var("TERM").ok();
    mouse_capable_for(
        cfg!(unix),
        cfg!(feature = "live-spill"),
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        term.as_deref(),
        opt_in,
    )
}

/// Pure predicate behind [`mouse_capable`]: the opt-in AND the full
/// `live_spill_capable_for` predicate. Injected-bool seam for the acceptance-1a
/// table test.
#[cfg(feature = "live-spill")]
fn mouse_capable_for(
    platform_supported: bool,
    feature_enabled: bool,
    stdin_terminal: bool,
    stdout_terminal: bool,
    term: Option<&str>,
    opt_in: bool,
) -> bool {
    opt_in
        && live_spill_capable_for(
            platform_supported,
            feature_enabled,
            stdin_terminal,
            stdout_terminal,
            term,
            // NOT the cockpit gate. That refusal says "this surface draws its
            // own frames", which is a fact about the VIEWPORT; the mouse tier
            // is asking whether the TERMINAL can report clicks, and the
            // cockpit does not take that away. Feeding it in here would
            // silently disable the mouse tier on the default rich surface,
            // which is a behaviour change and not this predicate's business.
            false,
        )
}

/// Maximum tool-call rounds per turn, from `[tui].max_tool_rounds`.
/// Uses the canonical core default when there's no `[tui]` table or config file.
fn max_tool_rounds(cfg: &newt_core::Config) -> usize {
    cfg.tui
        .as_ref()
        .map(|t| t.max_tool_rounds)
        .unwrap_or_else(|| newt_core::TuiConfig::default().max_tool_rounds)
}

/// Additional progress-aware tool-call rounds after `[tui].max_tool_rounds`.
/// Defaults to 5; set to 0 to keep the normal round cap hard.
fn workflow_grace_rounds(cfg: &newt_core::Config) -> usize {
    cfg.tui
        .as_ref()
        .map(|t| t.workflow_grace_rounds)
        .unwrap_or(5)
}

/// Narrate-then-stop rescue budget per turn (`[tui].narration_nudge_cap`).
/// Defaults to 1 — the historical one-shot rescue; weak local models that
/// chronically narrate instead of acting benefit from 2-3 (lever L3).
fn narration_nudge_cap(cfg: &newt_core::Config) -> usize {
    cfg.tui.as_ref().map(|t| t.narration_nudge_cap).unwrap_or(1)
}

const EFFECTIVELY_UNLIMITED_TOOL_ROUNDS: usize = newt_core::tenacity::RELENTLESS_TOOL_ROUND_TARGET;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolRoundLimitCommand {
    Show,
    Set(usize),
    Double,
    Reset,
    Configured,
    Unlimited,
}

/// The verb the operator typed and the argument after it.
///
/// The NAME is returned, not just the argument (#1998): `/rounds 320` and
/// `/max-rounds 320` are the same setting reached two ways, and which way is
/// the half of the event a reader cannot reconstruct afterwards — so it is
/// bound into the receipt's address. This shape also hid two aliases from the
/// help text for a long while; returning the name is what lets the receipt
/// name them.
fn tool_round_limit_command(input: &str) -> Option<(&'static str, &str)> {
    let body = input.trim().trim_start_matches('/').trim();
    ["rounds", "tool-rounds", "max-rounds"]
        .iter()
        .find_map(|cmd| {
            let rest = body.strip_prefix(*cmd)?;
            let boundary = rest.is_empty()
                || rest
                    .chars()
                    .next()
                    .map(char::is_whitespace)
                    .unwrap_or(false);
            boundary.then(|| (*cmd, rest.trim()))
        })
}

fn tool_round_limit_command_arg(input: &str) -> Option<&str> {
    tool_round_limit_command(input).map(|(_, arg)| arg)
}

/// Parse `/rounds [show|<n>|double|reset|config|unlimited]`, the
/// human-controlled session override for the agentic loop's tool-call round
/// safety valve. `reset`/`default`/`auto` return to the derived posture default;
/// `config` deliberately chooses the configured/model-tuned number even when
/// explicit Relentless tenacity would otherwise raise it.
fn parse_tool_round_limit_command(input: &str) -> anyhow::Result<ToolRoundLimitCommand> {
    let Some(arg) = tool_round_limit_command_arg(input) else {
        anyhow::bail!("not a tool-round limit command");
    };
    if arg.is_empty() {
        return Ok(ToolRoundLimitCommand::Show);
    }

    let normalized = arg.to_ascii_lowercase();
    match normalized.as_str() {
        "show" | "status" => Ok(ToolRoundLimitCommand::Show),
        "double" | "x2" | "2x" => Ok(ToolRoundLimitCommand::Double),
        "reset" | "default" | "auto" => Ok(ToolRoundLimitCommand::Reset),
        "config" | "configured" => Ok(ToolRoundLimitCommand::Configured),
        "unlimited" | "infinite" | "finish" | "until-finished" | "run-until-finished"
        | "until finished" | "run until finished" => Ok(ToolRoundLimitCommand::Unlimited),
        _ => {
            let n = arg.parse::<usize>().map_err(|_| {
                anyhow::anyhow!(
                    "unknown /rounds argument '{arg}' (use show, <n>, double, reset, config, unlimited)"
                )
            })?;
            if n == 0 {
                anyhow::bail!("tool-call round limit must be at least 1");
            }
            if n > EFFECTIVELY_UNLIMITED_TOOL_ROUNDS {
                anyhow::bail!(
                    "tool-call round limit must be <= {EFFECTIVELY_UNLIMITED_TOOL_ROUNDS}"
                );
            }
            Ok(ToolRoundLimitCommand::Set(n))
        }
    }
}

fn tenacity_tool_round_limit(
    configured: usize,
    explicit_tenacity: Option<newt_core::Tenacity>,
) -> usize {
    newt_core::tenacity::resolve_tool_round_limit(configured, explicit_tenacity, None).rounds
}

/// The cap this turn will run under, WITH its derivation (#1965).
///
/// Returns the whole [`ToolRoundLimit`](newt_core::tenacity::ToolRoundLimit)
/// rather than the number, because the number alone is what made an escalation
/// from 40 to effectively unlimited unrecordable: the caller stamps the
/// derivation into the turn's durable outcome, and cannot do that from a
/// `usize`.
fn effective_tool_round_limit(
    configured: usize,
    explicit_tenacity: Option<newt_core::Tenacity>,
    session_override: Option<usize>,
) -> newt_core::tenacity::ToolRoundLimit {
    newt_core::tenacity::resolve_tool_round_limit(configured, explicit_tenacity, session_override)
}

fn double_tool_round_limit(current: usize) -> usize {
    if current >= EFFECTIVELY_UNLIMITED_TOOL_ROUNDS {
        // The sentinel is an "effectively unlimited" target, not a maximum
        // valid configuration. Never make `/rounds double` lower a larger
        // config/model value.
        current
    } else {
        current
            .saturating_mul(2)
            .clamp(1, EFFECTIVELY_UNLIMITED_TOOL_ROUNDS)
    }
}

/// Resolve a parsed `/rounds` command to the override it lands on. Keeping this
/// transition pure makes the two different reset intents explicit and testable:
/// `Reset` removes the override (so tenacity/config derive the next value),
/// while `Configured` installs the raw config/model number as an override.
///
/// **It performs the derivation; it does not perform the write** (#1998).
/// `double` and `unlimited` are relative operations, which is why `/rounds`
/// stays a verb — but the value they resolve to goes through
/// `settings_form::apply_and_record` like every other setting, so the
/// escalation that produced #1965 now leaves a receipt.
fn apply_tool_round_limit_command(
    configured: usize,
    explicit_tenacity: Option<newt_core::Tenacity>,
    session_override: Option<usize>,
    command: ToolRoundLimitCommand,
) -> Option<usize> {
    match command {
        ToolRoundLimitCommand::Show => session_override,
        ToolRoundLimitCommand::Set(rounds) => Some(rounds),
        ToolRoundLimitCommand::Double => Some(double_tool_round_limit(
            effective_tool_round_limit(configured, explicit_tenacity, session_override).rounds,
        )),
        ToolRoundLimitCommand::Reset => None,
        ToolRoundLimitCommand::Configured => Some(configured),
        ToolRoundLimitCommand::Unlimited => Some(
            effective_tool_round_limit(configured, explicit_tenacity, session_override)
                .rounds
                .max(EFFECTIVELY_UNLIMITED_TOOL_ROUNDS),
        ),
    }
}

fn describe_tool_round_limit(rounds: usize) -> String {
    if rounds >= EFFECTIVELY_UNLIMITED_TOOL_ROUNDS {
        format!("{rounds} (effectively unlimited)")
    } else {
        rounds.to_string()
    }
}

fn tool_round_limit_status(
    configured: usize,
    explicit_tenacity: Option<newt_core::Tenacity>,
    session_override: Option<usize>,
) -> String {
    let posture_default = tenacity_tool_round_limit(configured, explicit_tenacity);
    let explicit_relentless = explicit_tenacity == Some(newt_core::Tenacity::Relentless);
    match session_override {
        Some(rounds) if explicit_relentless => format!(
            "tool-call round limit: {} this session (explicit relentless tenacity default {}; config/model default {})",
            describe_tool_round_limit(rounds),
            describe_tool_round_limit(posture_default),
            describe_tool_round_limit(configured),
        ),
        Some(rounds) => format!(
            "tool-call round limit: {} this session (config/model default {})",
            describe_tool_round_limit(rounds), describe_tool_round_limit(configured),
        ),
        None if explicit_relentless => format!(
            "tool-call round limit: {posture_default} (effectively unlimited; explicit relentless tenacity; config/model default {})",
            describe_tool_round_limit(configured),
        ),
        None => format!(
            "tool-call round limit: {} (config/model default)",
            describe_tool_round_limit(configured)
        ),
    }
}

/// Rich `/psyche` applies several controls at once. Pair its resolved posture
/// summary with the exact same round-budget diagnostic `/rounds` uses so a
/// pre-existing session override is never hidden by an "applied" message.
#[cfg_attr(not(feature = "rich-tui"), allow(dead_code))]
fn psyche_apply_summary(
    runtime_summary: &str,
    configured: usize,
    explicit_tenacity: Option<newt_core::Tenacity>,
    session_override: Option<usize>,
) -> String {
    format!(
        "{runtime_summary}\n{}",
        tool_round_limit_status(configured, explicit_tenacity, session_override)
    )
}

/// Ollama context-window cap. Resolution order:
///   1. `NEWT_NUM_CTX` env var (set by `--num-ctx` CLI flag or manually)
///   2. `[tui] num_ctx` in config
///   3. None → Ollama uses the model's compiled-in default
fn num_ctx(cfg: &newt_core::Config) -> Option<u32> {
    if let Ok(val) = std::env::var("NEWT_NUM_CTX") {
        if let Ok(n) = val.trim().parse::<u32>() {
            return Some(n);
        }
    }
    cfg.tui.as_ref().and_then(|t| t.num_ctx)
}

/// Whether to use empirical "real context discovery" (conserve-and-ratchet) for
/// `model`, vs. trusting the declared `/api/show` window. Resolution: per-model
/// `[[model_tuning]] real_context_discovery` wins, else `[tui]
/// real_context_discovery`, else `false` (trust declared — the default).
fn real_context_discovery(cfg: &newt_core::Config, model: &str) -> bool {
    cfg.find_model_tuning(model)
        .and_then(|t| t.real_context_discovery)
        .or_else(|| cfg.tui.as_ref().and_then(|t| t.real_context_discovery))
        .unwrap_or(false)
}

/// TCP connect timeout from `[tui].connect_timeout_secs` (default 5).
fn connect_timeout_secs(cfg: &newt_core::Config) -> u64 {
    cfg.tui
        .as_ref()
        .map(|t| t.connect_timeout_secs)
        .unwrap_or(5)
}

/// Full inference timeout from `[tui].inference_timeout_secs` (default 120).
fn inference_timeout_secs(cfg: &newt_core::Config) -> u64 {
    cfg.tui
        .as_ref()
        .map(|t| t.inference_timeout_secs)
        .unwrap_or(120)
}

/// Ollama keep_alive from `[tui].keep_alive` (default "5m").
pub(crate) fn keep_alive_str(cfg: &newt_core::Config) -> String {
    cfg.tui
        .as_ref()
        .map(|t| t.keep_alive.clone())
        .unwrap_or_else(|| "5m".to_string())
}

/// Build `SummarizerOpts` from the dedicated [`newt_core::SummarizerConfig`]
/// (Step 24.10; the timeout / retries / fallback knobs moved out of `[tui]`).
/// `keep_alive` falls back to `[tui].keep_alive` when the summarizer file does
/// not pin its own.
fn summarizer_opts(
    sum_cfg: &newt_core::SummarizerConfig,
    cfg: &newt_core::Config,
    num_ctx: Option<u32>,
    color: bool,
) -> SummarizerOpts {
    SummarizerOpts {
        num_ctx,
        keep_alive: sum_cfg
            .keep_alive
            .clone()
            .unwrap_or_else(|| keep_alive_str(cfg)),
        timeout_secs: sum_cfg.timeout_secs,
        retries: sum_cfg.retries,
        fallback_model: sum_cfg.fallback_model.clone(),
        color,
        // The live-notice decision is ownership, not styling: detect it once
        // here rather than re-deriving it from `color` at three call sites.
        caps: newt_core::tty::LineCaps::detect(),
    }
}

/// The summarizer's DEFAULT backend decision (when the operator pins NO
/// `[summarizer]` override). The anti-regression seam — the test below pins it,
/// so the codebase can't quietly slip back to reusing the session model (which
/// it repeatedly has). See memory `feedback_summarizer_defaults_to_embedded_cpu`.
#[derive(Debug, PartialEq)]
enum SummarizerChoice {
    /// The DEFAULT: the on-host embedded CPU engine (#661 group C), this GGUF.
    Embedded(String),
    /// Embedded is unavailable (feature off / no model pulled) → degrade to
    /// reusing the session model, which contends with the primary model.
    DegradedSession,
}

/// With no override, prefer the embedded CPU engine; only degrade to the
/// session model when no embedded GGUF resolves. REGRESSION GUARD — flipping
/// this to default to the session model fails the test below.
fn default_summarizer_choice(embedded_gguf: Option<String>) -> SummarizerChoice {
    match embedded_gguf {
        Some(path) => SummarizerChoice::Embedded(path),
        None => SummarizerChoice::DegradedSession,
    }
}

/// The default embedded summarizer GGUF, IFF the `embedded` engine is compiled
/// AND the default palette model has been pulled to `~/.newt/models`
/// (`newt models pull`). `None` otherwise → a warned degrade to the session.
fn embedded_summarizer_default() -> Option<String> {
    #[cfg(feature = "embedded")]
    {
        newt_inference::palette::resolve_local(newt_inference::palette::default_model().name)
            .map(|p| p.to_string_lossy().into_owned())
    }
    #[cfg(not(feature = "embedded"))]
    {
        None
    }
}

fn warn_summarizer_once(flag: &std::sync::atomic::AtomicBool, msg: &str) {
    if !flag.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!("{msg}");
    }
}

/// Resolve the summarizer's effective backend. The DEFAULT is the on-host
/// embedded CPU engine (#661 group C): context compaction must NOT run on the
/// session GPU model — compaction fires under peak load, so reusing the loaded
/// model overloads the GPU and stalls the turn (#979). A `[summarizer]` backend
/// override (session / off-box) is honored but WARNS; if the embedded engine is
/// unavailable (feature not compiled, or no model pulled — `newt models pull`)
/// it degrades to the session model with a loud warning. This default keeps
/// regressing; `default_summarizer_choice` + its test are the guard.
fn resolve_summarizer_backend(
    sum_cfg: &newt_core::SummarizerConfig,
    inf_url: &str,
    inf_model: &str,
    inf_kind: newt_core::BackendKind,
    inf_key: &Option<String>,
    // The resolved embedded-default GGUF path, or `None` when no on-host model is
    // available (→ degrade to session). INJECTED so resolution is hermetic in
    // tests; production passes `embedded_summarizer_default()`, which reads the
    // real `~/.newt/models`. (Reading it inline made this machine-dependent — a
    // provisioned box resolved to embedded and flipped the session-reuse test.)
    embedded_gguf: Option<String>,
) -> (
    String,
    String,
    newt_core::BackendKind,
    Option<String>,
    Option<String>,
) {
    let has_override = sum_cfg.kind.is_some()
        || sum_cfg.endpoint.is_some()
        || sum_cfg.model.is_some()
        || sum_cfg.model_path.is_some();

    if !has_override {
        match default_summarizer_choice(embedded_gguf) {
            SummarizerChoice::Embedded(path) => {
                let model = newt_inference::palette::default_model().name.to_string();
                // url/key are unused for the in-process embedded engine.
                return (
                    String::new(),
                    model,
                    newt_core::BackendKind::Embedded,
                    None,
                    Some(path),
                );
            }
            SummarizerChoice::DegradedSession => {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                warn_summarizer_once(&WARNED, "warning: the on-host embedded CPU summarizer is unavailable — context compaction will REUSE THE SESSION MODEL (on the GPU), which contends with the primary model under load and can stall the turn (see #979/#661). Enable it: `newt models pull`, and build with the `embedded` feature.");
            }
        }
    }

    // Override, or degraded fallback: reuse the session backend (or a pinned
    // off-box backend). Each present `summarizer.toml` field overrides the
    // session value; the session params are read live so a mid-session
    // `/backend` switch still carries a session-reusing summarizer along.
    let url = sum_cfg
        .endpoint
        .clone()
        .unwrap_or_else(|| inf_url.to_string());
    let model = sum_cfg
        .model
        .clone()
        .unwrap_or_else(|| inf_model.to_string());
    let kind = sum_cfg.kind.unwrap_or(inf_kind);
    let model_path = sum_cfg.model_path.clone();
    // A bearer token authenticates a specific host. Only inherit the session
    // key when the summarizer reuses the session endpoint; never leak it to a
    // pinned different host.
    let key = if sum_cfg.endpoint.is_some() {
        sum_cfg.resolve_api_key()
    } else {
        sum_cfg.resolve_api_key().or_else(|| inf_key.clone())
    };
    // Warn once when an explicit override picks a NON-embedded backend (embedded
    // is the default; a session/off-box override can contend with the primary).
    if has_override && !matches!(kind, newt_core::BackendKind::Embedded) {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        warn_summarizer_once(&WARNED, "warning: using a summarizer OVERRIDE (session / off-box) instead of the on-host embedded CPU default — this can contend with the primary model under load (#661). Drop the `[summarizer]` backend override to use the embedded engine.");
    }
    (url, model, kind, key, model_path)
}

#[cfg(test)]
#[path = "lib_tests/summarizer_default_tests.rs"]
mod summarizer_default_tests;

/// Whether a SESSION-INHERITING summarizer — one that resolves onto the active
/// inference backend, including the degraded fallback when the embedded engine
/// is unavailable — must FOLLOW a live `/model` / `/backend` switch, as opposed
/// to an explicitly PINNED one that stays fixed.
///
/// This is the ownership rule made explicit in ONE place; it mirrors
/// [`resolve_summarizer_backend`]'s pinned-vs-inherited decision:
///
/// - No `[summarizer]` override + embedded engine available → the in-process
///   embedded summarizer, independent of the session → does NOT follow.
/// - No override + embedded UNavailable → degraded reuse of the session backend
///   → follows.
/// - A pinned endpoint, a pinned `kind = "embedded"`, or a pinned `model_path`
///   → an independent backend → does NOT follow.
/// - A partial override that leaves the endpoint to inherit `inf_url` (e.g. only
///   a model name pinned) → still targets the session backend → follows.
fn summarizer_follows_session(
    sum_cfg: &newt_core::SummarizerConfig,
    embedded_gguf: Option<String>,
) -> bool {
    let has_override = sum_cfg.kind.is_some()
        || sum_cfg.endpoint.is_some()
        || sum_cfg.model.is_some()
        || sum_cfg.model_path.is_some();
    if !has_override {
        return matches!(
            default_summarizer_choice(embedded_gguf),
            SummarizerChoice::DegradedSession
        );
    }
    sum_cfg.endpoint.is_none()
        && sum_cfg.kind != Some(newt_core::BackendKind::Embedded)
        && sum_cfg.model_path.is_none()
}

/// Build a loop summarizer for the current session backend, applying any
/// `~/.newt/summarizer.toml` backend + knob overrides (Step 24.10). The single
/// seam every summarizer call site goes through.
#[allow(clippy::too_many_arguments)]
fn build_session_summarizer(
    sum_cfg: &newt_core::SummarizerConfig,
    cfg: &newt_core::Config,
    inf_url: &str,
    inf_model: &str,
    inf_kind: newt_core::BackendKind,
    inf_key: &Option<String>,
    num_ctx: Option<u32>,
    color: bool,
) -> newt_core::Summarizer {
    let (url, model, kind, key, model_path) = resolve_summarizer_backend(
        sum_cfg,
        inf_url,
        inf_model,
        inf_kind,
        inf_key,
        embedded_summarizer_default(),
    );
    make_loop_summarizer(
        url,
        model,
        kind,
        key,
        model_path,
        summarizer_opts(sum_cfg, cfg, num_ctx, color),
    )
}

/// Build the decision adjudicator (#1749).
///
/// Defaults to the **session** backend: adjudication decides whether the
/// operator delegated a choice, which is the steering model's own job and wants
/// the model that already holds the prompt. It deliberately does NOT ride the
/// summarizer — that one defaults to CPU-local inference precisely to keep
/// compaction load off the inference box, which makes it the wrong host for
/// reading intent.
///
/// `[intake.adjudicator]` overrides it field-by-field for an operator who wants
/// to spread load; `BackendRef::resolve` applies the "never inherit the session
/// key to a pinned host" rule.
pub(crate) fn build_adjudicator(
    cfg: &newt_core::Config,
    inf_url: &str,
    inf_model: &str,
    inf_kind: newt_core::BackendKind,
    inf_key: &Option<String>,
    num_ctx: Option<u32>,
    color: bool,
) -> newt_core::Summarizer {
    let (url, model, kind, key) = cfg
        .intake
        .as_ref()
        .and_then(|intake| intake.adjudicator.as_ref())
        .map_or_else(
            || {
                (
                    inf_url.to_string(),
                    inf_model.to_string(),
                    inf_kind,
                    inf_key.clone(),
                )
            },
            |over| over.resolve(inf_url, inf_model, inf_kind, inf_key),
        );
    let opts = SummarizerOpts {
        num_ctx,
        color,
        caps: newt_core::tty::LineCaps::detect(),
        ..SummarizerOpts::default()
    };
    make_loop_summarizer(url, model, kind, key, None, opts)
}

/// Whether to render assistant Markdown this turn (Step 25.4, #568). The
/// session `/markdown` override wins over `[tui].markdown`; either way the
/// result is gated by `color` (Markdown emits ANSI, so it is off without color).
/// Whether Markdown renders right now.
///
/// # The `session` parameter is gone (#2009 PR4)
///
/// It was a `run_chat` local threaded through nine call sites, which meant the
/// session's override was invisible to anything outside that loop — including
/// `settings_form::apply`, which is exactly what had to see it for `/markdown`
/// to become a field. `session_markdown_mode` owns the precedence now, so this
/// asks it rather than being told.
///
/// `cfg` stays a parameter: callers already hold the resolved config, and the
/// resolver falls back to `Config::resolve()` only when nobody has.
fn markdown_enabled(cfg: &newt_core::Config, color: bool) -> bool {
    let mode = if newt_core::config::markdown_is_session_pinned() {
        newt_core::config::session_markdown_mode()
    } else {
        cfg.tui.as_ref().map(|t| t.markdown).unwrap_or_default()
    };
    mode.forced().unwrap_or(color) && color
}

/// Resolve the active context manager (Step 24.8, #559): session override
/// (`/context manager`) → `[context].manager` → `standard`.
fn context_manager(
    cfg: &newt_core::Config,
    session: Option<newt_core::ContextManager>,
) -> newt_core::ContextManager {
    session
        .or_else(|| cfg.context.as_ref().map(|c| c.manager))
        .unwrap_or_default()
}

/// Resolve the automatic-compaction trigger policy: the interactive-session
/// override (`/context compaction`) wins over `[context]`, which in turn falls
/// back to the safe headroom-aware default.
/// The effective compaction trigger policy.
///
/// # The `session` parameter is gone (#2009 PR7)
///
/// It was a `run_chat` local threaded through seven call sites, so the
/// session's override was invisible to anything outside that loop — including
/// `settings_form::apply`, which is exactly what had to see it for
/// `/context compaction` to become a field. `session_compaction_trigger_policy`
/// owns the precedence now, so this asks rather than being told.
fn compaction_trigger_policy(cfg: &newt_core::Config) -> newt_core::CompactionTriggerPolicy {
    if newt_core::config::compaction_trigger_is_session_pinned() {
        return newt_core::config::session_compaction_trigger_policy();
    }
    configured_compaction_trigger_policy(cfg)
}

/// The policy IGNORING any session pin — what `reset` would fall back to.
///
/// Named rather than expressed as `compaction_trigger_policy(cfg, None)`,
/// which is how it read while the override was a parameter. With the override
/// global there is no `None` to pass, and "resolve as if unpinned" is a real
/// question with one answer instead of an argument someone has to get right.
fn configured_compaction_trigger_policy(
    cfg: &newt_core::Config,
) -> newt_core::CompactionTriggerPolicy {
    cfg.context
        .as_ref()
        .map(|c| c.compaction_trigger_policy)
        .unwrap_or_default()
}

/// Human-readable provenance for [`compaction_trigger_policy`]. A present
/// `[context]` section is the closest provenance the deserialized config can
/// preserve; TOML does not retain whether an individual defaulted field was
/// explicitly written.
fn compaction_trigger_policy_source(cfg: &newt_core::Config) -> &'static str {
    if newt_core::config::compaction_trigger_is_session_pinned() {
        "session"
    } else if cfg.context.is_some() {
        "config"
    } else {
        "default"
    }
}

/// Resolve the effective context-feature set (Step 26.1, #588): the `manager`
/// preset's base bundle → `[context.features]` config overrides → session
/// (`/context feature`) overrides.
fn context_features(
    cfg: &newt_core::Config,
    manager: newt_core::ContextManager,
    session: &newt_core::ContextFeatures,
    kind: newt_core::BackendKind,
) -> newt_core::ContextFeatureSet {
    // Step 27.4: the base depends on the backend — local (Ollama) sessions
    // default the local-assist features (scratchpad + semantic + scheduled) ON;
    // cloud keeps the all-off preset baseline. `[context.features]` config then
    // session overrides still layer on top and win.
    let base = newt_core::ContextFeatureSet::base_for(manager, kind);
    let with_config = cfg
        .context
        .as_ref()
        .map(|c| c.features)
        .unwrap_or_default()
        .apply_to(base);
    session.apply_to(with_config)
}

/// Outcome of a `/context …` command (Step 26.1, #588). Pure — no stdout, no
/// mutation — so the dispatch logic is unit-testable: the caller prints `lines`
/// and applies any `set_manager` / `set_feature` to the session overrides.
#[derive(Debug, Default, PartialEq, Eq)]
struct ContextCommandResult {
    lines: Vec<String>,
    set_manager: Option<newt_core::ContextManager>,
    set_feature: Option<(newt_core::ContextFeature, bool)>,
    /// `/context size <N>` session budget override; `Some(0)` clears it back
    /// to the probed / configured value.
    set_budget: Option<u32>,
    /// `/context compaction …` mutation. The explicit `Reset` variant keeps a
    /// reset distinguishable from a command that simply leaves the override
    /// alone.
    set_compaction_trigger_policy: Option<CompactionTriggerPolicyOverride>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactionTriggerPolicyOverride {
    Set(newt_core::CompactionTriggerPolicy),
    Reset,
}

fn handle_context_feature_arg(
    arg: &str,
    features: newt_core::ContextFeatureSet,
    out: &mut ContextCommandResult,
) {
    use newt_core::ContextFeature;
    let mut parts = arg.split_whitespace();
    let name = parts.next().unwrap_or("");
    let toggle = parts.next();
    match ContextFeature::from_keyword(name) {
        Some(f) => match toggle {
            None => {
                let state = if features.get(f) { "on" } else { "off" };
                let tail = if f.available() {
                    String::new()
                } else {
                    format!(" (not yet available — #{})", f.issue())
                };
                out.lines
                    .push(format!("context feature {}: {state}{tail}", f.keyword()));
            }
            Some(t @ ("on" | "off")) => {
                let want = t == "on";
                if f.available() {
                    out.set_feature = Some((f, want));
                    out.lines
                        .push(format!("context feature {} → {t}", f.keyword()));
                } else {
                    // Report the ACTUAL resolved state, not a hardcoded "off" —
                    // config can force an unimplemented feature on, and the
                    // toggle (which we refuse) doesn't change it.
                    let state = if features.get(f) { "on" } else { "off" };
                    out.lines.push(format!(
                        "context feature '{}' is not yet available (see #{}) — staying {state}",
                        f.keyword(),
                        f.issue()
                    ));
                }
            }
            Some(other) => out.lines.push(format!(
                "unknown toggle '{other}' for /context feature — use on|off"
            )),
        },
        None => out.lines.push(format!(
            "unknown context feature '{name}' — use {}",
            ContextFeature::ALL
                .iter()
                .map(|f| f.keyword())
                .collect::<Vec<_>>()
                .join("|")
        )),
    }
}

/// Dispatch `/context [manager <preset> | feature <name> [on|off] | compaction
/// <policy>]` against the current config + session overrides (Step 26.1, #588).
/// `rest` is the text after `context`. Unavailable presets/features report
/// "not yet available" and are NOT applied.
fn handle_context_command(
    rest: &str,
    cfg: &newt_core::Config,
    manager_override: Option<newt_core::ContextManager>,
    feature_override: &newt_core::ContextFeatures,
    kind: newt_core::BackendKind,
) -> ContextCommandResult {
    use newt_core::{ContextFeature, ContextManager};
    let rest = rest.trim();
    let manager = context_manager(cfg, manager_override);
    let features = context_features(cfg, manager, feature_override, kind);
    let compaction_policy = compaction_trigger_policy(cfg);
    let compaction_policy_source = compaction_trigger_policy_source(cfg);
    let mgr_src = if manager_override.is_some() {
        "session"
    } else {
        "config"
    };
    let feat_summary = || {
        let on = features.enabled();
        if on.is_empty() {
            "none".to_string()
        } else {
            // Honest: a feature can be config-forced on before it's implemented;
            // mark those as pending so "on" never implies "actually running".
            on.iter()
                .map(|f| {
                    if f.available() {
                        f.keyword().to_string()
                    } else {
                        format!("{} (pending #{})", f.keyword(), f.issue())
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let mut out = ContextCommandResult::default();

    if rest.is_empty() {
        out.lines.push(format!(
            "context manager: {} ({mgr_src}); features on: {}",
            manager.keyword(),
            feat_summary()
        ));
        out.lines.push(format!(
            "compaction trigger policy: {} ({compaction_policy_source})",
            compaction_policy.keyword()
        ));
        out.lines.push(
            "  /context manager <preset>  ·  /context feature <name> [on|off]  ·  \
             /context compaction [headroom_aware|message_count|reset]  ·  /context stats"
                .to_string(),
        );
    } else if rest == "size" {
        out.lines.push(
            "context size: use /context size <N> to set the per-turn send \
             budget (tokens), or /context size reset to restore the auto-sized value"
                .to_string(),
        );
    } else if let Some(arg) = rest.strip_prefix("size ") {
        let arg = arg.trim();
        if arg == "reset" || arg == "auto" || arg == "0" {
            out.set_budget = Some(0);
            out.lines
                .push("context size → reset (auto-sized budget)".to_string());
        } else {
            match arg.parse::<u32>() {
                Ok(n) if n > 0 => {
                    out.set_budget = Some(n);
                    out.lines
                        .push(format!("context size → {n} tokens (session override)"));
                }
                _ => out.lines.push(format!(
                    "invalid context size '{arg}' — use a positive token count or 'reset'"
                )),
            }
        }
    } else if rest == "manager" {
        out.lines.push(format!(
            "context manager: {} ({mgr_src}) — \
             use /context manager standard|append-only|progressive|distributed",
            manager.keyword()
        ));
    } else if let Some(name) = rest.strip_prefix("manager ") {
        match ContextManager::from_keyword(name.trim()) {
            Some(m) if m.available() => {
                out.set_manager = Some(m);
                out.lines.push(format!("context manager → {}", m.keyword()));
            }
            Some(m) => out.lines.push(format!(
                "context manager '{}' is not yet available (see #546) — staying on {}",
                m.keyword(),
                manager.keyword()
            )),
            None => out.lines.push(format!(
                "unknown context manager '{}' — use standard|append-only|progressive|distributed",
                name.trim()
            )),
        }
    } else if rest == "compaction" {
        out.lines.push(format!(
            "compaction trigger policy: {} ({compaction_policy_source}) — \
             use /context compaction headroom_aware|message_count|reset",
            compaction_policy.keyword()
        ));
    } else if let Some(policy) = rest.strip_prefix("compaction ") {
        let policy = policy.trim();
        if policy.eq_ignore_ascii_case("reset") {
            out.set_compaction_trigger_policy = Some(CompactionTriggerPolicyOverride::Reset);
            // The PREVIEW is deliberately unpinned: this line says what the
            // policy becomes after the reset the caller is about to apply, and
            // the pin is still installed while this runs.
            let reset_policy = configured_compaction_trigger_policy(cfg);
            let reset_source = if cfg.context.is_some() {
                "config"
            } else {
                "default"
            };
            out.lines.push(format!(
                "compaction trigger policy → {} ({reset_source})",
                reset_policy.keyword()
            ));
        } else if let Some(policy) = newt_core::CompactionTriggerPolicy::from_keyword(policy) {
            out.set_compaction_trigger_policy = Some(CompactionTriggerPolicyOverride::Set(policy));
            out.lines.push(format!(
                "compaction trigger policy → {} (session override)",
                policy.keyword()
            ));
        } else {
            out.lines.push(format!(
                "unknown compaction trigger policy '{policy}' — \
                 use headroom_aware|message_count|reset"
            ));
        }
    } else if rest == "feature" || rest == "features" {
        out.lines.push("context features:".to_string());
        for f in ContextFeature::ALL {
            let state = if features.get(f) { "on " } else { "off" };
            let tail = if f.available() {
                String::new()
            } else {
                format!("  (not yet available — #{})", f.issue())
            };
            out.lines.push(format!("  [{state}] {}{tail}", f.keyword()));
        }
    } else if let Some(arg) = rest.strip_prefix("feature ") {
        handle_context_feature_arg(arg, features, &mut out);
    } else if let Some(head) = rest.split_whitespace().next() {
        if ContextFeature::from_keyword(head).is_some() {
            handle_context_feature_arg(rest, features, &mut out);
        } else {
            out.lines.push(format!(
                "unknown /context subcommand '{rest}' — \
                 use /context [manager <preset> | feature <name> [on|off] | \
                 compaction <policy> | size <N> | show | stats]"
            ));
        }
    } else {
        out.lines.push(format!(
            "unknown /context subcommand '{rest}' — \
             use /context [manager <preset> | feature <name> [on|off] | \
             compaction <policy> | size <N> | show | stats]"
        ));
    }
    out
}

/// Render the `/context stats` experimentation dashboard (Step 26.2, #588).
/// Composes the live context budget (the 24.5/24.6 gauge state), the
/// compression counters, the effective automatic-compaction policy, and the
/// resolved feature set with per-feature impact. Pure → unit-testable.
/// `tool_offload_impact` = `(spills, offloaded_chars)` from the session spill
/// store (Step 26.3); other features instrument as they land (26.4+).
#[allow(clippy::too_many_arguments)] // gauge + counters + policy + features + one impact tuple per feature
fn context_stats_text(
    gauge: Option<(u32, u32)>,
    counters: &newt_core::CompressCounters,
    compaction_policy: newt_core::CompactionTriggerPolicy,
    compaction_policy_source: &str,
    features: newt_core::ContextFeatureSet,
    tool_offload_impact: Option<(u64, u64)>,
    scratchpad_impact: Option<(u64, u64)>,
    semantic_impact: Option<(u64, u64)>,
    experiential_impact: Option<(u64, u64)>,
    scheduled_impact: Option<(u64, u64)>,
) -> Vec<String> {
    let mut lines = vec!["context stats".to_string()];
    // Live send-budget fill (None until a turn has reported usage).
    match gauge {
        Some((used, budget)) if budget > 0 => {
            let pct = (u64::from(used) * 100 / u64::from(budget)) as u32;
            lines.push(format!(
                "  budget: {} ({pct}% of the send window)",
                newt_core::agentic::fmt_token_gauge(used, budget)
            ));
        }
        _ => lines.push("  budget: not yet measured (no completed turn)".to_string()),
    }
    lines.push(format!(
        "  automatic compaction: {} ({compaction_policy_source})",
        compaction_policy.keyword()
    ));
    // Compression activity — reuse the /memory anti-thrash section verbatim.
    for l in memory_compress_section(counters).lines() {
        lines.push(l.to_string());
    }
    // Feature states + per-feature impact (the experimentation payoff).
    lines.push("  features:".to_string());
    for f in newt_core::ContextFeature::ALL {
        let state = if features.get(f) { "on " } else { "off" };
        let tail = if f.available() {
            String::new()
        } else {
            format!("  (pending #{})", f.issue())
        };
        let mut line = format!("    [{state}] {}{tail}", f.keyword());
        // Step 26.3: tool_offload's measured impact this session.
        if f == newt_core::ContextFeature::ToolOffload {
            if let Some((spills, chars)) = tool_offload_impact {
                line.push_str(&format!(
                    "  — {spills} offloaded (~{}k chars elided)",
                    chars / 1000
                ));
            }
        }
        // Step 26.4: scratchpad's measured impact this session.
        if f == newt_core::ContextFeature::Scratchpad {
            if let Some((keys, chars)) = scratchpad_impact {
                line.push_str(&format!("  — {keys} keys (~{}k chars)", chars / 1000));
            }
        }
        // Step 26.5: semantic's measured impact this session.
        if f == newt_core::ContextFeature::Semantic {
            if let Some((chunks, chars)) = semantic_impact {
                line.push_str(&format!(
                    "  — {chunks} chunks indexed (~{}k chars)",
                    chars / 1000
                ));
            }
        }
        // Step 26.6a: experiential's measured impact this session.
        if f == newt_core::ContextFeature::Experiential {
            if let Some((n, chars)) = experiential_impact {
                line.push_str(&format!("  — {n} experiences (~{}k chars)", chars / 1000));
            }
        }
        // Step 26.6b: scheduled's plan progress this session.
        if f == newt_core::ContextFeature::Scheduled {
            if let Some((steps, done)) = scheduled_impact {
                line.push_str(&format!("  — {done}/{steps} plan steps done"));
            }
        }
        lines.push(line);
    }
    lines
}

/// Mid-loop message-trim threshold from `[tui].mid_loop_trim_threshold` (default 40).
///
/// Clamped to `max_tool_rounds - 3` so the safety valve always fires before the
/// round ceiling. With both canonical defaults at 40, trimming starts at round
/// 37 instead of colliding with the ceiling.
fn mid_loop_trim_threshold(cfg: &newt_core::Config) -> usize {
    let threshold = cfg
        .tui
        .as_ref()
        .map(|t| t.mid_loop_trim_threshold)
        .unwrap_or_else(|| newt_core::TuiConfig::default().mid_loop_trim_threshold);
    threshold.min(max_tool_rounds(cfg).saturating_sub(3))
}

/// Build-check command from `[tui].build_check_cmd`. `None` means no auto-check.
fn build_check_cmd(cfg: &newt_core::Config) -> Option<String> {
    cfg.tui.as_ref().and_then(|t| t.build_check_cmd.clone())
}

/// Print the telemetry summary line after an inference turn.
fn print_metrics(metrics: &newt_core::TurnMetrics, color: bool) {
    let line = metrics.display_line();
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!("  {line}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("  {line}");
    }
    io::stdout().flush().ok();
}

fn print_thinking(color: bool) {
    if color {
        // Frame-0 placeholder matching the animated spinner the inference loop
        // takes over (newt-core animates the probe wait in place), so there is
        // no glyph jump between this instant feedback and the live line.
        //
        // #1413: taken from the canonical frame set rather than written as a
        // literal. The "no glyph jump" promise above is only true if this glyph
        // and the one newt-core animates come from the same array — a literal
        // makes that a coincidence maintained by hand.
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!("{} thinking…", newt_core::tty::SPINNER_FRAMES[0])),
            ResetColor,
        )
        .ok();
        io::stdout().flush().ok();
    }
}

fn erase_line() {
    // Clear-to-end-of-line: the animated thinking line ("⏳ ⠋ thinking… 12.3s")
    // can be wider than a fixed blank run, so `\x1b[K` wipes whatever is there.
    //
    // #1413 — STILL ARBITER-BYPASSING, deliberately, and here is why the
    // obvious fix is wrong.
    //
    // This erase pairs with `print_thinking` (~800 lines up in `run_chat`), so
    // the row is ephemeral for the whole turn. The tempting fix is to hold a
    // `Terminal::lease` across that span and let `LineLease::drop` erase. It
    // would deadlock the spinner: `lease` is exclusive on `Inner.line_held`,
    // and `print_thinking`'s own comment says newt-core "takes over" this row —
    // `Spinner::start_with_caps` inside `stream_response` leases it. A
    // turn-length lease here starves that call for `LEASE_WAIT` and it returns
    // `None`: no thinking spinner, every turn. That is the same failure mode
    // rejected for the live-spill viewport in #1410.
    //
    // The real fix is to model the HANDOFF — newt-tui paints a placeholder,
    // newt-core adopts the same row — which is #1312's spinner migration, not a
    // local change. Until then this stays open-coded, and the pairing above is
    // the reason, not an oversight.
    print!("\r\x1b[K");
    io::stdout().flush().ok();
}

// ---------------------------------------------------------------------------
// Esc-to-interrupt: watch the keyboard during a turn and trip a cancel flag.
//
// The turn (`chat_complete`) runs in-place — it borrows half the session, so it
// can't move to a background task. Instead a sibling OS thread watches stdin and
// flips an `AtomicBool` the loop polls at its await checkpoints. The terminal is
// put in *cbreak* (ICANON+ECHO off) so a keypress arrives immediately, but ISIG
// and OPOST stay ON: Ctrl-C still signals as before, and streamed model output
// still gets CR-NL translation (no staircase) while we watch.
// ---------------------------------------------------------------------------

/// A lone `Esc` (`0x1b`) is the interrupt; `Esc [` / `Esc O` begin an arrow /
/// function-key sequence, and `Esc <char>` is an Alt-chord — neither interrupts.
/// Terminals deliver a real Esc press as a single `0x1b` byte, so an exact match
/// cleanly separates it from a multi-byte escape sequence read in one burst.
/// Unix-only: the keyboard watcher (its sole caller) needs termios.
#[cfg(unix)]
fn is_lone_esc(bytes: &[u8]) -> bool {
    bytes == [0x1b]
}

/// Ctrl-C arrives as `ETX` (`0x03`) once ISIG is off (see `CbreakGuard`). We
/// treat an interrupt as `0x03` appearing ANYWHERE in the read buffer, not only
/// as a lone `[0x03]` (#1303 FIX A): a single 64-byte read can coalesce the
/// `0x03` with other bytes — e.g. a trailing SGR-mouse event `ESC[<..M` under
/// motion, or fast typed-ahead — and an exact-match check would silently drop
/// the cancel. `0x03` never occurs inside a legitimate keystroke burst (escape
/// sequences use `0x1b`/`[`/digits; text is printable), so scanning is safe.
/// Unix-only: the keyboard watcher (its sole caller) needs termios.
#[cfg(unix)]
fn is_ctrl_c(bytes: &[u8]) -> bool {
    bytes.contains(&0x03)
}

// The trait itself stays available on every platform: `chat.rs` names
// `Option<&dyn SpillInput>` unconditionally to type the spill handle it
// threads through `with_live_spill_watch`. Only its methods — driven by the
// unix-only keyboard watcher (`dispatch_turn_keys`, `watch_for_interrupt_fd`)
// — are unix-gated, so a non-unix build never has an unused-method warning.
pub(crate) trait SpillInput: Sync {
    #[cfg(unix)]
    fn scroll_up(&self) -> bool;
    #[cfg(unix)]
    fn scroll_down(&self) -> bool;
    #[cfg(unix)]
    fn toggle_expanded(&self) -> bool;
    /// #1704: expand the viewport to half the visible console height.
    #[cfg(unix)]
    fn expand_half(&self) -> bool;
    /// #1704: is the user scrolled back (explore mode, not following the tail)?
    #[cfg(unix)]
    fn is_exploring(&self) -> bool;
    /// #1704: leave explore mode — snap back to following the tail.
    #[cfg(unix)]
    fn exit_explore(&self) -> bool;
    #[cfg(unix)]
    fn refresh_geometry(&self) -> bool;
    // #1303 step 5: editor-mode nav targets (vi `gg`/`G`/`C-d`/`C-u`, emacs
    // paging). Gated with their dispatch arms so the lean build links none.
    #[cfg(all(unix, feature = "live-spill"))]
    fn scroll_to_top(&self) -> bool;
    #[cfg(all(unix, feature = "live-spill"))]
    fn scroll_to_bottom(&self) -> bool;
    #[cfg(all(unix, feature = "live-spill"))]
    fn half_page_up(&self) -> bool;
    #[cfg(all(unix, feature = "live-spill"))]
    fn half_page_down(&self) -> bool;
}

#[cfg(feature = "live-spill")]
impl SpillInput for live_spill::LiveSpillRenderer {
    #[cfg(unix)]
    fn scroll_up(&self) -> bool {
        self.scroll_up()
    }

    #[cfg(unix)]
    fn scroll_down(&self) -> bool {
        self.scroll_down()
    }

    #[cfg(unix)]
    fn toggle_expanded(&self) -> bool {
        self.toggle_expanded()
    }

    #[cfg(unix)]
    fn expand_half(&self) -> bool {
        self.expand_half()
    }

    #[cfg(unix)]
    fn is_exploring(&self) -> bool {
        self.is_exploring()
    }

    #[cfg(unix)]
    fn exit_explore(&self) -> bool {
        self.exit_explore()
    }

    #[cfg(unix)]
    fn refresh_geometry(&self) -> bool {
        self.refresh_geometry()
    }

    #[cfg(unix)]
    fn scroll_to_top(&self) -> bool {
        self.scroll_to_top()
    }

    #[cfg(unix)]
    fn scroll_to_bottom(&self) -> bool {
        self.scroll_to_bottom()
    }

    #[cfg(unix)]
    fn half_page_up(&self) -> bool {
        self.half_page_up()
    }

    #[cfg(unix)]
    fn half_page_down(&self) -> bool {
        self.half_page_down()
    }
}

// ---------------------------------------------------------------------------
// Slash command dispatcher
// ---------------------------------------------------------------------------

/// `(topic, arg)` from a slash line if it is asking for help — `/<cmd> --help`,
/// `/<cmd> -h`, `/<cmd> help`, or `/help <cmd>`. `None` when it's an ordinary
/// command. Pure so the dispatch interception is unit-testable.
fn help_request(task: &str) -> Option<String> {
    let body = task.trim_start_matches('/');
    let mut parts = body.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    if cmd == "help" {
        // `/help <cmd>` → that cmd's page; bare `/help` is the full list (None).
        return rest.first().map(|c| c.to_string());
    }
    let is_help_token = |a: &&str| matches!(*a, "--help" | "-h" | "help");
    // #1030: `/start`, `/rename` and its `/name` alias take a free-text TITLE,
    // so a `help`/`-h`/`--help` token INSIDE a title must NOT be read as a help
    // request — only an invocation whose SOLE argument is the help token asks
    // for help (folded to the page documenting each: /start under /new,
    // /rename under /conversation).
    //
    // **`/name` was missing from this list** until #2009 PR5, so `/name help me
    // debug` opened a help page instead of retitling the conversation. A list
    // of verb names is exactly the shape that goes stale when an alias is
    // added, which is why the settings arm below asks the FIELD instead.
    if matches!(cmd, "start" | "rename" | "name") {
        if rest.len() == 1 && is_help_token(&rest[0]) {
            return Some(
                if cmd == "start" {
                    "new"
                } else {
                    "conversation"
                }
                .to_string(),
            );
        }
        return None;
    }
    // The same rule for a free-text SETTING, arriving through a different
    // door: `/settings prompt "help me"` is a template. Asked of the field, so
    // a future `Text` field is covered without editing this.
    if cmd == "settings" {
        if let Some(field) = rest
            .first()
            .and_then(|f| settings_form::Field::from_token(f))
        {
            if field.takes_free_text() {
                if rest.len() == 2 && is_help_token(&rest[1]) {
                    return Some(cmd.to_string());
                }
                return None;
            }
        }
    }
    if rest.iter().any(is_help_token) {
        return Some(cmd.to_string());
    }
    None
}

/// How a slash command that needs an ANSWER reaches the operator.
///
/// The same shape as `permissions.rs`'s `ask_surface` and `crew_form`'s
/// `Ask`, deliberately: a form needs exactly what a permission question needs.
/// Widening the existing seam is the reuse discipline; a second console path
/// is what produced two live prompts on one screen.
pub(crate) type SlashAsk<'a> = &'a dyn Fn(
    &newt_core::interaction_surface::SurfaceInteraction,
) -> newt_core::HumanQuestionOutcome;

/// The fallback ask for a caller that owns the terminal outright (the plain
/// CLI path, and every test). A cockpit session must NOT use this: it writes
/// under a mounted chat editor that keeps repainting its clock and its live
/// chevron over the question. Sessions pass their surface seam instead.
pub(crate) fn ask_on_this_terminal(
    interaction: &newt_core::interaction_surface::SurfaceInteraction,
) -> newt_core::HumanQuestionOutcome {
    let window =
        newt_core::tty::Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::SlashForm);
    crate::permissions::present_on_terminal(&window, interaction)
}

/// Dispatch a `/command` line. Returns `true` to keep the session alive,
/// `false` to exit.
///
/// Terminal-owning form: any question is asked on THIS terminal. A session
/// with a mounted surface calls [`dispatch_slash_with_ask`] instead.
///
/// Production has exactly one slash call site (`chat.rs`) and it always has a
/// surface, so this convenience form is the test/CLI shape.
#[cfg(test)]
fn dispatch_slash(
    input: &str,
    workspace: &str,
    color: bool,
    verbose: bool,
    markdown: bool,
) -> anyhow::Result<bool> {
    dispatch_slash_with_ask(input, workspace, color, verbose, markdown, None)
}

/// [`dispatch_slash`], with the surface seam a session presents questions
/// through (#1862 C1). `ask = Some(..)` routes a form's questions to the
/// thread that owns the terminal, which is what dims the chat chevron,
/// reserves the modal's rows, and freezes the header clock for the duration.
fn dispatch_slash_with_ask(
    input: &str,
    workspace: &str,
    color: bool,
    verbose: bool,
    markdown: bool,
    ask: Option<SlashAsk<'_>>,
) -> anyhow::Result<bool> {
    let (cmd, arg1, arg2) = slash_parts(input);

    match cmd {
        "exit" | "quit" | "help" | "version" | "workspace" | "config" => {
            commands::meta::dispatch(cmd, arg1, workspace, color, verbose, markdown)
        }
        "prompt" | "vi" | "emacs" | "nano" | "edit-mode" | "thinking" | "nudge" | "tenacity"
        | "cognition" | "psyche" => {
            commands::settings::dispatch(cmd, arg1, input, workspace, color, verbose)
        }
        "models" | "probe" | "model" | "backend" | "backends" | "summarizer" | "dgx" => {
            commands::model::dispatch(cmd, arg1, arg2, color, verbose)
        }
        // #1981: the typed settings form. Absorbs the knob verbs; every route
        // — form, deep link, deprecated verb — lands on `settings_form::apply`.
        "settings" => {
            // The seam the session supplied, or this terminal when there is no
            // session to ask through. ONE ask path either way.
            let fallback = ask_on_this_terminal;
            let ask: SlashAsk<'_> = ask.unwrap_or(&fallback);
            let rest = format!("{arg1} {arg2}");
            for line in settings_form::run(ask, rest.trim()) {
                print_newt(&line, color, verbose);
            }
            Ok(true)
        }
        "crew" => {
            let fallback = ask_on_this_terminal;
            commands::crew::dispatch(arg1, arg2, color, verbose, ask.unwrap_or(&fallback))
        }
        "setup" => commands::setup::dispatch(arg1, color, verbose),
        other => {
            print_newt(&slash_registry::fallthrough_message(other), color, verbose);
            Ok(true)
        }
    }
}

/// Split a slash command into its verb and first two arguments.
///
/// Whitespace between fields is syntax, not an empty argument: `/model  name`
/// must take the same path as `/model name`. The final field keeps embedded
/// whitespace so commands whose second argument is free text retain it.
fn slash_parts(input: &str) -> (&str, &str, &str) {
    let body = input.trim_start_matches('/').trim_start();
    let (cmd, rest) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
    let rest = rest.trim_start();
    let (arg1, arg2) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (cmd, arg1, arg2.trim())
}

/// Fetch model names from an Ollama endpoint's `/api/tags`.
/// Sync model-listing over any backend API (#backend-trait): builds a client
/// and asks `api_for(kind)`, bridging the async trait via block_in_place. The
/// ONE place the TUI lists models — /models, /model, setup, and doctor all
/// route here instead of each matching on `kind`.
pub(crate) fn fetch_models_for(
    url: &str,
    kind: newt_core::BackendKind,
    api_key: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?;
            newt_core::backend_probe::api_for(kind)
                .list_models(&client, url, api_key)
                .await
        })
    })
}

/// Run `newt <args>` as a subprocess using the current executable path so
/// the command works even when newt is not on PATH. stdout/stderr pass
/// through to the terminal unchanged.
pub(crate) fn run_newt_subcmd(args: &[&str], color: bool, verbose: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let status = std::process::Command::new(&exe).args(args).status()?;
    if !status.success() {
        print_newt(
            &format!("command exited with status {}", status.code().unwrap_or(-1)),
            color,
            verbose,
        );
    }
    Ok(())
}

/// Split a prompt line into the host command after a leading `!`, or `None`
/// when the line is not a bang-escape (or is just `!` with no command).
///
/// The `!` bang-escape is a HUMAN action in the interactive readline loop — the
/// model has no channel to type at the prompt, so it can never invoke this. It
/// runs on the host with the user's own authority (no OCAP/Caveats leash, which
/// governs only *model*-initiated `run_command`), like typing in a shell. Its
/// purpose is interactive logins such as `! pa login` (browser SAML).
fn bang_command(input: &str) -> Option<&str> {
    let rest = input.strip_prefix('!')?.trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// The user's shell + its "run this string" flag, per platform. Honors `$SHELL`
/// (unix) / `%COMSPEC%` (windows), falling back to the system default. Running
/// through a shell (not bare-exec) gives pipes, redirects, `&&`, and env
/// expansion — matching shell muscle memory.
///
/// Unix uses `-c` (**non-interactive**), NOT `-ic`. An interactive shell enables
/// job control (monitor mode): it grabs the terminal's foreground process group
/// via `tcsetpgrp` and does not reliably restore newt's on exit, leaving newt in
/// the background → `SIGTTOU` on its next TTY write → "suspended (tty output)".
/// `-c` avoids that entirely. PATH is still inherited from the shell that
/// launched newt, so binaries (e.g. `pa`) resolve fine; only `.zshrc`/`.bashrc`
/// aliases and shell *functions* are unavailable. Windows `cmd /C`.
fn bang_shell() -> (String, &'static str) {
    #[cfg(windows)]
    {
        let sh = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd".to_string());
        (sh, "/C")
    }
    #[cfg(not(windows))]
    {
        let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        (sh, "-c")
    }
}

/// Run a `!`-escaped host command interactively: stdio is **inherited** (the
/// child gets the real TTY, so it can prompt and launch a browser — e.g. the
/// `pa login` SAML flow), output scrolls live, and control returns to the
/// prompt. A non-zero exit prints a thin status line; a spawn failure surfaces
/// the error.
///
/// Interruptibility (unix, TTY): the child runs as its **own foreground process
/// group** — like a shell foreground job. `setpgid` in the child (via
/// `pre_exec`) and `tcsetpgrp` in the parent hand the terminal to the child, so
/// a terminal `Ctrl-C` is delivered to the *child's* group, not newt's. The
/// child dies; newt reclaims the terminal and returns to the prompt instead of
/// being killed alongside a hung command (e.g. `! pa login` waiting on a SAML
/// browser round-trip that never completes). `SIGTTOU`/`SIGTTIN` are ignored
/// across the swap so the `tcsetpgrp` calls don't stop newt.
fn run_bang_escape(cmd: &str, color: bool, verbose: bool) {
    let (shell, flag) = bang_shell();
    let mut command = std::process::Command::new(&shell);
    command.arg(flag).arg(cmd);

    #[cfg(unix)]
    {
        run_bang_escape_unix(command, &shell, color, verbose);
    }
    #[cfg(not(unix))]
    {
        match command.status() {
            Ok(status) if status.success() => {}
            Ok(status) => print_newt(
                &format!("exit {}", status.code().unwrap_or(-1)),
                color,
                verbose,
            ),
            Err(e) => print_newt(&format!("! failed to run `{shell}`: {e}"), color, verbose),
        }
    }
}

/// Unix launch for `run_bang_escape`: put the child in its own process group and
/// give it the controlling terminal so `Ctrl-C` interrupts the *command* and
/// returns to the newt prompt, rather than felling newt itself. Falls back to a
/// plain inherited-stdio `wait` when stdin is not a TTY (piped / non-interactive)
/// or the job-control setup fails.
#[cfg(unix)]
fn run_bang_escape_unix(
    mut command: std::process::Command,
    shell: &str,
    color: bool,
    verbose: bool,
) {
    use std::os::unix::process::CommandExt as _;

    let tty = libc::STDIN_FILENO;
    // Only take over the terminal when we actually own it interactively.
    let interactive = io::stdin().is_terminal();

    if interactive {
        // Child leads a fresh process group; the parent later foregrounds it.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            print_newt(&format!("! failed to run `{shell}`: {e}"), color, verbose);
            return;
        }
    };

    // Hand the terminal to the child's group for the duration of the run.
    // `tcsetpgrp` from a background write would raise SIGTTOU and stop newt, so
    // ignore SIGTTOU/SIGTTIN across the swap and restore the handlers after.
    let mut foregrounded = false;
    let (old_ttou, old_ttin);
    if interactive {
        unsafe {
            old_ttou = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            old_ttin = libc::signal(libc::SIGTTIN, libc::SIG_IGN);
        }
        let child_pgid = child.id() as libc::pid_t;
        // setpgid also here in the parent to close the fork/exec race window.
        unsafe {
            libc::setpgid(child_pgid, child_pgid);
        }
        foregrounded = unsafe { libc::tcsetpgrp(tty, child_pgid) == 0 };
    } else {
        old_ttou = libc::SIG_DFL;
        old_ttin = libc::SIG_DFL;
    }

    let status = child.wait();

    if interactive {
        // Reclaim the terminal for newt's process group, then restore handlers.
        if foregrounded {
            unsafe {
                let newt_pgid = libc::getpgrp();
                libc::tcsetpgrp(tty, newt_pgid);
            }
        }
        unsafe {
            libc::signal(libc::SIGTTOU, old_ttou);
            libc::signal(libc::SIGTTIN, old_ttin);
        }
    }

    match status {
        Ok(status) if status.success() => {}
        Ok(status) => {
            // A signalled child (e.g. interrupted by Ctrl-C) has no exit code.
            let msg = match status.code() {
                Some(code) => format!("exit {code}"),
                None => "interrupted".to_string(),
            };
            print_newt(&msg, color, verbose);
        }
        Err(e) => print_newt(&format!("! `{shell}` wait failed: {e}"), color, verbose),
    }
}

// ---------------------------------------------------------------------------
// `/cd` is the one human navigation command (#1096): it moves `session_cwd`,
// the local session working dir shown in the prompt, confined below the start
// dir. The bare `cd`/`pwd`/`ls`/`rm`/`mv`/`mkdir`/`env`/`date` verbs were
// retired — bare human text is a message to the model, and `!` runs the shell.

/// Parse a `/cd` line. `Some("")` for a bare `/cd` (return to root), `Some(arg)`
/// for `/cd <arg>`, and `None` for anything else (`/cdx`, the bare `cd` verb, …).
///
/// # Why `trim_start_matches('/')` and not `strip_prefix("/cd")`
///
/// **The old shape was invisible to the interception-site pin** (#2009 PR2).
/// That guard counts `trim_start_matches('/')` occurrences, because every
/// top-level command reaches its handler through one — so a new command shows
/// up as a new site and forces a registry review. `/cd` reached its handler
/// through a bespoke prefix strip instead, which is exactly how it stayed an
/// unregistered ghost outside every ratchet while being a real, shipped,
/// state-mutating command.
///
/// Splitting the verb from the argument also removes the `/cdate` class of
/// near-miss by construction rather than by a second `strip_prefix(' ')`
/// check: `/cdate` has the verb `cdate`, which is simply not `cd`.
fn cd_command(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    // The `/` is required — bare `cd` was retired (#1096) and is a message to
    // the model. Everything after it is trimmed the way every other command's
    // interception trims it, so `//cd` behaves like `//help` rather than
    // being the one verb in the shell that refuses a doubled slash.
    if !trimmed.starts_with('/') {
        return None;
    }
    let slash_verb = trimmed.trim_start_matches('/');
    let (verb, arg) = slash_verb
        .split_once(char::is_whitespace)
        .map_or((slash_verb, ""), |(v, a)| (v, a.trim()));
    (verb == "cd").then_some(arg)
}

/// Lexically resolve `.` / `..` WITHOUT touching the filesystem, so a symlinked
/// workspace keeps its symlink path (matching the confined shell's intent) and
/// `..` can't be used to climb — escapes are then rejected by the root check.
fn lexical_normalize(p: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Resolve `arg` against `session_cwd` and CONFINE it under `root` (the session
/// workspace) — the human suite stays inside the same tree the agent is confined
/// to. `None` when the path escapes the root (can't climb above it).
fn confine_under_root(
    root: &std::path::Path,
    session_cwd: &std::path::Path,
    arg: &str,
) -> Option<std::path::PathBuf> {
    let joined = if arg.is_empty() {
        session_cwd.to_path_buf()
    } else {
        let p = std::path::Path::new(arg);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            session_cwd.join(p)
        }
    };
    let normalized = lexical_normalize(&joined);
    let root_norm = lexical_normalize(root);
    normalized.starts_with(&root_norm).then_some(normalized)
}

/// Execute `/cd` against `session_cwd`. Never touches the agent's `workspace`,
/// the OCAP fence, or the process cwd — only the local `session_cwd`, confined
/// under `root`. A bare `/cd` (empty `arg`) returns to the root.
fn run_cd(arg: &str, session_cwd: &mut std::path::PathBuf, root: &str, color: bool, verbose: bool) {
    if matches!(arg, "--help" | "-h") {
        print_newt(
            "/cd [dir] — change the session working dir (below the start dir); bare /cd returns to the root",
            color,
            verbose,
        );
        return;
    }
    let root_path = std::path::Path::new(root);
    if arg.is_empty() {
        // Bare `/cd` returns to the session root.
        *session_cwd = lexical_normalize(root_path);
        return;
    }
    match confine_under_root(root_path, session_cwd, arg) {
        Some(t) if t.is_dir() => *session_cwd = t,
        Some(t) => print_newt(
            &format!("cd: not a directory: {}", t.display()),
            color,
            verbose,
        ),
        None => print_newt(
            "cd: outside the session root (can't climb above it)",
            color,
            verbose,
        ),
    }
}

#[cfg(test)]
#[path = "lib_tests/cd_tests.rs"]
mod cd_tests;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn resolve_workspace(path: Option<&std::path::Path>) -> String {
    path.map(|p| p.to_string_lossy().into_owned())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|d| d.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "(unknown)".into())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Build a prompt-only [`Persona`] (no role bindings) for tests that only care
/// about the prompt overlay. Mirrors how a plain `.md` with no front-matter
/// loads. Top-level (not inside a test module) so every `#[cfg(test)] mod` can
/// reach it via `super::test_persona`.
#[cfg(test)]
fn test_persona(name: &str, prompt: &str, path: std::path::PathBuf) -> Persona {
    Persona {
        name: name.to_string(),
        prompt: prompt.to_string(),
        path,
        profile: newt_core::RoleProfile {
            prompt: prompt.to_string(),
            ..Default::default()
        },
    }
}

#[cfg(test)]
#[path = "lib_tests/core.rs"]
mod tests;

/// Regression tests for the `run_command` → agent-bridle confined-shell unification.
///
/// The headline property: the WHOLE command is confined under the granted
/// `Caveats`, not just the leading token. On the old leading-token + `sh -c`
/// path, `echo ok && rm -rf /` passed the `echo` check and then ran `rm`
/// directly. Routing through agent-bridle's brush interceptor closes that
/// bypass — every external spawn passes the leash's `before_exec` gate.
#[cfg(test)]
#[path = "lib_tests/run_command_confinement_tests.rs"]
mod run_command_confinement_tests;

/// INTERIM (#297) `--disable-ocap` / `--yolo` session-surfacing + bypass
/// tests — the TUI half of the escape hatch: the loud banner, the
/// `ocap-disabled` permission-log record, and the run_command bypass under
/// the same caveat shapes the confinement tests above pin for flag-off.
/// Removed with the bypass when brush upstreams CommandInterceptor
/// (agent-bridle#20).
#[cfg(test)]
#[path = "lib_tests/disable_ocap_session_tests.rs"]
mod disable_ocap_session_tests;

// ---------------------------------------------------------------------------
// ManagerNoteSink wiring (Step 19.3, #248) — `/remember` and the model's
// `save_note` tool must hit the SAME MemoryManager → NoteStore write path.
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "lib_tests/note_sink_wiring_tests.rs"]
mod note_sink_wiring_tests;

// ---------------------------------------------------------------------------
// Close-time note extraction (Step 19.4, #248) — one tools-disabled
// completion on /new + clean exit, writing through the scanned note path.
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "lib_tests/close_extraction_tests.rs"]
mod close_extraction_tests;

#[cfg(test)]
#[path = "lib_tests/skills_integration.rs"]
mod skills_integration_tests;

// ---------------------------------------------------------------------------
// Operating modes (`/mode`). These are behavior controls, never authority
// grants; permission floors remain the separate `/posture` concern below.
// ---------------------------------------------------------------------------
#[cfg(test)]
#[path = "lib_tests/operating_mode_tests.rs"]
mod operating_mode_tests;

// ---------------------------------------------------------------------------
// Named permission presets + `/posture` (issue #307). The `build_posture` core is
// pure (config + an injected skill loader), so the atomic preload-skill +
// apply-preset + framing contract is exercised here without a live session.
// ---------------------------------------------------------------------------
#[cfg(test)]
#[path = "lib_tests/posture_command_tests.rs"]
mod posture_command_tests;

// ---------------------------------------------------------------------------
// Context-window 400 recovery (issue #223) — the one agentic-loop test that
// stays TUI-side after Step 9.7 moved the loop suites to newt-core::agentic:
// it exercises the TUI's `recover_cw_400` hook (`recover_context_window_400`),
// plus the observation-owned probe-cache persistence that follows it.
// ---------------------------------------------------------------------------
#[cfg(test)]
#[path = "lib_tests/tool_round_cap_tests.rs"]
mod tool_round_cap_tests;

// ---------------------------------------------------------------------------
// fd_exhaustion_tests — verify O_CLOEXEC marking and EMFILE detection
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "lib_tests/terminal_probe_tests.rs"]
mod terminal_probe_tests;

#[cfg(all(test, unix))]
#[path = "lib_tests/fd_exhaustion_tests.rs"]
mod fd_exhaustion_tests;

// ---------------------------------------------------------------------------
// Pure / near-pure helper tests — no network, no env mutation, no real HOME
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "lib_tests/helper_fns.rs"]
mod helper_fn_tests;

// ---------------------------------------------------------------------------
// Persona helper tests — store edge cases + command plumbing
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "lib_tests/tenacity_indicator_tests.rs"]
mod tenacity_indicator_tests;

#[cfg(test)]
#[path = "lib_tests/persona_helper_tests.rs"]
mod persona_helper_tests;

// ---------------------------------------------------------------------------
// Env-var resolution tests — serialized behind a lock because the process
// environment is shared across the parallel test runner.
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "lib_tests/env_resolution.rs"]
mod env_resolution_tests;

// ---------------------------------------------------------------------------
// Model-listing HTTP tests against wiremock backends. (The streaming /
// overflow-retry / mid-loop-trim / final-summary / warm-up suites moved to
// newt-core::agentic with the loop in Step 9.7.)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "lib_tests/http_loop.rs"]
mod http_loop_tests;

// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 13:18 EDT | Date: 2026-08-12
