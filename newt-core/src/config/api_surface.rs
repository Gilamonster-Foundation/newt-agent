//! Language-pack schema and budgets for workspace API-surface extraction.

use serde::{Deserialize, Serialize};

/// One symbol-extraction rule in a [`LanguagePack`]: a regex over a single source
/// line whose **first capture group is the public symbol's name**, plus a
/// free-form kind label. Free-form so a pack is not locked to one language's
/// vocabulary (`fn`/`struct` for Rust, `class`/`def` for Python, `func` for Go…).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRule {
    /// Regex; capture group 1 = the symbol name.
    pub pattern: String,
    /// Kind shown in the surface (e.g. `"fn"`, `"struct"`, `"class"`, `"func"`).
    pub kind: String,
}

/// A **language pack** for the workspace API surface (#669): how to recognize a
/// language's files, which files expose its public API, and how to extract its
/// public symbols — entirely as DATA, so a new language is config, not code.
///
/// Built-in packs cover common source languages. A project ships more by
/// dropping a `<name>.toml` into `~/.newt/language-packs/` (global) or
/// `.newt/language-packs/` (project-local), or inline under
/// `[[context.api_surface.language_packs]]`. Packs merge **by `name`** (a custom
/// pack with a built-in's name replaces it), so anyone can add Java, Ruby, Swift,
/// Objective-C, … without touching the binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguagePack {
    /// Stable id (a config pack with a built-in's name replaces that built-in).
    pub name: String,
    /// Human spellings accepted by harness source-file classification, e.g.
    /// `["c++", "cpp"]` or `["c#", "dotnet"]`. The stable `name` is always an
    /// implicit alias. Pure data keeps language understanding out of prompt-
    /// specific conditionals.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// File extensions this pack claims, no dot — `["rs"]`, `["h", "hpp", "cpp"]`.
    pub extensions: Vec<String>,
    /// Entry-point filename globs (the public-API files, listed first in the
    /// surface). Supported globs: exact (`lib.rs`), suffix (`*.h`), or all (`*`).
    /// Empty ⇒ no file is prioritized for this pack.
    #[serde(default)]
    pub entry_points: Vec<String>,
    /// Public-symbol extraction rules, applied per source line.
    pub symbols: Vec<SymbolRule>,
}

/// `[context.api_surface]` — the workspace-API-surface knowledge_base technique.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiSurfaceConfig {
    /// Inline language packs, merged by `name` over the built-ins and the
    /// drop-in directories (the highest-precedence layer).
    #[serde(default)]
    pub language_packs: Vec<LanguagePack>,
    /// **Deprecated** operator pin. When present, the tier-2 budget is pinned to
    /// this char count (`floor_chars == ceiling_chars == max_block_chars`) — the
    /// legacy fixed cap. Prefer the proportional trio below (spec §3, SC-L2).
    /// Absent by default so the surface scales with the discovered window.
    #[serde(default)]
    pub max_block_chars: Option<usize>,
    /// SC-L2 floor: the minimum tier-2 char allowance, even on a tiny window —
    /// the surface must never be starved to nothing (dominates near ~8k tokens).
    #[serde(default = "default_api_surface_floor_chars")]
    pub floor_chars: usize,
    /// SC-L2 slope: percent of the resolved send budget `w` (tokens) the tier-2
    /// surface may claim, before the chars/token conversion and clamp.
    #[serde(default = "default_api_surface_pct_of_budget")]
    pub pct_of_budget: usize,
    /// SC-L2 ceiling — a §8 *pin*, not law: the max tier-2 char allowance on a
    /// large window; the v1 value is set empirically by the #548 map-size arms.
    #[serde(default = "default_api_surface_ceiling_chars")]
    pub ceiling_chars: usize,
    /// Per-file symbol cap, so one huge file can't crowd out the surface.
    #[serde(default = "default_api_surface_max_symbols_per_file")]
    pub max_symbols_per_file: usize,
}

impl Default for ApiSurfaceConfig {
    fn default() -> Self {
        Self {
            language_packs: Vec::new(),
            max_block_chars: None,
            floor_chars: default_api_surface_floor_chars(),
            pct_of_budget: default_api_surface_pct_of_budget(),
            ceiling_chars: default_api_surface_ceiling_chars(),
            max_symbols_per_file: default_api_surface_max_symbols_per_file(),
        }
    }
}

// SC-L2 pins (spec §8). Defaults chosen so the floor dominates at the
// DEFAULT_CONTEXT_TOKENS=8,192 fallback (8192·5% ·4 = 1,638 < 2,000) and the
// ceiling caps a 262k-window session (its ~168k send budget · 5% · 4 ≫ 24,000).
fn default_api_surface_floor_chars() -> usize {
    2_000
}

fn default_api_surface_pct_of_budget() -> usize {
    5
}

fn default_api_surface_ceiling_chars() -> usize {
    24_000
}

fn default_api_surface_max_symbols_per_file() -> usize {
    12
}
