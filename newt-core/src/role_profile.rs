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
    /// Operating ALTITUDE — act (doer, the default) vs advise (coach). Selected
    /// in front-matter as `altitude = "coach"`. `None` = doer. Drives which base
    /// identity the system prompt installs (FR-5, #999): a `Coach` altitude
    /// REPLACES `DEFAULT_SOUL` with `COACH_SOUL` so a coaching persona doesn't
    /// ship two contradictory identities (doer soul + advise overlay).
    pub altitude: Option<Altitude>,
}

/// The persona's operating altitude (FR-5, #999). Decides whether the base
/// identity ("soul") is the doer `DEFAULT_SOUL` or the advise-first
/// `COACH_SOUL`. Serialized in front-matter as `altitude = "coach"` (or the
/// synonym `"advise"`); an absent field is [`Altitude::Doer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Altitude {
    /// Act: make the change, run the command. Today's behavior.
    #[default]
    Doer,
    /// Advise: explain, present options, recommend — never mutate. Accepts the
    /// synonym `"advise"` in front-matter.
    #[serde(alias = "advise")]
    Coach,
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
    #[serde(default)]
    altitude: Option<Altitude>,
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
            // DEFERRED (issue #755 / epic #749): `valid_for_generation` is a
            // causal-generation-window axis, not a filesystem/exec/net
            // capability a preset or role profile clamps — it is minted at
            // dispatch time. Narrowing it here is out of scope for the M6
            // grammar-gap fix; left at `Scope::All` as a follow-up.
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

/// A **named permission preset** (issue #307): a config-declared authority
/// *clamp* that maps onto the same [`CaveatProfile`] / [`Caveats`] lattice a
/// role profile uses. It is deliberately a thin, human-friendly surface over
/// [`CaveatProfile`] — the existing caveat-profile mechanism — rather than a
/// parallel type, so the same `to_scope` / `to_caveats` lowering is reused.
///
/// ## Config shape (`[permission_presets.<name>]`)
///
/// ```toml
/// [permission_presets.readonly-triage]
/// readonly   = true            # read anything; deny all writes
/// exec_allow = ["git", "gh"]   # a small exec allowlist
/// deny       = ["*"]           # everything else (net) denied
/// max_calls  = 40              # optional tool-call ceiling
/// # fs_read  = ["src/"]        # optional: confine reads (omit ⇒ read all)
/// ```
///
/// ## Semantics — a clamp, never a grant
///
/// The clamp is consumed via [`NamedPermissionPreset::clamp`], which lowers to a
/// [`Caveats`] **ceiling**. The session's effective authority is the
/// *intersection* (`meet`) of the session's base caveats and this ceiling — a
/// preset can only ever **attenuate**, never widen. This is the load-bearing
/// property: the ceiling wins over `--disable-ocap` / `--yolo` and over
/// interactive session-grants, because it is `meet`-ed into the authority the
/// enforcement site consults (and re-`meet`-ed over any re-minted grant).
///
/// Each field maps onto a [`CaveatProfile`] axis:
/// - `fs_read = [..]` / `"none"` ⇒ clamp `fs_read = Only({..})` / `none`.
///   **Optional** — omitted (`None`) leaves `fs_read` unrestricted (`all`),
///   the back-compat default. Before this field a preset structurally could
///   *not* narrow reads (the M6 grammar gap, issue #755) even though
///   [`CaveatProfile`] / [`Caveats`] can express a read clamp. A read-only
///   triage mode still reads everything unless it sets this deliberately.
/// - `readonly = true` ⇒ `fs_write = none`, `exec = none` (the floor a triage
///   mode wants). `false`/omitted leaves those axes unconstrained (`all`).
/// - `exec_allow = [..]` ⇒ `exec = Only({..})`. Overrides `readonly`'s
///   exec clamp with the explicit allowlist (so a triage mode can permit a few
///   read-only commands).
/// - `deny = ["*"]` ⇒ clamp `net` to `none` (the remaining "everything else"
///   axis once fs/exec are pinned). A non-`*` `deny` list is recorded but does
///   not subtract from an allowlist here — the lattice is allow-list shaped, so
///   "deny everything else" is the expressible clamp.
/// - `max_calls` ⇒ the [`CountBound`] ceiling, as on a role profile.
///
/// A preset with no fields set is the identity clamp (`Caveats::top()`), so an
/// empty `[permission_presets.x]` block leaves authority unchanged.
///
/// **Footgun (review #312):** omitted axes stay at `TOP`. A preset that sets
/// only `exec_allow = ["git"]` clamps *exec* but leaves `fs_write` and `net`
/// wide open — it is NOT "locked down" despite reading that way. For a true
/// read-only triage clamp, set `readonly = true` (denies writes + exec) and
/// add `exec_allow` only for the few programs the mode genuinely needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NamedPermissionPreset {
    /// Optional `fs_read` clamp (issue #755 — the M6 grammar-gap fix).
    /// `None`/omitted ⇒ the `fs_read` axis stays at the top of its axis
    /// (`all`), exactly the pre-#755 behavior where a preset structurally
    /// could not narrow reads. `Some(spec)` narrows reads (e.g.
    /// `Only({..})` to confine a triage mode to a subtree). Serde-defaults
    /// to `None`, so every existing preset is byte-for-byte unchanged.
    #[serde(default)]
    pub fs_read: Option<ScopeSpec>,
    /// `true` ⇒ deny all writes and (unless `exec_allow` overrides) all exec.
    #[serde(default)]
    pub readonly: bool,
    /// Explicit exec allowlist. Non-empty ⇒ `exec = Only({..})`, overriding the
    /// `readonly` exec clamp. Empty + `readonly` ⇒ `exec = none`.
    #[serde(default)]
    pub exec_allow: Vec<String>,
    /// "Deny everything else" marker. `["*"]` clamps the `net` axis to `none`
    /// (writes/exec are pinned by `readonly`/`exec_allow`). Other entries are
    /// accepted for forward-compat but the allow-list lattice expresses denial
    /// as "not in the allowlist", so they do not subtract here.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Optional tool-call ceiling. `None` ⇒ no extra bound.
    #[serde(default)]
    pub max_calls: Option<u64>,
}

impl NamedPermissionPreset {
    /// `true` when `deny` asks to deny everything else (`["*"]`).
    fn denies_all(&self) -> bool {
        self.deny.iter().any(|d| d == "*")
    }

    /// Lower this named preset into the equivalent [`CaveatProfile`] — reusing
    /// the SAME serde-shaped caveat mechanism a role profile carries, rather
    /// than a parallel lowering. Axes the preset does not constrain stay at the
    /// top of their axis (`all`) so `clamp` is the identity on them.
    #[must_use]
    pub fn to_caveat_profile(&self) -> CaveatProfile {
        // fs_write: readonly pins it to none; otherwise unconstrained.
        let fs_write = if self.readonly {
            ScopeSpec::Keyword(ScopeKeyword::None)
        } else {
            ScopeSpec::default()
        };
        // exec: an explicit allowlist wins; else readonly pins it to none; else
        // unconstrained.
        let exec = if !self.exec_allow.is_empty() {
            ScopeSpec::Items(self.exec_allow.clone())
        } else if self.readonly {
            ScopeSpec::Keyword(ScopeKeyword::None)
        } else {
            ScopeSpec::default()
        };
        // net: `deny = ["*"]` clamps the remaining axis to none.
        let net = if self.denies_all() {
            ScopeSpec::Keyword(ScopeKeyword::None)
        } else {
            ScopeSpec::default()
        };
        // fs_read: an explicit clamp narrows reads (issue #755 — the M6
        // grammar-gap fix). `None`/omitted ⇒ the top of the axis (`all`), the
        // historical behavior: before #755 this was hardcoded to
        // `ScopeSpec::default()`, so a preset structurally could not clamp
        // reads even though `CaveatProfile`/`Caveats` can. A read-only triage
        // mode still reads everything unless a preset deliberately narrows it.
        let fs_read = self.fs_read.clone().unwrap_or_default();
        // DEFERRED (default-deny is step 8's job, not here): the "empty/absent
        // preset ⇒ identity clamp (`Caveats::top()`)" default — each unset axis
        // staying at the top of its axis — is the CORRECT meet-identity
        // semantics for a *clamp*: an empty clamp adds no restriction and is
        // applied via `meet`, so an absent axis must be the top. The
        // "default-deny for un-annotated subtasks" (an absent subtask `kind` ⇒
        // the most-restrictive preset) belongs in step 8's subtask-clamp
        // derivation, NOT in this general lowering. Flipping the general default
        // to deny here would break back-compat for every preset consumer
        // (steps 2-7), so step 8 owns that default-deny.
        CaveatProfile {
            fs_read,
            fs_write,
            exec,
            net,
            max_calls: self.max_calls,
        }
    }

    /// The authority **ceiling** this preset imposes, as a [`Caveats`]. The
    /// session's effective authority is `base.meet(&preset.clamp())` — the
    /// clamp can only attenuate, never widen. Reuses [`CaveatProfile::to_caveats`].
    #[must_use]
    pub fn clamp(&self) -> Caveats {
        self.to_caveat_profile().to_caveats()
    }

    /// One-line human summary for `/permissions` and `/mode` reporting, e.g.
    /// `readonly exec=[git, gh] deny=* max_calls=40`.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.readonly {
            parts.push("readonly".to_string());
        }
        if !self.exec_allow.is_empty() {
            parts.push(format!("exec=[{}]", self.exec_allow.join(", ")));
        } else if self.readonly {
            parts.push("exec=none".to_string());
        }
        if self.denies_all() {
            parts.push("deny=*".to_string());
        }
        if let Some(n) = self.max_calls {
            parts.push(format!("max_calls={n}"));
        }
        if parts.is_empty() {
            "unconstrained".to_string()
        } else {
            parts.join(" ")
        }
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
            altitude: fm.altitude,
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
            || self.altitude.is_some()
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

    /// FR-5 (#999): `altitude` parses (with `advise` as a synonym for `coach`),
    /// defaults to `None` when absent, and counts as role-binding front-matter.
    #[test]
    fn parses_altitude_with_advise_synonym() {
        let coach = RoleProfile::parse("+++\naltitude = \"coach\"\n+++\n\nx\n").unwrap();
        assert_eq!(coach.altitude, Some(Altitude::Coach));
        assert!(coach.is_role_bound(), "altitude alone binds the role");
        assert_eq!(
            RoleProfile::parse("+++\naltitude = \"advise\"\n+++\n\nx\n")
                .unwrap()
                .altitude,
            Some(Altitude::Coach),
            "`advise` is a synonym for coach"
        );
        assert_eq!(
            RoleProfile::parse("+++\naltitude = \"doer\"\n+++\n\nx\n")
                .unwrap()
                .altitude,
            Some(Altitude::Doer)
        );
        assert_eq!(
            RoleProfile::parse("+++\nrole = \"w\"\n+++\n\nx\n")
                .unwrap()
                .altitude,
            None,
            "absent altitude is None (doer applies downstream)"
        );
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

    // --- #307 named permission presets ----------------------------------

    #[test]
    fn empty_preset_is_identity_clamp() {
        // An empty `[permission_presets.x]` block leaves authority unchanged:
        // its clamp is `top()`, so `base.meet(clamp) == base`.
        let p = NamedPermissionPreset::default();
        assert_eq!(p.clamp(), Caveats::top());
    }

    #[test]
    fn readonly_preset_denies_writes_and_exec() {
        // `readonly = true` ⇒ fs_write=none, exec=none; fs_read/net untouched.
        let p = NamedPermissionPreset {
            readonly: true,
            ..NamedPermissionPreset::default()
        };
        let c = p.clamp();
        assert_eq!(c.fs_read, Scope::All, "read-only still reads everything");
        assert_eq!(c.fs_write, Scope::none());
        assert_eq!(c.exec, Scope::none());
        assert_eq!(c.net, Scope::All, "net untouched without deny=*");
    }

    #[test]
    fn exec_allow_overrides_readonly_exec_clamp() {
        // A readonly triage mode may still permit a few read-only commands.
        let p = NamedPermissionPreset {
            readonly: true,
            exec_allow: vec!["git".to_string(), "gh".to_string()],
            ..NamedPermissionPreset::default()
        };
        let c = p.clamp();
        assert_eq!(c.fs_write, Scope::none(), "readonly still pins writes");
        assert_eq!(
            c.exec,
            Scope::only(["git".to_string(), "gh".to_string()]),
            "explicit allowlist wins over readonly's exec=none"
        );
    }

    #[test]
    fn deny_star_clamps_net_to_none() {
        let p = NamedPermissionPreset {
            fs_read: None,
            readonly: true,
            exec_allow: vec!["git".to_string()],
            deny: vec!["*".to_string()],
            max_calls: Some(40),
        };
        let c = p.clamp();
        assert_eq!(c.net, Scope::none(), "deny=* clamps the net axis");
        assert_eq!(c.max_calls, CountBound::AtMost(40));
    }

    #[test]
    fn preset_clamp_only_attenuates_a_full_base() {
        // The load-bearing property: meeting a fully-authorized base with a
        // readonly clamp yields exactly the clamp's restrictions — never wider.
        let p = NamedPermissionPreset {
            readonly: true,
            exec_allow: vec!["git".to_string()],
            deny: vec!["*".to_string()],
            ..NamedPermissionPreset::default()
        };
        let clamp = p.clamp();
        let effective = Caveats::top().meet(&clamp);
        assert_eq!(effective, clamp, "meet(top, clamp) == clamp");
        assert!(effective.leq(&Caveats::top()), "clamp ⊑ top (attenuation)");
    }

    #[test]
    fn preset_parses_from_config_toml_shape() {
        // The exact `[permission_presets.<name>]` shape from the issue.
        let toml = "\
readonly = true
exec_allow = [\"git\", \"gh\"]
deny = [\"*\"]
max_calls = 40
";
        let p: NamedPermissionPreset = toml::from_str(toml).unwrap();
        assert!(p.readonly);
        assert_eq!(p.exec_allow, vec!["git".to_string(), "gh".to_string()]);
        assert_eq!(p.deny, vec!["*".to_string()]);
        assert_eq!(p.max_calls, Some(40));
    }

    #[test]
    fn preset_summary_renders_each_clamp() {
        let p = NamedPermissionPreset {
            fs_read: None,
            readonly: true,
            exec_allow: vec!["git".to_string(), "gh".to_string()],
            deny: vec!["*".to_string()],
            max_calls: Some(40),
        };
        assert_eq!(p.summary(), "readonly exec=[git, gh] deny=* max_calls=40");
        assert_eq!(NamedPermissionPreset::default().summary(), "unconstrained");
    }

    // --- #755 M6 grammar gap: a preset can clamp fs_read -----------------

    #[test]
    fn fs_read_clamp_narrows_reads() {
        // M6 grammar-gap fix (issue #755): a preset CAN now clamp `fs_read`.
        //
        // RED on pre-#755 code: `to_caveat_profile` hardcoded
        // `fs_read: ScopeSpec::default()` (= `Scope::All`), so the `fs_read`
        // axis was *always* unrestricted regardless of the preset — this
        // assertion would have read `Scope::All`. GREEN once the optional
        // `fs_read` clamp is honored.
        let p = NamedPermissionPreset {
            fs_read: Some(ScopeSpec::Items(vec!["src/".to_string()])),
            ..NamedPermissionPreset::default()
        };
        let c = p.clamp();
        assert_eq!(
            c.fs_read,
            Scope::only(["src/".to_string()]),
            "an fs_read clamp must narrow the read axis"
        );
        // The other axes are untouched by an fs_read-only clamp.
        assert_eq!(c.fs_write, Scope::All);
        assert_eq!(c.exec, Scope::All);
        assert_eq!(c.net, Scope::All);
    }

    #[test]
    fn omitted_fs_read_clamp_reads_all() {
        // Back-compat: a preset that does not set `fs_read` leaves the read
        // axis at the top (`all`), exactly the pre-#755 behavior. A `None`
        // serde-default lowers to `ScopeSpec::default()` ⇒ `Scope::All`.
        let p = NamedPermissionPreset {
            readonly: true,
            ..NamedPermissionPreset::default()
        };
        assert_eq!(
            p.clamp().fs_read,
            Scope::All,
            "an unspecified fs_read clamp must leave reads unrestricted"
        );
        // The default preset declares no fs_read clamp.
        assert_eq!(NamedPermissionPreset::default().fs_read, None);
    }

    #[test]
    fn fs_read_clamp_parses_from_config_toml() {
        // The `[permission_presets.<name>]` config shape accepts an optional
        // `fs_read` clamp as a keyword or an allow-list, like the other axes.
        let toml = "\
readonly = true
fs_read = [\"src/\", \"docs/\"]
";
        let p: NamedPermissionPreset = toml::from_str(toml).unwrap();
        assert!(p.readonly);
        assert_eq!(
            p.fs_read,
            Some(ScopeSpec::Items(vec![
                "src/".to_string(),
                "docs/".to_string()
            ]))
        );
        assert_eq!(
            p.clamp().fs_read,
            Scope::only(["src/".to_string(), "docs/".to_string()])
        );
    }
}
