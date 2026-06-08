//! `RoleProfile` — promotes a persona from a prompt-only overlay into a
//! "role profile" that binds a system-prompt overlay together with a toolset,
//! a capability (caveat) profile, and backend/router policy.
//!
//! ## Why this exists
//!
//! A persona today is just a system-prompt overlay loaded from a `.md` file
//! (see `newt-tui`'s `Persona`). The one-airframe-many-roles topology — a
//! single newt binary that can embody a `dragon-rider` (orchestrator), a
//! `wing-commander` (arbiter/judge), or a `worker` — needs each role to carry
//! *more* than prose: which tools it may call, what authority (caveats) it
//! holds, and which model/tier it should route to. This module is that seam.
//!
//! ## File format (backward compatible)
//!
//! A role profile is still a plain `.md` file. The only addition is an
//! **optional** TOML front-matter block fenced by `+++` lines at the very top
//! of the file:
//!
//! ```text
//! +++
//! role = "worker"
//! tools = ["read_file", "write_file", "run_command"]
//! model = "qwen2.5-coder:14b"
//! tier = "STANDARD"
//!
//! [caveats]
//! fs_read = "all"
//! fs_write = ["src/"]
//! exec = ["cargo"]
//! net = "none"
//! max_calls = 40
//! +++
//!
//! # Worker
//!
//! You edit files and run builds...
//! ```
//!
//! A file with **no** `+++` front-matter parses into a `RoleProfile` whose
//! every field except `prompt` is `None` — i.e. exactly today's prompt-only
//! persona. This is the load-bearing backward-compat guarantee.
//!
//! ## What this slice wires vs. follow-ups
//!
//! This module only *parses and represents* a role profile and converts the
//! caveat shape into the canonical [`Caveats`]. It does **not** enforce the
//! toolset or caveats — enforcement (an agent-bridle registry at the tool
//! dispatch site, worker/MCP entry points reading the profile) is a deliberate
//! follow-up. See `docs/design/role-profiles.md`.

use serde::{Deserialize, Serialize};

use crate::router::Tier;
use crate::{Caveats, CountBound, Scope};

/// A parsed role profile: a system-prompt overlay plus the optional toolset /
/// capability / router policy declared in TOML front-matter.
///
/// `prompt` is always the markdown body (front-matter stripped). Every other
/// field is `Some` only when the front-matter declared it; a profile with no
/// front-matter has all-`None` non-prompt fields and behaves exactly like
/// today's prompt-only persona.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoleProfile {
    /// The markdown body (front-matter removed). Injected as the persona
    /// overlay exactly as today.
    pub prompt: String,
    /// Declared role name (e.g. `"dragon-rider"`, `"worker"`). Independent of
    /// the persona file's stem, so a file can name the role it embodies.
    pub role: Option<String>,
    /// Allow-list of tool names this role may use. `None` = unconstrained
    /// (today's behavior); enforcement is a follow-up.
    pub tools: Option<Vec<String>>,
    /// Capability profile (agent-bridle caveats) this role carries.
    pub caveats: Option<CaveatProfile>,
    /// Preferred backend model id (router policy hint).
    pub model: Option<String>,
    /// Preferred router tier (router policy hint).
    pub tier: Option<Tier>,
}

/// The TOML front-matter shape. Kept separate from [`RoleProfile`] so the
/// `prompt` (markdown body) is never part of the serde surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct FrontMatter {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    caveats: Option<CaveatProfile>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tier: Option<Tier>,
}

/// A small, human-friendly serde shape for an agent-bridle capability profile.
///
/// Each filesystem/exec/net axis is a [`ScopeSpec`] that maps onto the
/// canonical [`Scope<String>`]; `max_calls` maps onto [`CountBound`]. Omitting
/// an axis defaults it to the top of that axis (unrestricted) via
/// [`ScopeSpec::default`], matching `Caveats::top()` so a sparse profile
/// attenuates only the axes it names. Convert with [`CaveatProfile::to_caveats`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CaveatProfile {
    #[serde(default)]
    pub fs_read: ScopeSpec,
    #[serde(default)]
    pub fs_write: ScopeSpec,
    #[serde(default)]
    pub exec: ScopeSpec,
    #[serde(default)]
    pub net: ScopeSpec,
    /// Upper bound on tool calls. `None`/omitted = unlimited.
    #[serde(default)]
    pub max_calls: Option<u64>,
}

/// A human-friendly serde form of a single [`Scope<String>`] axis.
///
/// Accepts either the bare string `"all"`/`"none"` or a list of allowed items:
///
/// ```text
/// fs_read  = "all"            # Scope::All
/// net      = "none"           # Scope::Only({})  — authorizes nothing
/// fs_write = ["src/", "Cargo.toml"]   # Scope::Only({...})
/// ```
///
/// Defaults to [`ScopeSpec::All`] (unrestricted) so an omitted axis is the top
/// of that axis, matching `Caveats::top()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScopeSpec {
    /// The keyword `"all"` or `"none"`.
    Keyword(ScopeKeyword),
    /// An explicit allow-list of items.
    Items(Vec<String>),
}

impl Default for ScopeSpec {
    fn default() -> Self {
        Self::Keyword(ScopeKeyword::All)
    }
}

/// The `"all"` / `"none"` keywords for a [`ScopeSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeKeyword {
    /// Unrestricted (top of the axis).
    All,
    /// Authorizes nothing.
    None,
}

impl ScopeSpec {
    /// Convert to the canonical lattice [`Scope<String>`].
    #[must_use]
    pub fn to_scope(&self) -> Scope<String> {
        match self {
            Self::Keyword(ScopeKeyword::All) => Scope::All,
            Self::Keyword(ScopeKeyword::None) => Scope::none(),
            Self::Items(items) => Scope::only(items.iter().cloned()),
        }
    }

    /// One-line human summary of this axis (`"all"`, `"none"`, or `"[a, b]"`),
    /// used by `/persona show` reporting.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Keyword(ScopeKeyword::All) => "all".to_string(),
            Self::Keyword(ScopeKeyword::None) => "none".to_string(),
            Self::Items(items) => format!("[{}]", items.join(", ")),
        }
    }
}

impl CaveatProfile {
    /// Convert this profile into the canonical [`Caveats`] lattice element.
    ///
    /// Omitted axes default to the top of their axis (unrestricted),
    /// `valid_for_generation` is always `Scope::All` here (the profile does not
    /// express generation bounds — that is minted at dispatch time), and
    /// `max_calls` becomes [`CountBound::AtMost`] when set, else
    /// [`CountBound::Unlimited`].
    #[must_use]
    pub fn to_caveats(&self) -> Caveats {
        Caveats {
            fs_read: self.fs_read.to_scope(),
            fs_write: self.fs_write.to_scope(),
            exec: self.exec.to_scope(),
            net: self.net.to_scope(),
            max_calls: match self.max_calls {
                Some(n) => CountBound::AtMost(n),
                None => CountBound::Unlimited,
            },
            valid_for_generation: Scope::All,
        }
    }

    /// One-line human summary of this capability profile for `/persona show`,
    /// e.g. `fs_read=all fs_write=none exec=[cargo] net=all max_calls=40`.
    #[must_use]
    pub fn summary(&self) -> String {
        let calls = match self.max_calls {
            Some(n) => n.to_string(),
            None => "unlimited".to_string(),
        };
        format!(
            "fs_read={} fs_write={} exec={} net={} max_calls={}",
            self.fs_read.summary(),
            self.fs_write.summary(),
            self.exec.summary(),
            self.net.summary(),
            calls,
        )
    }
}

/// The fence marker for TOML front-matter (must be on its own line at the very
/// top of the file).
const FENCE: &str = "+++";

impl RoleProfile {
    /// Parse a role-profile `.md` file's full text into a [`RoleProfile`].
    ///
    /// If the text begins with a `+++` fence line, everything up to the closing
    /// `+++` line is parsed as TOML front-matter and the remainder is the
    /// prompt body. Otherwise the whole text is the prompt and every other
    /// field is `None` (prompt-only — today's behavior).
    ///
    /// # Errors
    ///
    /// Returns an error if a front-matter fence is opened but never closed, or
    /// if the front-matter is not valid TOML for the expected shape.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let (front_matter, body) = split_front_matter(text)?;
        let body = body.trim().to_string();
        let Some(fm_text) = front_matter else {
            // No front-matter: prompt-only, exactly today's persona.
            return Ok(Self {
                prompt: body,
                ..Self::default()
            });
        };
        let fm: FrontMatter = toml::from_str(fm_text)
            .map_err(|e| anyhow::anyhow!("invalid role-profile front-matter: {e}"))?;
        Ok(Self {
            prompt: body,
            role: fm.role,
            tools: fm.tools,
            caveats: fm.caveats,
            model: fm.model,
            tier: fm.tier,
        })
    }

    /// `true` when this profile declares more than a prompt (i.e. front-matter
    /// bound a role, tools, caveats, model, or tier). A prompt-only persona
    /// returns `false`.
    #[must_use]
    pub fn is_role_bound(&self) -> bool {
        self.role.is_some()
            || self.tools.is_some()
            || self.caveats.is_some()
            || self.model.is_some()
            || self.tier.is_some()
    }
}

/// Split optional `+++`-fenced TOML front-matter from a markdown body.
///
/// Returns `(Some(front_matter_text), body)` when a fence is present, or
/// `(None, whole_text)` otherwise.
fn split_front_matter(text: &str) -> anyhow::Result<(Option<&str>, &str)> {
    // Front-matter must be the very first line (allowing a leading BOM /
    // whitespace-free start). We intentionally do not skip blank lines before
    // the fence: a leading blank line means "no front-matter".
    let trimmed_start = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = trimmed_start.strip_prefix(FENCE) else {
        return Ok((None, text));
    };
    // The opening fence must be its own line: the char right after `+++` must
    // be a newline (or the string ends).
    let rest = match rest.strip_prefix('\n') {
        Some(r) => r,
        None => match rest.strip_prefix("\r\n") {
            Some(r) => r,
            // `+++foo` on the first line is not a fence — treat as body.
            None if rest.is_empty() => "",
            None => return Ok((None, text)),
        },
    };
    // Find the closing fence: a line that is exactly `+++`.
    for (idx, line) in LineOffsets::new(rest) {
        if line.trim_end_matches(['\r', '\n']).trim() == FENCE {
            let fm = &rest[..idx];
            let after = &rest[idx + line.len()..];
            return Ok((Some(fm), after));
        }
    }
    anyhow::bail!("role-profile front-matter opened with `+++` but never closed")
}

/// Iterator over `(byte_offset, line_with_terminator)` pairs of a string.
struct LineOffsets<'a> {
    rest: &'a str,
    offset: usize,
}

impl<'a> LineOffsets<'a> {
    fn new(s: &'a str) -> Self {
        Self { rest: s, offset: 0 }
    }
}

impl<'a> Iterator for LineOffsets<'a> {
    type Item = (usize, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let end = match self.rest.find('\n') {
            Some(i) => i + 1,
            None => self.rest.len(),
        };
        let line = &self.rest[..end];
        let start = self.offset;
        self.offset += end;
        self.rest = &self.rest[end..];
        Some((start, line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_front_matter_is_prompt_only() {
        let text = "# Coder\n\nYou write code.\n";
        let rp = RoleProfile::parse(text).unwrap();
        assert_eq!(rp.prompt, "# Coder\n\nYou write code.");
        assert_eq!(rp.role, None);
        assert_eq!(rp.tools, None);
        assert_eq!(rp.caveats, None);
        assert_eq!(rp.model, None);
        assert_eq!(rp.tier, None);
        assert!(!rp.is_role_bound());
    }

    #[test]
    fn parses_full_front_matter() {
        let text = "\
+++
role = \"worker\"
tools = [\"read_file\", \"write_file\", \"run_command\"]
model = \"qwen2.5-coder:14b\"
tier = \"STANDARD\"

[caveats]
fs_read = \"all\"
fs_write = [\"src/\"]
exec = [\"cargo\"]
net = \"none\"
max_calls = 40
+++

# Worker

You edit files and run builds.
";
        let rp = RoleProfile::parse(text).unwrap();
        assert_eq!(rp.prompt, "# Worker\n\nYou edit files and run builds.");
        assert_eq!(rp.role.as_deref(), Some("worker"));
        assert_eq!(
            rp.tools,
            Some(vec![
                "read_file".to_string(),
                "write_file".to_string(),
                "run_command".to_string(),
            ])
        );
        assert_eq!(rp.model.as_deref(), Some("qwen2.5-coder:14b"));
        assert_eq!(rp.tier, Some(Tier::Standard));
        assert!(rp.is_role_bound());

        let caveats = rp.caveats.unwrap().to_caveats();
        assert_eq!(caveats.fs_read, Scope::All);
        assert_eq!(caveats.fs_write, Scope::only(["src/".to_string()]));
        assert_eq!(caveats.exec, Scope::only(["cargo".to_string()]));
        assert_eq!(caveats.net, Scope::none());
        assert_eq!(caveats.max_calls, CountBound::AtMost(40));
    }

    #[test]
    fn sparse_caveats_default_to_top() {
        // Only fs_write named; every other axis defaults to unrestricted.
        let text = "\
+++
role = \"reviewer\"
[caveats]
fs_write = \"none\"
+++

# Reviewer
Grade diffs.
";
        let rp = RoleProfile::parse(text).unwrap();
        let c = rp.caveats.unwrap().to_caveats();
        assert_eq!(c.fs_read, Scope::All);
        assert_eq!(c.fs_write, Scope::none());
        assert_eq!(c.exec, Scope::All);
        assert_eq!(c.net, Scope::All);
        assert_eq!(c.max_calls, CountBound::Unlimited);
    }

    #[test]
    fn malformed_front_matter_errors_clearly() {
        let text = "\
+++
role = \"oops
+++

body
";
        let err = RoleProfile::parse(text).unwrap_err().to_string();
        assert!(
            err.contains("front-matter"),
            "error should mention front-matter, got: {err}"
        );
    }

    #[test]
    fn unclosed_front_matter_errors() {
        let text = "+++\nrole = \"worker\"\n\n# body never fenced\n";
        let err = RoleProfile::parse(text).unwrap_err().to_string();
        assert!(
            err.contains("never closed"),
            "error should mention unclosed fence, got: {err}"
        );
    }

    #[test]
    fn empty_front_matter_block_is_valid() {
        // A `+++ +++` with nothing between is valid empty TOML → all None.
        let text = "+++\n+++\n\n# Just a prompt\n";
        let rp = RoleProfile::parse(text).unwrap();
        assert_eq!(rp.prompt, "# Just a prompt");
        assert!(!rp.is_role_bound());
    }

    #[test]
    fn leading_plus_text_is_not_a_fence() {
        // `+++foo` on the first line is body text, not a fence opener.
        let text = "+++foo bar\nstill body\n";
        let rp = RoleProfile::parse(text).unwrap();
        assert_eq!(rp.prompt, "+++foo bar\nstill body");
        assert!(!rp.is_role_bound());
    }

    #[test]
    fn scope_spec_items_round_trip() {
        let spec = ScopeSpec::Items(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            spec.to_scope(),
            Scope::only(["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn caveat_profile_default_is_top() {
        let c = CaveatProfile::default().to_caveats();
        assert_eq!(c, Caveats::top());
    }

    #[test]
    fn scope_spec_summary_renders_each_variant() {
        assert_eq!(ScopeSpec::Keyword(ScopeKeyword::All).summary(), "all");
        assert_eq!(ScopeSpec::Keyword(ScopeKeyword::None).summary(), "none");
        assert_eq!(
            ScopeSpec::Items(vec!["a".to_string(), "b".to_string()]).summary(),
            "[a, b]"
        );
    }

    #[test]
    fn caveat_profile_summary_is_one_line_per_axis() {
        let text = "\
+++
role = \"worker\"
[caveats]
fs_read = \"all\"
fs_write = \"none\"
exec = [\"cargo\"]
max_calls = 40
+++

# Worker
body
";
        let rp = RoleProfile::parse(text).unwrap();
        let summary = rp.caveats.unwrap().summary();
        assert_eq!(
            summary,
            "fs_read=all fs_write=none exec=[cargo] net=all max_calls=40"
        );
    }

    #[test]
    fn caveat_profile_summary_unlimited_when_no_max_calls() {
        let summary = CaveatProfile::default().summary();
        assert!(
            summary.ends_with("max_calls=unlimited"),
            "omitted max_calls renders as unlimited, got: {summary}"
        );
    }
}
