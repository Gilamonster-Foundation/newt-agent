//! The shell-engine axis (ADR 0005 D2) + its confinement gating — carved from
//! `config.rs` (kernel-first decomposition, handoff §D, carve 2/3; follows the
//! `config/tools.rs` sibling precedent). Everything here re-exports through
//! `config.rs`, so both `newt_core::ShellEngine` and
//! `newt_core::config::ShellEngine` remain byte-identical paths for consumers
//! (newt-cli doctor, the TUI, newt-mcp-client).
//!
//! The 12 shell-engine tests live HERE, beside the resolution chain they pin —
//! parse aliases, precedence order, and the L3 gate must fail in this file.

use serde::{Deserialize, Serialize};

/// Which shell **engine** interprets `run_command` — the ADR 0005 D2 seam. This
/// is the *engine* axis (what parses/runs the command line); the *L3 backend*
/// axis (Landlock/Seatbelt/AppContainer, the kernel fence) is auto-selected
/// per-OS and is **not** chosen here. "Landlock vs brush" is really "the `host`
/// engine (guarantee rests entirely on the kernel fence) vs the `brush` engine
/// (L2 interceptor confines in-process, with the fence as an added backstop)."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ShellEngine {
    /// bridle's argv + safe-subset parser: refuses `$(...)`/backticks/dynamic
    /// constructs by design and spawns argv directly. Portable default.
    #[default]
    SafeSubset,
    /// The sandboxed-host engine (ADR 0019): real `/bin/sh -c` with the whole
    /// process tree in the L3 jail. Full grammar; the guarantee rests entirely
    /// on the kernel fence. Refuses a *restricted* `exec`/`net` grant. Needs a
    /// host `/bin/sh`. `--full-access` auto-selects this.
    Host,
    /// The carried brush engine (bash-in-Rust + the L2 `CommandInterceptor`):
    /// full grammar, in-process, cross-platform, and the only engine that also
    /// confines a *restricted* `exec`/`net` grant. Requires the `brush` build
    /// (agent-bridle#20 / Track 2); until that ships, selecting `brush` falls
    /// back to `host` with a warning.
    Brush,
}

impl ShellEngine {
    /// The canonical config/flag token (`kebab-case`).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SafeSubset => "safe-subset",
            Self::Host => "host",
            Self::Brush => "brush",
        }
    }
}

impl std::fmt::Display for ShellEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ShellEngine {
    type Err = String;

    /// Parse a shell-engine token. Accepts the canonical names plus intuitive
    /// aliases (`subset`/`safe`, `sandbox-host`/`landlock`, `brush-ocap`), so a
    /// user thinking in "landlock vs brush-ocap" terms still resolves correctly.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "safe-subset" | "subset" | "safe" => Ok(Self::SafeSubset),
            "host" | "sandbox-host" | "landlock" | "seatbelt" => Ok(Self::Host),
            "brush" | "brush-ocap" => Ok(Self::Brush),
            other => Err(format!(
                "unknown shell engine '{other}' (expected one of: safe-subset, host, brush)"
            )),
        }
    }
}

/// `[shell]` — engine selection for `run_command`. `engine = None` (the field
/// unset) is deliberately distinct from an explicit choice: an unset engine lets
/// `[intake]` — operator overrides for prompt-disposition inference (#1260).
///
/// Disposition inference is three needle lists + a trailing-`?` fallback,
/// held as pure data ([`crate::agentic::DispositionLexicon`]). This table
/// lets an operator retune it without a code change — the three-Cs shape.
/// Each present list REPLACES its built-in default wholesale (droppable,
/// predictable — no merge ambiguity); an absent list keeps the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IntakeConfig {
    /// Needles that force an **Act** turn (full catalog). Replaces the default
    /// list when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Vec<String>>,
    /// Needles classifying **Research** (bounded evidence loop). Replaces the
    /// default list when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research: Option<Vec<String>>,
    /// Needles classifying **Explain** (read-only evidence set). Replaces the
    /// default list when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explain: Option<Vec<String>>,
    /// Where a prompt matching NO list but ending in `?` lands:
    /// `"explain"` (default), `"research"`, or `"act"` — the #1257 fallback
    /// cliff, made explicit and tunable. Unknown values fall back to explain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_mark_disposition: Option<String>,
}

impl IntakeConfig {
    /// Resolve this table over the built-in defaults into the lexicon
    /// [`crate::agentic::PromptIntake::analyze_with`] consumes.
    #[must_use]
    pub fn to_lexicon(&self) -> crate::agentic::DispositionLexicon {
        let mut lex = crate::agentic::DispositionLexicon::default();
        if let Some(list) = &self.action {
            lex.action = list.clone();
        }
        if let Some(list) = &self.research {
            lex.research = list.clone();
        }
        if let Some(list) = &self.explain {
            lex.explain = list.clone();
        }
        match self.question_mark_disposition.as_deref() {
            Some("research") => {
                lex.question_mark_disposition = crate::agentic::PromptDisposition::Research;
            }
            Some("act") => {
                lex.question_mark_disposition = crate::agentic::PromptDisposition::Act;
            }
            // "explain", unset, and unknown values all keep the default —
            // fall back predictably rather than erroring at config load.
            _ => {}
        }
        lex
    }
}

/// `--full-access` auto-upgrade to `host` (see [`resolve_shell_engine`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    /// The selected engine, or `None` to accept the context default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<ShellEngine>,
    /// Env vars passed into the confined `run_command` shell. The confined shell
    /// gets no ambient shell variables, so without this `~` cannot expand (brush
    /// resolves `~` from its `HOME` shell var) — which silently produced literal
    /// `~/…` paths. Minimal by default (`HOME`, `USER`) so a secret in the
    /// process env (API keys, tokens) never leaks into a sandboxed command;
    /// widen it via config if a command genuinely needs more. `None` accepts the
    /// default; each named var is seeded only if present in the process env.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_passthrough: Option<Vec<String>>,
}

/// The default confined-shell env passthrough: `HOME` (so `~` expands) + `USER`.
/// Deliberately minimal — the confined shell is a trust boundary.
#[must_use]
pub fn shell_env_passthrough_default() -> Vec<String> {
    vec!["HOME".to_string(), "USER".to_string()]
}

/// The env allow-list for a spawned **stdio MCP subprocess** (newt#1155).
///
/// A stdio MCP server was inheriting newt's ENTIRE environment — every API
/// key and token in the process — making it strictly LESS confined than a
/// `run_command` (which passes only [`shell_env_passthrough_default`]). This
/// is the opposite of the OCAP model. The subprocess still needs enough to
/// *execute* (unlike a shell builtin), so this is a deliberately-wider but
/// still CLOSED list: the shell defaults plus the vars a child process needs
/// to find its interpreter/libraries and render output. Server-specific
/// secrets belong in the entry's own explicit `env` map, overlaid on top —
/// never leaked by inheritance.
#[must_use]
pub fn mcp_stdio_env_passthrough() -> Vec<&'static str> {
    vec![
        "HOME",
        "USER",
        "PATH",
        "LANG",
        "LC_ALL",
        "TERM",
        "TMPDIR",
        "SHELL",
        // interpreter/runtime discovery a language MCP server commonly needs
        "PYTHONPATH",
        "NODE_PATH",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
    ]
}

/// The engine `--full-access` auto-selects when none is set explicitly: `host`
/// on unix (a real `/bin/sh` in the kernel jail), but **`brush` on Windows** —
/// host-shell spawns `/bin/sh -c`, which Windows lacks, so the cross-platform
/// carried brush engine is the full-grammar option there (with a Windows-usage
/// warning surfaced at selection).
#[must_use]
pub fn full_access_default_engine() -> ShellEngine {
    #[cfg(windows)]
    {
        ShellEngine::Brush
    }
    #[cfg(not(windows))]
    {
        ShellEngine::Host
    }
}

/// Resolve the effective shell engine in precedence order: an explicit CLI
/// `--shell-engine` wins, then the `[shell] engine` config key, then — only when
/// neither was set — `--full-access` auto-upgrades (to `host` on unix, `brush`
/// on Windows), otherwise the `safe-subset` default. Keeping the auto-upgrade
/// *last* means an explicit choice is never silently overridden.
#[must_use]
pub fn resolve_shell_engine(
    cli: Option<ShellEngine>,
    configured: Option<ShellEngine>,
    full_access: bool,
) -> ShellEngine {
    // Back-compat: the confined-auto case (`None`) collapses to safe-subset,
    // exactly today's behavior for callers that want a concrete engine now.
    resolve_shell_engine_choice(cli, configured, full_access).unwrap_or(ShellEngine::SafeSubset)
}

/// #1243 Leg 1: the engine CHOICE — `Some(engine)` when it is fixed at startup
/// (an explicit `--shell-engine`/`[shell] engine`, or the `--full-access`
/// auto-upgrade), and **`None` for the confined default**, which is
/// deliberately NOT decided here.
///
/// The confined default is **L3-gated and resolved per-dispatch** by
/// [`confined_default_engine`] against the *live* fence state, never cached at
/// startup — the CLI publishes `NEWT_SHELL_ENGINE` only for the `Some` case, so
/// the deep dispatch re-checks the fence at exec time. This closes the
/// mechanized TOCTOU (agent-bridle #239 `EnforcementGate.tla`): a fence that
/// dropped between a grant and an exec must not leave a stale brush selection
/// running a dynamic construct advisory.
#[must_use]
pub fn resolve_shell_engine_choice(
    cli: Option<ShellEngine>,
    configured: Option<ShellEngine>,
    full_access: bool,
) -> Option<ShellEngine> {
    if let Some(explicit) = cli.or(configured) {
        return Some(explicit);
    }
    if full_access {
        return Some(full_access_default_engine());
    }
    // Confined, no explicit choice: dispatch-time L3 gate decides.
    None
}

/// #1243 Leg 1: the confined default engine, gated on whether an L3 kernel fence
/// is enforcing on this host RIGHT NOW (`l3_active`, from [`ocap_l3_backend`]).
///
/// - **L3 enforcing ⇒ `Brush`** — the carried bash-in-Rust engine intercepts
///   every real spawn at the primitive `before_exec` funnel (pipes, subshells,
///   `$(…)`) and its dynamic constructs are actually confined by the kernel.
/// - **No L3 ⇒ `SafeSubset`** — brush would run those constructs advisory-only
///   (`sandbox_kind = None`), a honesty regression, so fall back to the static
///   parser's STRUCTURAL REFUSAL of dynamic constructs (least authority by
///   construction). Pure so the gate is unit-tested without a kernel.
#[must_use]
pub fn confined_default_engine(l3_active: bool) -> ShellEngine {
    if l3_active {
        ShellEngine::Brush
    } else {
        ShellEngine::SafeSubset
    }
}

/// #1243 Leg 1: the confined default engine resolved for THIS host — the single
/// source of truth for both `shell_engine()`'s dispatch and doctor's display.
///
/// The brush flip is scoped to platforms with a **real per-run kernel fence**:
/// Linux (landlock) and macOS (seatbelt), where [`ocap_l3_backend`] is a live
/// capability probe. Windows is deliberately left on `safe-subset`: its
/// AppContainer backend reports active *unconditionally* (not a per-run probe),
/// and brush is already the Windows `--full-access` default — a brush-confined
/// Windows default is its own follow-up, not part of the landlock/seatbelt gate
/// this leg proves. Evaluated live per call, so the fence is never cached.
#[must_use]
pub fn resolved_confined_default() -> ShellEngine {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        confined_default_engine(ocap_l3_backend().1)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        ShellEngine::SafeSubset
    }
}

/// The OCAP **L3 backend** (the kernel fence) for this platform, and whether it
/// is active on this host. This is the axis *separate* from the shell engine
/// ([`ShellEngine`]): the engine parses/runs the command line (L2), the backend
/// confines it in the kernel (L3). Surfaced by `newt doctor` (#926) so the
/// operator can see what actually enforces a restricted grant here. A restricted
/// `fs` grant is only real when the backend is available; otherwise it is
/// honestly advisory (agent-bridle reports `sandbox_kind = None`).
#[must_use]
pub fn ocap_l3_backend() -> (&'static str, bool) {
    #[cfg(target_os = "linux")]
    {
        ("Landlock", agent_bridle::landlock_is_supported())
    }
    #[cfg(target_os = "macos")]
    {
        ("Seatbelt", agent_bridle::seatbelt_is_supported())
    }
    #[cfg(target_os = "windows")]
    {
        ("AppContainer", true)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        ("none", false)
    }
}

#[cfg(test)]
mod shell_engine_tests {
    use super::{
        confined_default_engine, full_access_default_engine, resolve_shell_engine,
        resolve_shell_engine_choice, shell_env_passthrough_default, IntakeConfig, ShellConfig,
        ShellEngine,
    };
    use crate::config::Config;

    #[test]
    fn env_passthrough_default_is_minimal() {
        // Minimal by design — the confined shell is a trust boundary. HOME is the
        // load-bearing one (brush needs it to expand `~`); USER is a nicety. A
        // wider default would risk leaking secrets into a sandboxed command.
        assert_eq!(
            shell_env_passthrough_default(),
            vec!["HOME".to_string(), "USER".to_string()]
        );
    }

    #[test]
    fn from_str_canonical_and_aliases() {
        assert_eq!(
            "safe-subset".parse::<ShellEngine>().unwrap(),
            ShellEngine::SafeSubset
        );
        assert_eq!(
            "subset".parse::<ShellEngine>().unwrap(),
            ShellEngine::SafeSubset
        );
        assert_eq!("host".parse::<ShellEngine>().unwrap(), ShellEngine::Host);
        // A user thinking "landlock vs brush" resolves `landlock` → the host
        // engine (whose guarantee rests on the Landlock/Seatbelt fence).
        assert_eq!(
            "landlock".parse::<ShellEngine>().unwrap(),
            ShellEngine::Host
        );
        assert_eq!("BRUSH".parse::<ShellEngine>().unwrap(), ShellEngine::Brush);
        assert_eq!(
            "brush-ocap".parse::<ShellEngine>().unwrap(),
            ShellEngine::Brush
        );
        assert!("bogus".parse::<ShellEngine>().is_err());
    }

    #[test]
    fn as_str_roundtrips() {
        for e in [
            ShellEngine::SafeSubset,
            ShellEngine::Host,
            ShellEngine::Brush,
        ] {
            assert_eq!(e.as_str().parse::<ShellEngine>().unwrap(), e);
        }
    }

    #[test]
    fn resolve_flag_wins_over_everything() {
        assert_eq!(
            resolve_shell_engine(
                Some(ShellEngine::SafeSubset),
                Some(ShellEngine::Brush),
                true
            ),
            ShellEngine::SafeSubset,
            "explicit --shell-engine wins even over config and --full-access"
        );
    }

    #[test]
    fn resolve_config_wins_over_full_access_auto_upgrade() {
        assert_eq!(
            resolve_shell_engine(None, Some(ShellEngine::SafeSubset), true),
            ShellEngine::SafeSubset,
            "an explicit [shell] engine is not overridden by --full-access"
        );
    }

    #[test]
    fn resolve_full_access_auto_upgrades_when_unset() {
        // `host` on unix, `brush` on Windows (host-shell needs `/bin/sh`).
        assert_eq!(
            resolve_shell_engine(None, None, true),
            full_access_default_engine()
        );
        #[cfg(not(windows))]
        assert_eq!(resolve_shell_engine(None, None, true), ShellEngine::Host);
        #[cfg(windows)]
        assert_eq!(resolve_shell_engine(None, None, true), ShellEngine::Brush);
    }

    #[test]
    fn resolve_defaults_to_safe_subset() {
        assert_eq!(
            resolve_shell_engine(None, None, false),
            ShellEngine::SafeSubset
        );
    }

    /// #1243 Leg 1: the confined default is L3-gated — brush only where a
    /// kernel fence enforces; safe-subset's structural refusal otherwise.
    #[test]
    fn confined_default_is_l3_gated() {
        assert_eq!(confined_default_engine(true), ShellEngine::Brush);
        assert_eq!(confined_default_engine(false), ShellEngine::SafeSubset);
    }

    /// #1243 Leg 1 (the TOCTOU-closing invariant): an explicit flag/config or
    /// --full-access yields a FIXED `Some(engine)` published at startup, but
    /// the confined default is `None` — deliberately unpublished so the deep
    /// dispatch re-checks the live fence per command.
    #[test]
    fn choice_is_none_only_for_the_confined_default() {
        // Explicit and full-access are fixed at startup.
        assert_eq!(
            resolve_shell_engine_choice(Some(ShellEngine::Brush), None, false),
            Some(ShellEngine::Brush)
        );
        assert_eq!(
            resolve_shell_engine_choice(None, Some(ShellEngine::Host), false),
            Some(ShellEngine::Host)
        );
        assert_eq!(
            resolve_shell_engine_choice(None, None, true),
            Some(full_access_default_engine())
        );
        // The confined default is NOT fixed — the dispatch gate decides.
        assert_eq!(resolve_shell_engine_choice(None, None, false), None);
    }

    #[test]
    fn shell_config_deserializes_kebab_case() {
        let cfg: ShellConfig = toml::from_str("engine = \"host\"").unwrap();
        assert_eq!(cfg.engine, Some(ShellEngine::Host));
        let cfg: ShellConfig = toml::from_str("engine = \"safe-subset\"").unwrap();
        assert_eq!(cfg.engine, Some(ShellEngine::SafeSubset));
    }

    // ── #1260: the `[intake]` disposition-inference table ───────────────────

    #[test]
    fn intake_config_defaults_are_all_unset_and_resolve_to_builtin_lexicon() {
        let cfg: IntakeConfig = toml::from_str("").unwrap();
        assert_eq!(cfg, IntakeConfig::default());
        assert_eq!(
            cfg.to_lexicon(),
            crate::agentic::DispositionLexicon::default(),
            "an empty [intake] must resolve to exactly the built-in defaults"
        );
    }

    #[test]
    fn intake_config_overrides_round_trip_and_resolve() {
        let cfg: IntakeConfig = toml::from_str(
            r#"
                explain = ["tell me about"]
                question_mark_disposition = "research"
            "#,
        )
        .unwrap();
        // Round-trip: the knob names survive serialize → parse (never silently
        // lost by a rename).
        let echoed: IntakeConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(echoed, cfg);

        let lex = cfg.to_lexicon();
        assert_eq!(
            lex.explain,
            vec!["tell me about".to_string()],
            "a present list REPLACES its default wholesale"
        );
        assert_eq!(
            lex.action,
            crate::agentic::DispositionLexicon::default().action,
            "an absent list keeps the built-in default"
        );
        assert_eq!(
            lex.question_mark_disposition,
            crate::agentic::PromptDisposition::Research
        );
        // Unknown fallback values degrade to the default, never error.
        let odd: IntakeConfig = toml::from_str("question_mark_disposition = \"bogus\"").unwrap();
        assert_eq!(
            odd.to_lexicon().question_mark_disposition,
            crate::agentic::PromptDisposition::Explain
        );
    }

    #[test]
    fn config_root_parses_the_intake_table() {
        let cfg: Config = toml::from_str(
            r#"
                [intake]
                action = ["deploy"]
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.intake.as_ref().and_then(|i| i.action.clone()),
            Some(vec!["deploy".to_string()])
        );
    }
}
