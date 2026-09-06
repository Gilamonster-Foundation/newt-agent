use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use super::Config;

// ---------------------------------------------------------------------------
// Project-local config layering (issue #222)
// ---------------------------------------------------------------------------

/// How arrays (`[[backends]]`, `[[providers]]`, `[[mcp_servers]]`,
/// `[[model_tuning]]`) are combined when a project-local `.newt/config.toml`
/// is layered over the global config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArrayMergeStrategy {
    /// The project array replaces the global array wholesale. Predictable and
    /// safe — the project fully owns that list. **Default.**
    #[default]
    Replace,
    /// The project array is appended to the global array (global entries first,
    /// then the project's). Additive — e.g. register an extra local stdio MCP
    /// server without redefining the global ones.
    Append,
}

/// Controls how a project-local `.newt/config.toml` is merged over the global
/// config. Tables always merge recursively (project keys win); this only
/// governs array handling. See issue #222.
///
/// Example project `.newt/config.toml`:
/// ```toml
/// [merge]
/// arrays = "append"     # add to the global lists instead of replacing them
///
/// [[mcp_servers]]
/// name = "project-fs"
/// command = "mcp-fs"
/// args = ["--root", "."]
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MergeConfig {
    /// Array-combination strategy. Default: [`ArrayMergeStrategy::Replace`].
    #[serde(default)]
    pub arrays: ArrayMergeStrategy,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Best-effort home directory lookup without pulling in the `dirs` crate.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Deep-merge `overlay` into `base`. Tables always merge recursively (overlay
/// keys win on collision). Arrays follow `arrays`: [`ArrayMergeStrategy::Replace`]
/// swaps the base array for the overlay's, [`ArrayMergeStrategy::Append`]
/// concatenates (base entries first). Scalars are always replaced by the
/// overlay. Used to layer a project-local `.newt/config.toml` over the global
/// config. See issue #222.
pub(crate) fn merge_toml(base: &mut toml::Value, overlay: toml::Value, arrays: ArrayMergeStrategy) {
    match (base, overlay) {
        (toml::Value::Table(base_tbl), toml::Value::Table(overlay_tbl)) => {
            for (key, val) in overlay_tbl {
                match base_tbl.get_mut(&key) {
                    Some(existing) => merge_toml(existing, val, arrays),
                    None => {
                        base_tbl.insert(key, val);
                    }
                }
            }
        }
        // Append mode: concatenate two arrays (global entries first).
        (toml::Value::Array(base_arr), toml::Value::Array(overlay_arr))
            if arrays == ArrayMergeStrategy::Append =>
        {
            base_arr.extend(overlay_arr);
        }
        // Replace mode (and any scalar): the overlay replaces the base outright.
        (slot, overlay) => *slot = overlay,
    }
}

/// Top-level config keys that grant **control-plane authority** — command
/// execution, the exec backend, or inference/data endpoints. A walked-up
/// project `.newt/config.toml` is attacker-reachable (a cloned repo can ship
/// one), so these keys are stripped from an untrusted project overlay before it
/// is merged: a hostile repo cannot silently run a command or redirect the
/// agent's endpoints via config alone. This is data, not logic — extend the
/// table, not the merge code (the three-Cs convention).
///
/// `mcp_servers` is deliberately absent: it has its own literal-only untrusted
/// gate ([`mark_project_mcp_untrusted`] + `McpTrust::Untrusted`), which keeps a
/// project's stdio services usable without ever interpolating `${cmd:…}` or
/// running a ref — a finer treatment than a blanket strip.
pub(crate) const CONTROL_PLANE_KEYS: &[&str] = &[
    "providers",       // `[[providers]]` subprocess plugins — arbitrary command execution
    "lifecycle",       // build / check / lint shell commands — arbitrary command execution
    "shell",           // the shell/exec backend selection (host vs confined)
    "backends",        // inference endpoints — every prompt + context is sent there (exfil)
    "default_backend", // selects the active backend (an attacker-pinned one, if present)
    "discovery",       // backend auto-discovery endpoints (exfil)
    "dgx",             // DGX endpoints + ssh (exfil / remote exec)
    "scratch",         // external scratch paths
    // `[network] owned_suffixes` is the operator's "these hosts are mine"
    // declaration (#1789). It grants no authority, but it decides which
    // endpoints get the patient seven-attempt retry policy instead of the
    // thrifty hosted one — so a repo could make newt hammer a billable
    // third-party endpoint seven times per failure by declaring its suffix
    // owned. Same class as `discovery`: a repo has no business telling the
    // operator which hosts they own.
    "network",
    // `[crews.*].test` / `loop_program` are shell verification commands run on
    // `newt crew` (config/crew.rs Crew.test → WorktreeWorkspace test_cmd → sh -c),
    // and a `[loadouts.*]` with only a model passes validation — so a project
    // overlay could mint a command by declaring the sole crew (auto-selected).
    // Confined by `run_confined_build`, but still config-minted exec authority:
    // strip both so an untrusted overlay cannot introduce a crew/loadout at all.
    "crews",
    "loadouts",
    // `[tui.permissions]` is the SESSION AUTHORITY preset — `to_caveats()` turns
    // it into the caveats the turn runs under (config/permissions.rs mcp_probe_caveats /
    // caveats_for_session). A project overlay setting `preset = "full-access"` /
    // `extra_exec` / `net` would escalate an ordinary interactive turn to
    // `Caveats::top()`. A repo has no business setting the operator's permission
    // authority, so the whole `[tui]` table is stripped from an untrusted config
    // (convergence-audit finding: repo-controlled posture escalation).
    "tui",
];

/// Remove every [`CONTROL_PLANE_KEYS`] entry from an untrusted config table in
/// place, at the `toml::Value` layer — *before* `try_into::<Config>()`, so a
/// stripped key fails closed to the trusted base's value (or the built-in
/// default), never the attacker's. A no-op on a non-table value.
pub(crate) fn strip_control_plane(value: &mut toml::Value) {
    if let Some(table) = value.as_table_mut() {
        for key in CONTROL_PLANE_KEYS {
            table.remove(*key);
        }
    }
}

/// Merge an **untrusted** project overlay over the trusted base, stripping every
/// control-plane key from the overlay first ([`strip_control_plane`]). The
/// replacement for a raw [`merge_toml`] of a walked-up `.newt/config.toml`: the
/// repo can still pin benign, non-control-plane preferences (rules, context
/// tuning, `[merge]` strategy), but never executable/endpoint authority.
pub(crate) fn merge_project_overlay(
    base: &mut toml::Value,
    mut overlay: toml::Value,
    arrays: ArrayMergeStrategy,
) {
    strip_control_plane(&mut overlay);
    merge_toml(base, overlay, arrays);
}

/// Stamp the MCP servers that originated from the walked-up project-local
/// `.newt/config.toml` as [`crate::mcp::McpTrust::Untrusted`] — the #1301 trust
/// boundary for a cloned repo's ambient config.
///
/// By the time [`Config::resolve`] has a typed `Config`, the project entries are
/// already folded into `servers` by [`merge_toml`] and — because `trust` is
/// `#[serde(skip)]` — every entry carries the `Trusted` default, so provenance
/// is reconstructed from the merge shape (which must match [`merge_toml`]):
/// - `project_mcp_count == None` → the project file had no `mcp_servers` key, so
///   `servers` came wholly from the trusted base (user config) → mark none.
/// - [`ArrayMergeStrategy::Replace`] with a project `mcp_servers` array present →
///   the project array REPLACED the base's, so every entry is project-origin.
/// - [`ArrayMergeStrategy::Append`] → the project entries were concatenated
///   AFTER the base's (base first), so only the trailing `count` are
///   project-origin.
///
/// Only ever downgrades (Trusted → Untrusted); it never elevates, so a genuine
/// user-config entry can never be mislabeled trusted by this path.
pub(super) fn mark_project_mcp_untrusted(
    servers: &mut [crate::mcp::McpServerEntry],
    strategy: ArrayMergeStrategy,
    project_mcp_count: Option<usize>,
) {
    let Some(count) = project_mcp_count else {
        return;
    };
    let start = match strategy {
        // Replace swapped the whole array for the project's — all project-origin.
        ArrayMergeStrategy::Replace => 0,
        // Append put the project entries last — mark only that trailing slice.
        ArrayMergeStrategy::Append => servers.len().saturating_sub(count),
    };
    for entry in &mut servers[start..] {
        entry.trust = crate::mcp::McpTrust::Untrusted;
    }
}

/// Whether the resolved base config is the AMBIENT cwd-relative `./newt.toml`
/// candidate (a freshly cloned repo can ship one at its root — the #1301 sibling
/// of the walked-up `.newt/config.toml` vector), as opposed to an
/// operator-explicit base.
///
/// The only base a caller can pin explicitly *through [`Config::resolve`]* is
/// `$NEWT_CONFIG` (the `--config` flag routes through [`Config::load`], which
/// never reaches `resolve`, so it is Trusted without touching this path). So the
/// `./newt.toml` base is explicit — Trusted — ONLY when `$NEWT_CONFIG` points AT
/// it; the implicit fallthrough to the `./newt.toml` candidate (`$NEWT_CONFIG`
/// unset, or set to some other/broken path) is ambient — Untrusted.
pub(super) fn base_is_ambient_newt_toml(base: Option<&Path>) -> bool {
    let ambient_candidate = Path::new("./newt.toml");
    if base != Some(ambient_candidate) {
        return false;
    }
    // Mirror `candidate_paths`' `env::var("NEWT_CONFIG")` read: only a
    // `$NEWT_CONFIG` that *is* `./newt.toml` selected this base explicitly.
    match std::env::var("NEWT_CONFIG") {
        Ok(explicit) => Path::new(&explicit) != ambient_candidate,
        Err(_) => true,
    }
}

/// Determine the array-merge strategy from the raw config values, before they
/// are deserialized. The project config expresses how *it* wants to be merged,
/// so it is consulted first; then the base config; else the built-in default.
pub(super) fn array_merge_strategy(
    project: &toml::Value,
    base: &toml::Value,
) -> ArrayMergeStrategy {
    read_array_strategy(project)
        .or_else(|| read_array_strategy(base))
        .unwrap_or_default()
}

/// Read `[merge] arrays = "replace" | "append"` from a raw config value.
/// Returns `None` when the key is absent or unrecognized (caller falls back).
fn read_array_strategy(value: &toml::Value) -> Option<ArrayMergeStrategy> {
    match value.get("merge")?.get("arrays")?.as_str()? {
        "append" => Some(ArrayMergeStrategy::Append),
        "replace" => Some(ArrayMergeStrategy::Replace),
        _ => None,
    }
}

/// Walk up from `start` looking for a project-local `.newt/config.toml`,
/// stopping before `home` (so the global `~/.newt/config.toml` is never
/// returned) and at the filesystem root. Returns the innermost match.
///
/// Split out from [`Config::project_config_path`] so it can be unit-tested
/// against temp directories without mutating the process environment.
pub(crate) fn find_project_config_from(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        // Never treat the home directory's `.newt` as a project override.
        if home == Some(current) {
            break;
        }
        let candidate = current.join(".newt").join("config.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = current.parent();
    }
    None
}

/// Expand a leading `~/` (or a bare `~`) to the home directory. Paths
/// without a leading tilde are returned unchanged.
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    } else if path == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

/// Walk `start` and its ancestors, returning the first `ancestor.join(rel)` for
/// which `exists` is true. Pure: the filesystem probe is the injected `exists`
/// closure, so the walk logic is unit-testable without touching disk.
pub(crate) fn find_ancestor_dir(
    start: &Path,
    rel: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        let candidate = ancestor.join(rel);
        if exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}
