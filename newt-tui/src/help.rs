//! Command help content and its plain/Markdown presentation.
//! Command recognition and dispatch remain with the session.

use newt_core::agentic::newt_line;

/// Map a typed command (incl. aliases) to its help topic. Several commands
/// share one page (the editor modes; the conversation-end trio).
pub(super) fn canonical_help_topic(cmd: &str) -> &str {
    match cmd {
        "quit" => "exit",
        "end" | "restart" | "clear" => "new",
        "allow" => "permissions",
        "plan" => "roadmap",
        "compact" => "compress",
        "vi" | "vim" | "emacs" | "nano" | "edit-mode" => "editor",
        "tool-rounds" | "max-rounds" => "rounds",
        _ => cmd,
    }
}

/// A single-page `--help` for one command — usage, what it does, and a couple of
/// examples. `None` for an unknown topic. Kept terse on purpose: newt's help is
/// a one-screen reference (the man-page-style browser is gilamonster's job).
pub(super) fn command_help_page(cmd: &str) -> Option<&'static str> {
    let page = match canonical_help_topic(cmd) {
        "models" => {
            "\
/models · /models capabilities — inspect the active endpoint's models

  /models                list models on the active endpoint, ◀ the active one
  /models capabilities   the matrix: Tool Use, Think (reasoning), Ctx Win,
                         Safe Ctx, tuning Conf, and tested date

Untested rows show '—'; classify one with /probe <model>. Per-model overrides
live in [model_tuning] (see /config)."
        }
        "model" => {
            "\
/model <name> — switch the model on the active backend

Changes the model newt talks to. The choice sticks across runs (saved to
~/.newt/settings.toml) but does not edit config; switching backends clears it.
Tab through what's installed with /models.
  /model qwen3:30b"
        }
        "backend" => {
            "\
/backend — an alias of /backends (#2048)

They were never two commands the operator should have had to tell apart, and
the slash registry always described them as one. See /help backends."
        }
        "backends" => {
            "\
/backends [name] — the backend panel, the text list, or a switch by name

Bare /backends on a rich interactive terminal opens the panel: ←/→ spins
through the configured backends, ◀ marks the active one; Enter applies the
pick; Esc leaves silently. `e` edits the selected entry, `a` adds one (name,
kind, url, model, api-key env/file — written crash-safe to
~/.newt/backends/<name>.toml), and `d` removes one after confirming, naming
what is lost and where from.

The text forms work everywhere (piped, lean build, headless):
  /backends            list every configured backend, ◀ the active one
  /backends dgx1       repoint this session at the 'dgx1' backend

A backend's WIRE KIND (openai / ollama) is a field on the backend itself,
edited with `e` in the panel. `/backend <kind>` used to set it globally,
detached from the endpoint it applied to — which is how a session ends up
pointed at an OpenAI-wire URL in Ollama mode. Pick a named backend that
already has the kind you want instead.

Your choice sticks across runs (~/.newt/settings.toml); an explicit
NEWT_PROVIDER or a --loadout still overrides it."
        }
        "crew" => {
            "\
/crew edit [name] — edit a crew's settings interactively

Prompts field-by-field (planner/navigator/triage loadouts, control loop, test
command, and budgets), previews the result, then writes it as a bare-Crew TOML
to ~/.newt/crews/<name>.toml. Enter keeps the [current] value; '-' clears an
optional field. Same form as `newt crew --edit`.
  /crew edit            edit the sole crew (or be prompted for a name)
  /crew edit home       edit (or create) the 'home' crew"
        }
        "thinking" => {
            "\
/thinking — folded into /settings (#2044)

  /settings                    open the form; the reasoning row is in it
  /settings thinking fold      (default) the first `/spill N` rows of
                               reasoning, then one line saying how long it
                               thought and what is behind the fold, reopenable
                               with the `/spill open <id>` it names
  /settings thinking stream    every line, unbounded — the historical trickle
  /settings thinking off       just the answer

`on` is an alias for `fold`. Reasoning is dimmed and sits above the answer
either way; TTY only. Persist with [tui] thinking in config."
        }
        "tenacity" => {
            "\
/tenacity — folded into /psyche (#1665)

  /psyche                open the dial panel (TTY); ←/→ dials tenacity
  /psyche tenacity <level>   set relaxed | standard | insistent | relentless
  /psyche tenacity auto      clear the override; inherit persona / config / family
  /psyche tenacity list      list every level, patient → forcing

Higher tenacity forces an edit after fewer read-only rounds and makes
exit_plan_mode require a concrete edit. The session-scoped override wins over
the persona declaration and [tenacity] config; `auto` (aliases `inherit` /
`reset`) releases it. Persist per-family in [tenacity]."
        }
        "cognition" => {
            "\
/cognition — folded into /psyche (#1665)

  /psyche                open the dial panel (TTY); ←/→ dials cognition
  /psyche cognition <level>  set glancing | pondering | deliberating | contemplating
  /psyche cognition off      send no reasoning controls (override any persona)
  /psyche cognition auto     follow the active persona's cognition (default)
  /psyche cognition list     list every level, light → deep

Responses maps the level to OpenAI reasoning.effort (glancing=minimal …
contemplating=high). Chat Completions maps it to local generation controls only
when the endpoint explicitly advertises that capability; unknown endpoints are
unchanged. The session override beats the active persona's cognition; a
persona sets its own default via `cognition:`."
        }
        "psyche" => {
            "\
/psyche — the agent's effort posture: cognition, tenacity, crew

  /psyche                open the dial panel (TTY): ↑↓ select, ←→ dial,
                         Enter apply, Esc leave (nothing changed → just exits)
                         rows: persona · model (spins the backend's served
                         models, applied via the /model path) · cognition ·
                         tenacity
  /psyche status         the read-only text view (also what piped/lean gets)
  /psyche cognition <level|off|auto|list>   text setter for the cognition dial
  /psyche tenacity <level|auto|list>        text setter for the tenacity dial
  /psyche obsessive      engage the max-everything posture's live dials

The three orthogonal psyche dials:
  cognition   backend-specific reasoning depth per call
  tenacity    how hard the loop pushes read → act
  crew        how many minds work the task    (NEWT_TEAM / newt crew)

obsessive = contemplating + relentless + crew on — newt's 'ultra'. In-session
/psyche obsessive sets cognition + tenacity live; crew is a launch gate, so
start with `newt --obsessive` to include the crew this session."
        }
        "probe" => {
            "\
/probe [model|all] · /probe window <model> · /probe reset

Classify models empirically; results feed /models capabilities.
  /probe <model>        warm up, then test: tool conformance, context window,
                        thinking quirk, token calibration
  /probe                probe the active model
  /probe all            RE-probe every model on the endpoint (a long sweep —
                        press Esc to cancel; finishes the current model first)
  /probe window <model> empirical input-boundary search (max input at High conf)
  /probe reset          wipe all learned values (conformance, windows,
                        calibration) so the next /probe re-learns from scratch"
        }
        "memory" => {
            "\
/memory — show context-window and notes usage

Read-only: how full the context window is, persistent NOTES usage, and the
session compression counters. Add facts with /remember; compact with /compress."
        }
        "compress" => {
            "\
/compress [focus] — compress the conversation context now

Summarize-and-prune the in-flight context to reclaim window, optionally biased
toward a topic. Runs automatically when the window fills; this forces it early.
  /compress
  /compress the auth refactor"
        }
        "summarizer" => {
            "\
/summarizer [subcommand] — inspect or manage the mid-loop summarizer

  /summarizer                  show the effective backend + knobs
  /summarizer setup [alias]    provision the default/named embedded mini-model
  /summarizer embedded [alias] pin an explicit embedded summarizer override
  /summarizer fallback <m>     set fallback_model (use 'none' to clear)
  /summarizer timeout <secs>   set timeout_secs
  /summarizer retries <n>      set retries
  /summarizer keep-alive <v>   set keep_alive (use 'none' to clear)
  /summarizer clear            remove summarizer.toml, return to built-in default

This is the interactive wrapper around `newt summarizer ...`."
        }
        "rounds" => {
            "\
/rounds [show|<n>|double|reset|config|unlimited] — session tool-call round limit

Human-only override for how many tool-call rounds the agent may run in a
single turn. It does not edit config and lasts only for this session.
  /rounds             show the effective limit
  /rounds 50          allow 50 tool-call rounds per turn
  /rounds double      double the current effective limit
  /rounds reset       clear the override; derive from tenacity + config/model
                      (`default` and `auto` are aliases)
  /rounds config      use config/model tuning even under relentless tenacity
  /rounds unlimited   raise to at least 10000 rounds (effectively run-until-finished)

Aliases: /tool-rounds, /max-rounds."
        }
        "remember" => {
            "\
/remember <fact> — add a fact to persistent NOTES.md

Writes a durable note the agent carries across turns and sessions (workspace
NOTES). Survives /new. View usage with /memory.
  /remember the staging DB is read-only"
        }
        "new" => {
            "\
/new · /end · /restart · /start — begin a new conversation

/new, /end, and /restart FINALIZE the current conversation (its summary is
extracted to memory) and start a fresh one, staying in the session. /start
switches to a fresh one too but leaves the previous conversation OPEN so you can
/resume it. /start <title> and /rename <title> name a conversation so it is easy
to find in /resume. Nothing auto-resumes on next launch (#1030) — use /resume to
reopen a past conversation. /exit · /quit · vi :wq leave the session."
        }
        "conversation" => {
            "\
/conversation <sub> — manage saved conversations

  /conversation list              list saved conversations
  /conversation show <id>         print one
  /conversation restore <id>      switch the session to it
  /conversation rename <id> <t>   retitle it
  /conversation delete <id>       delete it (alias: rm)

ids accept a unique prefix. Search bodies with /recall."
        }
        "recall" => {
            "\
/recall [query] — browse or search past conversations

  /recall            recent conversations in this workspace
  /recall <query>    full-text search across this workspace's turns

Read-only and workspace-fenced. Bring one back with /conversation restore <id>
or /resume."
        }
        "resume" => {
            "\
/resume [query|n|id] — find and REOPEN a past conversation (#1030)

  /resume            list recent conversations, annotated by liveness
  /resume <query>    full-text search this workspace's turns
  /resume <n>        reopen the n-th row from the last listing
  /resume <id>       reopen by id or unique prefix

Markers: ▶ current · ● open in another newt · ○ resumable. Reopening a
conversation another live newt holds is refused (it would mix turns) —
this is how #1030 keeps multiple newts from colliding."
        }
        "transcript" => {
            "\
/transcript — review the WHOLE current conversation (#1670)

On the RICH surface: a full-screen pager where the conversation spine —
your › prompts and the model's ▸ replies — is the structure. Scroll
freely; the grey per-turn tool blocks are folded behind ⚙ headers.

  q / Esc          leave the pager (the screen is restored)
  ↑↓ / j k         scroll by line        PgUp/PgDn      page
  Ctrl-U / Ctrl-D  half page             g / G          top / bottom
  n / p            next / previous message (jump the spine)
  Enter/Space/Tab  fold or unfold the current turn's tool block

On the LEAN surface the same command PRINTS the spine into scrollback —
the plain scroller has no scroll regions, by charter. Tool detail shown
is the stored summary (name · ok · duration); raw tool output is never
persisted."
        }
        "roadmap" => {
            "\
/roadmap [sub] — manage the per-session planning roadmap

  /roadmap list             list open roadmaps
  /roadmap show             render the active roadmap tree
  /roadmap new              create a new roadmap
  /roadmap use <n>          bind a roadmap by number
  /roadmap add <title>      add a roadmap item
  /roadmap task <n>         show one task
  /tree                     render the active roadmap tree (alias of /roadmap show)

Alias: /plan"
        }
        "persona" => {
            "\
/persona <sub> — configured personas

  /persona list           list configured personas
  /persona show           show the active persona
  /persona <name>         start a fresh conversation with that persona
  /persona switch <name>  same as /persona <name> (an explicit verb)
  /persona clear          start fresh with no persona

Setting or clearing a persona starts a new conversation (the system prompt
changes). Define personas in config."
        }
        "dgx" => {
            "\
/dgx <sub> — NVIDIA DGX endpoint operations

  /dgx status       endpoint health + currently-loaded models
  /dgx models       models installed on the DGX
  /dgx ps           models currently loaded in VRAM
  /dgx warm [model] pre-load a model into VRAM (cuts first-token latency)
  /dgx pull <model> pull an Ollama or HuggingFace GGUF model onto the node
  /dgx rm <model>   delete a model from the DGX
  /dgx route <task> recommend a formation for a task
  /dgx doctor       probe every configured endpoint

  Note: flags like --dry-run/--force/--name are CLI-only; use
  `newt dgx pull ...` from a shell for the full pull workflow."
        }
        "permissions" => {
            "\
/permissions — review prompted permission decisions + the active posture

Read-only: what you've allowed/denied this session and the posture's optional
authority floor, when configured. Durable grants are made by editing
[tui.permissions] in config, not here.

Usage:
  /permissions                overview of this session's prompt flow
  /permissions audit [N]      newest N audit rows from the persisted permission log
  /allow                      alias for /permissions

Examples:
  /permissions                # show current session decisions
  /permissions audit 25       # show newest 25 rows from permission-log.jsonl
  /allow                      # alias for /permissions"
        }
        "status" => {
            "\
/status — show session status and environment summary

  workspace, backend, mode, posture, permissions state, and active identifiers.

Tip: use /info for a slightly richer version, and /permissions for full
prompted-decisions history."
        }
        "info" => {
            "\
/info — show machine-readable context for the current session

Shows the same status surface as /status, plus the version, active model
identity, and resolved backend details that drive this prompt.
        "
        }
        "byline" => {
            "\
/byline — show the Co-authored-by block the next commit would carry

  Read-only. Prints the exact trailer block, in commit order:

    every model that has contributed since the last commit (a /model,
    /backend, loadout, or crew switch ADDS one — it never replaces one),
    then the model driving this turn, then the human operator when a real
    email is known, then the Harness: provenance line.

  The email in a trailer identifies the HARNESS, not the model, so a
  harness signs its own address and never another's. Under newt's own
  address the account name is not repeated: the qualifier carries only
  what the address does not already say — `(v<version> <build>)`, or
  `(crew v<version> <build>)` for a crew leaf. A foreign harness keeps its
  full name, e.g. `(Claude Code v2.1.239)`.

  The operator by-line comes from your git identity (user.name +
  user.email). If no real email is known it is OMITTED, never invented —
  so a missing operator line means \"unknown\", not \"nobody\".

  This is rendered by the same finalizer the commit path runs, so it
  cannot show a shape a commit would not produce. Do not hand-write these
  trailers: the `git` tool stamps them itself."
        }
        "docs" => {
            "\
/docs — open the right docs quickly

  GitHub README: https://github.com/Gilamonster-Foundation/newt-agent
  issue tracker: https://github.com/Gilamonster-Foundation/newt-agent/issues
  architecture docs: https://github.com/Gilamonster-Foundation/newt-agent/tree/main/docs

Use /help for the in-session command list."
        }
        "mcp" => {
            "\
/mcp — manage MCP servers for this session

  /mcp                         status of every discovered server
  /mcp off [name]              mute this session (tools leave the catalog now;
                               connection stays — /mcp on restores instantly)
  /mcp on [name]               unmute this session (bare = unmute all)
  /mcp disable <name>          durable: write enabled=false to config + drop now
  /mcp enable <name>           durable: write enabled=true (connects next launch;
                               live reconnect is #1148)
  /mcp auth <name>             how to (re)authenticate (`newt auth <name>`)

on/off is session-scoped (like /nudge) — use it while testing schema budget.
enable/disable rewrites ~/.newt/config.toml."
        }
        "mode" => {
            "\
/mode [name] — show or choose the session's operating mode

  /mode              show the active mode and describe every available mode
  /mode list         same as bare /mode
  /mode show         show only the active mode
  /mode <name>       select chat, dev, admin, plan, diagnose, auto, or full-auto
  /mode reset        return to chat (the default)

Modes guide working style. Plan may update Newt's plan ledger but cannot mutate
the workspace; diagnose is bounded read-only research. In Auto, the model may
select chat, dev, admin, plan, or diagnose for a later action-shaped turn;
protected intake still wins, and only the human can select full-auto. No mode
grants authority or bypasses the active permission posture."
        }
        "posture" => {
            "\
/posture [name] — show or choose a configured permission posture

  /posture              show the active posture and configured names
  /posture list         same as bare /posture
  /posture show         show only the active posture
  /posture status       same as /posture show
  /posture <name>       preload skill/framing and apply its optional preset floor
  /posture off          clear the active posture
  /posture clear

Configured postures continue to use [modes.<name>] entries for compatibility.
A configured preset can only NARROW authority, never widen it; a posture with
no preset leaves authority unchanged."
        }
        "loadout" => {
            "\
/loadout — show the active loadout

Prints the declared axes (backend, model, persona, mode, …) and what each
actually resolved to, so you can see why the session is configured as it is."
        }
        "workspace" => {
            "\
/workspace — print the current workspace path

The workspace fences conversations, recall, and NOTES. It's the directory newt
was launched in unless overridden."
        }
        "spill" => {
            "\
/spill [status|N|reset|summary|excerpt|last|open ID] — tool-output controls

  /spill                 show the effective row count and live availability
  /spill <N>             set collapsed live and completed rows for later tools
  /spill reset           return to the configured [tui] spill_lines value
  /spill 0               disable live display; show completed output unbounded
  /spill summary         collapse spilled results to one line (rich default)
  /spill excerpt         restore the multi-row excerpt for spilled results
  /spill last            open the newest retained result (rich, this session)
  /spill open <ID>       open the retained result named by a collapse marker

While a tool is active, Up/Down scroll retained output. Space or Enter toggles
the boundary: ⧉ expands up to the terminal's safe capacity; ▣ collapses it.
Completed bodies are memory-only and bounded; older IDs can expire."
        }
        "config" => {
            "\
/config — dump the resolved configuration (secrets redacted)

Shows the effective config after merging /etc/newt, ~/.newt, and ./.newt — the
source of truth for backends, loadouts, model tuning, and [tui] settings.
api_key_file/env values are redacted."
        }
        "prompt" => {
            "\
/prompt · /prompt set \"<tmpl>\" · /prompt reset — customize the input prompt

  /prompt                  list tokens ($MODEL/$DATE/…, \\m/\\t/\\M/…) + current
  /prompt set \"<template>\"  set the prompt for this session
  /prompt reset            revert to [tui] prompt / the built-in default

Tokens: \\t time · \\m model · \\M edit mode · \\w workspace · \\u user · \\h host ·
\\v version. Persist by putting a template in [tui] prompt (prefer the $NAME
macros there to dodge TOML escaping)."
        }
        "editor" => {
            "\
/vi · /emacs · /nano — switch line-editor key bindings for this session

  /vi      modal vi keys (Esc=NORMAL; i/a/o insert; :w send, :wq send+end+quit)
  /emacs   emacs/readline keys (Enter sends; Ctrl-O newline; C-x C-c exit)
  /nano    nano-style (Enter sends; ^X exit; ^G help)

Persist with [tui] edit_mode. Press Ctrl-h/^G/:help in-editor for the cheatsheet."
        }
        "version" => {
            "\
/version — print the newt-agent version."
        }
        "exit" => {
            "\
/exit · /quit (or bare exit/quit, Ctrl-D) — leave the session

Ends the session. Conversations do NOT auto-resume on next launch (#1030): each
launch starts fresh — use /resume to reopen a past conversation. (Opt into
auto-resuming the folder's latest with [conversations] resume = true.)"
        }
        "help" => {
            "\
/help [command] — command help

  /help            list every command
  /help <command>  this page for one command (same as /<command> --help)

Add --help (or -h) to any command for its page."
        }
        _ => return None,
    };
    Some(page)
}

/// Render one command's `--help` page to a `String`; `bool` is `true` when a
/// page exists. Unknown topics render a one-line miss (so a typo doesn't fall
/// through to the wrong handler) and return `false`.
///
/// This is the single byte-source for a plain per-command page. The interactive
/// TUI derives its Markdown document from the same [`command_help_page`]
/// corpus, while the startup-free CLI routes through [`render_help`].
fn command_help_output(cmd: &str, color: bool, verbose: bool) -> (String, bool) {
    match command_help_page(cmd) {
        Some(page) => {
            let mut out = newt_line(
                &format!("/{} help", canonical_help_topic(cmd)),
                color,
                verbose,
            );
            out.push('\n');
            for line in page.lines() {
                out.push_str(line);
                out.push('\n');
            }
            (out, true)
        }
        None => {
            let mut out = newt_line(
                &format!("no help for '/{cmd}' — /help lists every command"),
                color,
                verbose,
            );
            out.push('\n');
            (out, false)
        }
    }
}

/// Render the bare-`/help` command list to a `String`.
///
/// The plain top-level list: the `Available commands:` narrator line followed
/// by every [`help_lines`] entry. The interactive TUI derives its Markdown
/// document from that same corpus; plain mode and the startup-free CLI
/// ([`render_help`]) route through this function.
fn help_list_output(color: bool, verbose: bool) -> String {
    let mut out = newt_line("Available commands:", color, verbose);
    out.push('\n');
    for line in help_lines() {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Markdown source for RichTUI's bare command catalog. The long-standing
/// [`help_lines`] corpus remains the single source of truth; this only gives
/// each row Markdown structure so the renderer preserves command boundaries
/// instead of folding their soft line breaks into one paragraph.
fn help_list_markdown() -> String {
    let mut out = String::from("## Available commands\n\n");
    for line in help_lines() {
        let line = line.trim();
        if line.is_empty() {
            out.push('\n');
        } else if let Some((usage, description)) = line.split_once(" - ") {
            out.push_str(&format!("- `{usage}` — {description}\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Markdown source for one command's detail page.
fn command_help_markdown(cmd: &str) -> Option<String> {
    let page = command_help_page(cmd)?;
    let mut out = format!("## /{} help\n\n", canonical_help_topic(cmd));
    for line in page.lines() {
        if line.starts_with("  ") {
            out.push_str("- ");
            out.push_str(line.trim());
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    Some(out)
}

/// Render help for the interactive TUI. Markdown mode is deliberately a
/// presentation layer over the existing corpus: disabling it returns the
/// byte-identical plain/startup-free output from [`render_help`].
pub(super) fn render_help_for_tui(
    topic: Option<&str>,
    color: bool,
    verbose: bool,
    markdown: bool,
    cols: usize,
) -> String {
    if !markdown {
        return render_help(topic, color, verbose);
    }
    let source = match topic {
        None => help_list_markdown(),
        Some(cmd) => match command_help_markdown(cmd) {
            Some(source) => source,
            None => return render_help(topic, color, verbose),
        },
    };
    let rendered = newt_core::agentic::render_markdown(
        &source,
        newt_core::agentic::RenderOpts { color, cols },
    );
    format!("{}{rendered}\n", newt_line("", color, verbose))
}

/// Print one command's `--help` page; `true` when a page exists.
pub(super) fn print_command_help(cmd: &str, color: bool, verbose: bool, markdown: bool) -> bool {
    let found = command_help_page(cmd).is_some();
    print!(
        "{}",
        render_help_for_tui(
            Some(cmd),
            color,
            verbose,
            markdown,
            newt_core::tty::term_cols(),
        )
    );
    found
}

/// Render newt's command help WITHOUT starting a session or connecting to a
/// backend. `topic == None` is the bare-`/help` command list; `Some(cmd)` is
/// that command's detail page (an unknown topic renders the one-line miss).
///
/// This is the startup-free entry point behind `newt help [command]` and the
/// interactive TUI's plain-render fallback. [`help_lines`] and
/// [`command_help_page`] remain the single source of truth for WHAT help says
/// (issue #548 measures that content); RichTUI only adds a Markdown
/// presentation over those corpora.
pub fn render_help(topic: Option<&str>, color: bool, verbose: bool) -> String {
    match topic {
        None => help_list_output(color, verbose),
        Some(cmd) => command_help_output(cmd, color, verbose).0,
    }
}

pub(crate) fn help_lines() -> &'static [&'static str] {
    &[
        "  /model <name>            - switch model on the active backend (sticks across runs)",
        "  /backends [name]         - backend panel on a rich TTY (choose · edit · add · remove); text: list, or switch by name",
        "  /backend                 - alias of /backends",
        "  /settings [field value]  - the settings form: edit-mode + effort dials + rounds; every applied change writes a receipt (#1981)",
        "  /probe [model|all]       - classify tool use, context window, thinking, calibration (all = re-probe every model; Esc cancels)",
        "  /probe window [model]    - empirical input-boundary search (records max input at High confidence)",
        "  /probe reset             - wipe all learned probe values (conformance, windows, calibration)",
        "  /compress [focus]        - compress context now, optionally focused on a topic (alias: /compact)",
        "  /summarizer              - show or change the summarizer backend and knobs",
        "  /rounds [n|double|reset|config|unlimited] - set this session's tool-call round limit",
        "  /context                 - show the active context manager + features",
        "  /context manager [preset] - show or set the strategy preset (standard; progressive/distributed pending #546)",
        "  /context feature <name> [on|off] - toggle a composable context feature (all pending #582-#586)",
        "  /context compaction [headroom_aware|message_count|reset] - set this session's automatic-compaction trigger policy",
        "  /context stats           - experimentation dashboard: budget, compression, feature states",
        "  /search <query>          - semantic code search cockpit (#1387): preview · model · rejects · pin · exclude · status",
        "  /remember <fact>         - add a fact to persistent NOTES.md",
        "  /new                     - finalize this conversation and start a fresh one (stays in the session; alias: /clear)",
        "  /end                     - the same, recorded as ended by /end rather than /new (it no longer exits — use /exit)",
        "  /restart                 - the same, recorded as a restart",
        "  /start [title]           - begin a new conversation, leaving the current one open to /resume",
        "  /resume [name|search|n|id] - find & reopen a past conversation: bare lists recent, then match by name/title, id, or full-text search",
        "  /resume find [query]     - search conversations WITHOUT reopening one (bare: browse)",
        "  /resume list             - list saved conversations",
        "  /resume show <id>        - show a saved conversation",
        "  /resume restore <id>     - restore a saved conversation",
        "  /resume rename <id> <title> - rename a saved conversation",
        "  /resume delete <id>      - delete a saved conversation (asks first; alias: rm)",
        "  /name <title>            - retitle the current conversation so it is easy to find in /resume (alias: /rename)",
        "  /transcript              - review this conversation: full-screen pager (rich) / printed spine (lean)",
        "  /roadmap [sub]           - #1030 plan tree: new·list·show·use·add · next·bind·done·eval·drive · task <n> commit [sha] · issue <n> <#> · export·import [path]",
        "  /roadmap tree            - render the active roadmap tree (▶ marks the next-ready node / DFS cursor)",
        "  /persona list            - list configured personas",
        "  /persona show            - show the active persona",
        "  /persona <name>          - start fresh with a persona",
        "  /persona switch <name>   - same as /persona <name> (an explicit verb)",
        "  /persona clear           - start fresh with no persona",
        "  /crew edit [name]        - edit a crew's settings (roles, control loop, test, budgets)",
        "  /setup [host]            - configure an inference backend (wizard, or probe a host); \
         pasted keys are stored encrypted",
        "  /dgx status              - DGX endpoint health + running models",
        "  /dgx models              - list models installed on the DGX",
        "  /dgx ps                  - models currently loaded in VRAM",
        "  /dgx warm [model]        - pre-load a model into VRAM",
        "  /dgx pull <model>        - pull an Ollama/HuggingFace GGUF model onto the node",
        "  /dgx rm <model>          - delete a model from the DGX",
        "  /dgx route <task>        - recommend a formation for a task",
        "  /dgx doctor              - probe every configured endpoint",
        "  /mode [name]             - show/set operating style: chat, dev, admin, plan, diagnose, auto, full-auto",
        "  /posture [name]          - show/set configured posture; permission floor is optional",
        "  /permissions             - prompted decisions + active permission posture",
        "  /status                  - session and environment summary (the default view)",
        "  /status <topic>          - config · version · workspace · loadout · byline · memory · models · info",
        "  /dock [status|disable|enable] - remote-HTMX docking kill-switch (req 7): disable forcibly undocks THIS box from every hub; status lists approved peers",
        "  /allow                   - alias for /permissions",
        "  /nudge <on|off|status>   - action-pressure nudges (narration rescue etc.); off = answer-in-peace mode",
        "  /psyche                  - effort dial panel: cognition, tenacity, persona (Esc exits; /psyche obsessive = max)",
        "  /mcp [on|off|enable|disable|auth] [name] - MCP servers: session mute (on/off) or durable config (enable/disable)",
        "  /spill [status|N|reset|summary|excerpt|last|open ID] - tool output",
        "  /prompt                  - list prompt tokens ($MODEL, $DATE, …) + current prompt",
        "  /prompt set \"<template>\"  - set the prompt for this session; /prompt reset to revert",
        "  /vi  /emacs  /nano       - switch line-editor key bindings for this session",
        "  ! <command>              - run a host command interactively (e.g. ! pa login) — you, not the agent",
        "  /cd [dir]                - change the session working dir (shown in prompt), confined below the start dir; bare /cd returns to the root — use ! for pwd/ls/rm/…",
        "  Esc                      - while the agent is working: interrupt the turn, back to your prompt",
        "  Up/Down                  - while a tool is active: scroll its retained output",
        "  Space/Enter              - while a tool is active: toggle ⧉ expand / ▣ collapse",
        "  /search [query|preview|model|rejects|pin|exclude|status|clear] - #1387 semantic search cockpit",
        "  /nav                     - the code navigator; bare lists its verbs",
        "  /nav def <symbol>        - goto definition ([SYMBOL])",
        "  /nav text <regex>        - lexical search ([LEXICAL])",
        "  /nav uses <symbol>       - find references (usage index)",
        "  /nav tests <symbol>      - related tests (heuristic)",
        "  /nav map [unit]          - project map; optional expand unit",
        "  /nav callers <symbol>    - inbound edges (GRAPH regex-floor)",
        "  /nav callees <symbol>    - outbound edges (GRAPH regex-floor)",
        "  /nav implementations <symbol> - implementors (GRAPH regex-floor)",
        "  /nav hierarchy <symbol>  - type hierarchy (GRAPH regex-floor)",
        "  /nav type <symbol>       - inspect_type (not typechecker-proved)",
        "  /nav impact <unit>       - outbound/reverse deps (+ optional lcov)",
        "  /nav retrieval [turn N] [human|model|diff] - retrieval ledger",
        "  /nav compare <what>      - compare retrieval: semantic lexical · turn A B · index",
        "  /nav export <json|markdown> - export retrieval ledger",
        "  /help [command]          - this command list, or one command's detail page",
        "  /exit  /quit  exit  quit - leave the session",
        "",
        "  Add --help (or -h) to any command — or /help <command> — for its detail page.",
    ]
}
