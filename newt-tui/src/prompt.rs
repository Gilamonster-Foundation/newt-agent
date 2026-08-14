/// Whether to show "newt" / "you" labels before the carets.
pub(crate) fn verbose_mode() -> bool {
    std::env::var("NEWT_CHAT_STYLE")
        .map(|v| v.eq_ignore_ascii_case("verbose"))
        .unwrap_or(false)
}

/// Whether per-round agent-loop diagnostics are enabled.
/// Set `NEWT_DEBUG=1` in the environment, or `[tui] debug = true` in config.
pub(crate) fn debug_mode(cfg: &newt_core::Config) -> bool {
    trace_mode(cfg)
        || std::env::var("NEWT_DEBUG").is_ok()
        || cfg.tui.as_ref().and_then(|t| t.debug).unwrap_or(false)
}

/// Whether deep backend/inference diagnostics are enabled.
/// Set `NEWT_TRACE=1` in the environment, or `[tui] trace = true` in config.
pub(crate) fn trace_mode(cfg: &newt_core::Config) -> bool {
    std::env::var("NEWT_TRACE").is_ok() || cfg.tui.as_ref().and_then(|t| t.trace).unwrap_or(false)
}

/// Resolve the edit mode from env (`NEWT_EDIT_MODE`) then config file, defaulting
/// to vi. The single source of truth for the lean and rich-tui surfaces (the
/// latter needs the `nano` distinction the others fold into emacs-style editing).
pub(crate) fn resolve_edit_mode() -> newt_core::EditMode {
    std::env::var("NEWT_EDIT_MODE")
        .ok()
        .and_then(|v| match v.to_lowercase().as_str() {
            "vi" | "vim" => Some(newt_core::EditMode::Vi),
            "emacs" => Some(newt_core::EditMode::Emacs),
            "nano" => Some(newt_core::EditMode::Nano),
            _ => None,
        })
        .or_else(|| {
            newt_core::Config::resolve()
                .ok()
                .and_then(|c| c.tui)
                .map(|t| t.edit_mode)
        })
        // vi is the default (no env, no `[tui] edit_mode`); `/nano` and `/emacs`
        // switch for those who want them.
        .unwrap_or(newt_core::EditMode::Vi)
}

/// Resolve the rich-tui gutter setting from env (`NEWT_GUTTER`) then config.
/// `None` = auto; `Some(0)` = off; `Some(n)` = an n-column input indent.
/// `NEWT_GUTTER` accepts `auto`, `off`, or a number; an unrecognized value
/// falls through to the config file. Only the rich-tui surface consumes this.
///
/// Default (nothing set in env or config): `Some(1)` — a single-space gutter
/// (stacked layout, prompt on its own row, input indented one column). The
/// user calls the continuation indent the "gutter" and wants it one space with
/// prompt-overhang acceptable; the old wide auto-gutter read as dead space.
/// Set `NEWT_GUTTER=auto` to restore the width-aware horizontal layout, or
/// `tui.gutter = N` in config for a fixed indent.
#[cfg(feature = "rich-tui")]
pub(crate) fn resolve_gutter_setting() -> Option<u16> {
    if let Ok(v) = std::env::var("NEWT_GUTTER") {
        match v.trim().to_lowercase().as_str() {
            "auto" => return None,
            "off" => return Some(0),
            s => {
                if let Ok(n) = s.parse::<u16>() {
                    return Some(n);
                }
                // Unrecognized value: ignore the env override, use config.
            }
        }
    }
    newt_core::Config::resolve()
        .ok()
        .and_then(|c| c.tui)
        .and_then(|t| t.gutter)
        // Unset anywhere → a 1-space gutter, not auto-wide.
        .or(Some(1))
}

/// Resolve the configured footer mode: `NEWT_FOOTER` env > `[tui].footer` > `Auto`.
pub(crate) fn footer_mode() -> newt_core::FooterMode {
    use newt_core::FooterMode;
    if let Ok(v) = std::env::var("NEWT_FOOTER") {
        match v.to_lowercase().as_str() {
            "off" | "plain" | "0" | "false" => return FooterMode::Off,
            "on" | "stamp" | "bar" | "1" | "true" => return FooterMode::On,
            "auto" => return FooterMode::Auto,
            _ => {}
        }
    }
    newt_core::Config::resolve()
        .ok()
        .and_then(|c| c.tui)
        .map(|t| t.footer)
        .unwrap_or_default()
}

/// Whether to use the rich default prompt (timestamp + status folded into the
/// prompt line) versus the plain bare prompt, from the configured mode + a TTY
/// probe. An explicit `[tui] prompt` overrides both. Pure for testing.
pub(crate) fn footer_rich_enabled(mode: newt_core::FooterMode, is_tty: bool) -> bool {
    use newt_core::FooterMode;
    match mode {
        FooterMode::Off => false,
        FooterMode::On => true,
        FooterMode::Auto => is_tty,
    }
}

/// Whether chat selects the RICH input surface — the gate the #1674 slash
/// palette (and every other rich-surface behavior) actually lives behind.
/// [`footer_rich_enabled`] ALONE does not protect a piped run:
/// `FooterMode::On` forces the rich PROMPT STRING even off a TTY (it lands in
/// logfiles by design). The rich SURFACE additionally requires stdout to be a
/// real terminal — this conjunction, used by `run_chat`'s surface selection,
/// is what keeps piped / headless runs on the lean surface. Pure for testing.
#[cfg(feature = "rich-tui")]
pub(crate) fn rich_surface_selected(mode: newt_core::FooterMode, stdout_is_tty: bool) -> bool {
    footer_rich_enabled(mode, stdout_is_tty) && stdout_is_tty
}

/// The built-in rich prompt template (used when `[tui] prompt` is unset and the
/// prompt is rich). Expands via [`expand_prompt_tokens`] to e.g.
/// `[2026-06-16 11:59:02] gpt-4.1 | emacs | newt-agent ❯ `. Written in the
/// readable `$NAME` form so it self-documents when copied into a config.
pub const DEFAULT_RICH_PROMPT: &str = "[$TIMESTAMP] $MODEL | $MODE | $WS ❯ ";

/// The built-in **lean** prompt template (issue #527): a timestamped server-log
/// line so the LeanTUI conversational stream doubles as a greppable log when
/// captured (`script`, tmux, a pipe). Used when `[tui] prompt` is unset and the
/// prompt is not rich (footer off / `-n` / `--plain` / piped).
pub const DEFAULT_LEAN_PROMPT: &str = "[$TIMESTAMP] ❯ ";

/// The prompt-token reference — the single source of truth shared by the
/// `/prompt` command, the scaffolded config comment, and the docs. Each entry
/// is `($NAME, \x, description)`.
pub const PROMPT_TOKENS: &[(&str, &str, &str)] = &[
    ("$TIMESTAMP", "\\t", "date + time, e.g. 2026-06-16 10:34:55"),
    ("$DATE", "", "date, e.g. 2026-06-16"),
    ("$TIME", "", "time, e.g. 10:34:55"),
    ("$MODEL", "\\m", "active model"),
    ("$MODE", "\\M", "edit mode (vi / emacs)"),
    ("$USER", "\\u", "username"),
    ("$HOST", "\\h", "hostname"),
    ("$WS", "\\w", "workspace basename"),
    ("$PATH", "\\W", "full workspace path"),
    ("$VERSION", "\\v", "newt version"),
];

/// Strip one matching pair of surrounding quotes (`"` or `'`), if present.
/// Preserves everything inside, including trailing spaces. Pure for testing.
pub(crate) fn strip_one_quote_pair(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Render [`PROMPT_TOKENS`] as aligned help lines (for `/prompt`).
pub(crate) fn prompt_token_help() -> Vec<String> {
    PROMPT_TOKENS
        .iter()
        .map(|(name, slash, desc)| format!("  {name:<11} {slash:<3}  {desc}"))
        .collect()
}

/// The active prompt template (`NEWT_PROMPT` > `[tui] prompt` > the built-in
/// default) and its live expansion, for `/prompt`'s preview line. Resolving the
/// model + edit mode here lets the preview show the *expanded* result so a
/// backslash/TOML escaping mistake is visible at a glance.
pub(crate) fn current_prompt_and_preview(workspace: &str) -> (String, String) {
    let template = std::env::var("NEWT_PROMPT")
        .ok()
        .or_else(|| {
            newt_core::Config::resolve()
                .ok()
                .and_then(|c| c.tui)
                .and_then(|t| t.prompt)
        })
        .unwrap_or_else(|| DEFAULT_RICH_PROMPT.to_string());
    let model = newt_core::Config::resolve()
        .ok()
        .map(|c| super::resolve_backend_choice(&c).model)
        .unwrap_or_default();
    let is_vi = resolve_edit_mode() == newt_core::EditMode::Vi;
    let preview = expand_prompt_tokens(&template, workspace, &model, is_vi);
    (template, preview)
}

/// Build the input-surface prompt string for this turn — PS1 tokens expanded, a
/// fresh timestamp each call.
///
/// Precedence: `NEWT_PROMPT` env > `[tui] prompt` config > built-in default.
/// The built-in default is the **rich** prompt ([`DEFAULT_RICH_PROMPT`] — the
/// timestamped status line) when `rich` is set, else the **lean** prompt
/// ([`DEFAULT_LEAN_PROMPT`] — `[ts] ❯ `, the server-log morphology, issue #527).
///
/// Tokens: `\t` timestamp, `\m` model, `\M` edit mode, `\u` user, `\h` host,
/// `\w` workspace basename, `\W` full path, `\v` newt version.
pub(crate) fn prompt_str(workspace: &str, is_vi: bool, model: &str, rich: bool) -> String {
    let template = std::env::var("NEWT_PROMPT").ok().or_else(|| {
        newt_core::Config::resolve()
            .ok()
            .and_then(|c| c.tui)
            .and_then(|t| t.prompt)
    });

    if let Some(ref tmpl) = template {
        return expand_prompt_tokens(tmpl, workspace, model, is_vi);
    }
    // Rich (footer on, TTY): the timestamped status line. Lean (#527: footer off
    // / `-n` / `--plain` / piped): a timestamped server-log line so the stream
    // doubles as a greppable log.
    let default = if rich {
        DEFAULT_RICH_PROMPT
    } else {
        DEFAULT_LEAN_PROMPT
    };
    expand_prompt_tokens(default, workspace, model, is_vi)
}

pub(crate) fn expand_prompt_tokens(
    template: &str,
    workspace: &str,
    model: &str,
    is_vi: bool,
) -> String {
    let ws_base = std::path::Path::new(workspace)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| workspace.to_string());
    let host = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "localhost".into());
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let now = chrono::Local::now();
    let timestamp = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let date = now.format("%Y-%m-%d").to_string();
    let time = now.format("%H:%M:%S").to_string();
    let mode = if is_vi { "vi" } else { "emacs" };
    let version = crate::VERSION;
    template
        // Readable `$NAME` macros. Longer names BEFORE their prefixes
        // (`$TIMESTAMP` before `$TIME`, `$MODEL` before `$MODE`) so a short
        // name never clobbers a longer one.
        .replace("$TIMESTAMP", &timestamp)
        .replace("$DATE", &date)
        .replace("$TIME", &time)
        .replace("$MODEL", model)
        .replace("$MODE", mode)
        .replace("$USER", &user)
        .replace("$HOST", &host)
        .replace("$VERSION", version)
        .replace("$PATH", workspace)
        .replace("$WS", &ws_base)
        // Terse `\x` PS1 tokens (bash-style; e.g. `\u@\h:\W`).
        .replace("\\W", workspace)
        .replace("\\w", &ws_base)
        .replace("\\h", &host)
        .replace("\\u", &user)
        .replace("\\t", &timestamp)
        .replace("\\m", model)
        .replace("\\M", mode)
        .replace("\\v", version)
}

#[cfg(test)]
mod footer_tests {
    use super::*;
    use newt_core::FooterMode;

    #[test]
    fn rich_follows_mode_and_tty() {
        // Auto → rich on a TTY, plain otherwise (the amphibious default).
        assert!(footer_rich_enabled(FooterMode::Auto, true));
        assert!(!footer_rich_enabled(FooterMode::Auto, false));
        // On forces rich even off a TTY; Off is always plain.
        assert!(footer_rich_enabled(FooterMode::On, true));
        assert!(footer_rich_enabled(FooterMode::On, false));
        assert!(!footer_rich_enabled(FooterMode::Off, true));
        assert!(!footer_rich_enabled(FooterMode::Off, false));
    }

    #[test]
    fn prompt_tokens_expand() {
        // The model + mode + workspace tokens fill in; `\t` is replaced (its
        // exact value is the clock, so just assert the literal token is gone).
        let p = expand_prompt_tokens("\\m · \\w · \\M", "/home/me/proj", "gpt-4.1", true);
        assert!(p.starts_with("gpt-4.1 · proj · vi"));
        let p = expand_prompt_tokens("[\\t] \\m", "/srv/x", "llama3", false);
        assert!(!p.contains("\\t"), "timestamp token expanded: {p}");
        assert!(p.ends_with("llama3"));
    }

    #[test]
    fn rich_default_prompt_renders_the_status_line() {
        // The built-in rich default expands to the `[ts · model · ws · mode ]`
        // prompt-prefix shape.
        let p = expand_prompt_tokens(DEFAULT_RICH_PROMPT, "/home/me/newt-agent", "gpt-4.1", false);
        assert!(p.contains("] gpt-4.1 | emacs | newt-agent ❯ "), "{p}");
        assert!(p.starts_with('['));
    }

    #[test]
    fn lean_default_prompt_is_a_timestamped_log_line() {
        // The built-in lean default (#527) expands to `[<ts>] ❯ ` — the
        // server-log morphology, no model/mode/ws status.
        let p = expand_prompt_tokens(DEFAULT_LEAN_PROMPT, "/home/me/newt-agent", "gpt-4.1", true);
        assert!(p.starts_with('['), "{p}");
        assert!(p.ends_with("] ❯ "), "{p}");
        assert!(!p.contains("$TIMESTAMP"), "timestamp token expanded: {p}");
    }

    #[test]
    fn strip_one_quote_pair_preserves_inner_spaces() {
        assert_eq!(strip_one_quote_pair("\"[$TIME] ❯ \""), "[$TIME] ❯ ");
        assert_eq!(strip_one_quote_pair("'hi'"), "hi");
        // Unquoted, mismatched, or too-short input is returned unchanged.
        assert_eq!(strip_one_quote_pair("bare"), "bare");
        assert_eq!(strip_one_quote_pair("\"oops'"), "\"oops'");
        assert_eq!(strip_one_quote_pair("\""), "\"");
    }

    #[test]
    fn dollar_macros_expand_without_prefix_clobber() {
        let p = expand_prompt_tokens("[$MODEL | $MODE | $WS]", "/home/me/proj", "gpt-4.1", true);
        assert_eq!(p, "[gpt-4.1 | vi | proj]");
        // $TIMESTAMP must win over its $TIME prefix; both tokens fully consumed.
        let p = expand_prompt_tokens("$TIMESTAMP|$TIME|$DATE", "/x", "m", false);
        assert!(
            !p.contains("$TIME") && !p.contains("$DATE") && !p.contains("STAMP"),
            "{p}"
        );
        // Every documented token has a working expansion (no literal left).
        for (name, _slash, _desc) in PROMPT_TOKENS {
            let out = expand_prompt_tokens(name, "/srv/work", "llama3", false);
            assert!(!out.contains(name), "token {name} not expanded: {out}");
        }
    }

    #[test]
    fn prompt_help_renders_every_documented_token() {
        let help = prompt_token_help();
        assert_eq!(help.len(), PROMPT_TOKENS.len());
        for ((name, slash, desc), line) in PROMPT_TOKENS.iter().zip(help.iter()) {
            assert!(line.contains(name), "{line}");
            assert!(line.contains(desc), "{line}");
            if !slash.is_empty() {
                assert!(line.contains(slash), "{line}");
            }
        }
    }

    #[test]
    fn workspace_basename_falls_back_to_full_path_when_absent() {
        let p = expand_prompt_tokens("$WS|\\w", "/", "m", true);
        assert_eq!(p, "/|/");
    }

    #[cfg(feature = "rich-tui")]
    #[test]
    fn backslash_continuation_is_bang_only() {
        // A `! …` host-shell line continues on a trailing backslash.
        assert!(crate::footer_continues("! date \\"));
        // A chat line submits on Enter even when it ends with `\` (literal).
        assert!(!crate::footer_continues("write a\\"));
        // Balanced input submits.
        assert!(!crate::footer_continues("write a function"));
        assert!(!crate::footer_continues(""));
        // The rejoin the REPL applies turns a `\`-break into a real newline.
        assert_eq!("foo\\\nbar".replace("\\\n", "\n"), "foo\nbar");
    }

    #[cfg(feature = "rich-tui")]
    #[test]
    fn triple_quote_block_stays_open_until_closed() {
        // `"""`/`'''` alone on line 1 opens a block; it stays open until a
        // matching closing fence appears on a later line.
        assert!(crate::footer_continues("\"\"\""));
        assert!(crate::footer_continues("\"\"\"\nline one"));
        assert!(crate::footer_continues("\"\"\"\nline one\nline two"));
        assert!(
            !crate::footer_continues("\"\"\"\nline one\n\"\"\""),
            "closing fence submits"
        );
        // `'''` works too, and mismatched fences don't close.
        assert!(crate::footer_continues("'''\nbody"));
        assert!(!crate::footer_continues("'''\nbody\n'''"));
        assert!(
            crate::footer_continues("'''\nbody\n\"\"\""),
            "mismatched fence stays open"
        );
        // A leading `"""` that is NOT alone on the first line is not a fence.
        assert!(!crate::footer_continues("\"\"\" inline text"));
    }
}
