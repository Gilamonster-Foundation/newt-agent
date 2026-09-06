//! The Codex-compat `OPENAI_*` environment: detect it, ask once, remember.
//!
//! `newt` can inherit an ambient OpenAI environment left by another tool.
//! Whether to adopt it is an operator decision, session-scoped or persisted
//! as the `~/.newt/openai-env.toml` drop-in. This module is that decision and
//! the `BackendChoice` it synthesizes; resolving a backend from it stays in
//! `super`.

use super::*;

/// Operator decision for the Codex-compat OPENAI_* environment (iteration #9):
/// "OPENAI env detected: use it? use/ignore/use-always/ignore-always".
///
/// `use`/`ignore` are session-scoped; the `-always` forms persist as the
/// drop-in `~/.newt/openai-env.toml` (`decision = "use-always" |
/// "ignore-always"` — the config law: core config stays lean, new knobs are
/// drop-ins; delete the file to be asked again). Non-interactive sessions
/// never prompt: they honor a stored `use-always` and otherwise ignore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexEnvDecision {
    UseIt,
    Skip,
}

/// Parse a stored decision file body. Unknown content → `None` (ask again),
/// never a silent yes.
fn parse_codex_env_decision(body: &str) -> Option<CodexEnvDecision> {
    for line in body.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some(value) = line.strip_prefix("decision") {
            let value = value
                .trim_start_matches([' ', '='])
                .trim()
                .trim_matches('"');
            return match value {
                // Canonical vocabulary + tolerated aliases.
                "use-always" | "always" | "use" => Some(CodexEnvDecision::UseIt),
                "ignore-always" | "never" | "ignore" => Some(CodexEnvDecision::Skip),
                _ => None,
            };
        }
    }
    None
}

fn codex_env_decision_path() -> Option<std::path::PathBuf> {
    newt_core::Config::user_config_path().map(|p| p.with_file_name("openai-env.toml"))
}

/// Resolve the operator's stance on the detected OPENAI_* env, prompting at
/// most once per process (OnceLock) and only on a TTY. `detected` names the
/// variables found, for the prompt line.
pub(crate) fn codex_env_allowed(detected: &str) -> bool {
    use std::io::IsTerminal;
    static DECISION: std::sync::OnceLock<CodexEnvDecision> = std::sync::OnceLock::new();
    *DECISION.get_or_init(|| {
        // Durable decision first.
        if let Some(path) = codex_env_decision_path() {
            if let Ok(body) = std::fs::read_to_string(&path) {
                if let Some(decision) = parse_codex_env_decision(&body) {
                    return decision;
                }
            }
        }
        if !std::io::stdin().is_terminal() {
            // Headless: only a stored `always` may adopt the env.
            return CodexEnvDecision::Skip;
        }
        // Through the seal (#1909). This function returns a decision rather
        // than a Result, so a refused or failed read falls through to the
        // `_ =>` arm below — "ignore this session", which is what the
        // non-interactive branch above already chooses. Fail-closed either
        // way, and now a protocol-mode process cannot obtain a speaking
        // window at all (#1908) rather than printing a question onto the wire.
        //
        // EOF (`Ok(0)`) is folded in deliberately: an operator who closes the
        // stream has not adopted the environment, and `_ =>` is exactly that.
        let window = newt_core::tty::Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::CodexEnvAdoption);
        let mut line = String::new();
        let _ = window
            .ask(&format!(
                "OPENAI env detected ({detected}): use it? \
                 [use/ignore/use-always/ignore-always] "
            ))
            .and_then(|()| window.read_line_into(&mut line));
        let answer = line.trim().to_ascii_lowercase();
        let (decision, persist) = match answer.as_str() {
            "use" | "u" | "y" | "yes" => (CodexEnvDecision::UseIt, None),
            "use-always" | "always" | "a" => (CodexEnvDecision::UseIt, Some("use-always")),
            "ignore-always" | "never" => (CodexEnvDecision::Skip, Some("ignore-always")),
            // "ignore", empty, or anything unrecognized: ignore this session.
            _ => (CodexEnvDecision::Skip, None),
        };
        if let (Some(value), Some(path)) = (persist, codex_env_decision_path()) {
            let body = format!(
                "# Written by newt: Codex-compat OPENAI_* env adoption.\n\
                 # \"use-always\" adopts silently; \"ignore-always\" ignores silently; delete to be asked again.\n\
                 decision = \"{value}\"\n"
            );
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, body);
        }
        decision
    }) == CodexEnvDecision::UseIt
}

/// Codex-parity environment resolution, pure for the mocked test tier: given
/// the raw `OPENAI_BASE_URL` / `OPENAI_API_KEY` / `OPENAI_MODEL` values,
/// synthesize an OpenAI-kind [`BackendChoice`] or decline.
///
/// - `OPENAI_BASE_URL` set (non-empty) → always fires: an explicit redirect,
///   usable against any OpenAI-compatible server (a lab llama.cpp router
///   included). A trailing `/v1` is trimmed — newt appends the wire path
///   itself, and the Codex convention includes `/v1` in the base URL.
/// - Only `OPENAI_API_KEY` set → fires ONLY when no `[[backends]]` are
///   configured (zero-config onboarding; never hijacks a configured setup).
/// - `OPENAI_MODEL` (else the session override) names the model; empty means
///   adopt() fills it from the served list at session start (#1126).
pub(crate) fn codex_env_backend(
    base_url: Option<&str>,
    api_key: Option<&str>,
    model: Option<&str>,
    session_model: Option<String>,
    have_configured_backends: bool,
) -> Option<BackendChoice> {
    let base_url = base_url.map(str::trim).filter(|s| !s.is_empty());
    let api_key = api_key.map(str::trim).filter(|s| !s.is_empty());
    let model = model.map(str::trim).filter(|s| !s.is_empty());
    let fires = base_url.is_some() || (api_key.is_some() && !have_configured_backends);
    if !fires {
        return None;
    }
    let url = base_url.unwrap_or("https://api.openai.com");
    let url = url.trim_end_matches('/');
    let url = url.strip_suffix("/v1").unwrap_or(url).to_string();
    let requested = model.map(str::to_string).or(session_model);
    Some(BackendChoice {
        api_key: api_key.map(str::to_string),
        api_needs_probe: true,
        // The env-supplied model is an operator REQUEST (an explicitly
        // selected identity), and the initial route until adoption decides.
        requested_model: requested.clone(),
        ..BackendChoice::synthesized("openai-env", url, newt_core::BackendKind::Openai, requested)
    })
}

#[cfg(test)]
#[path = "lib_tests/codex_env_tests.rs"]
mod codex_env_tests;
