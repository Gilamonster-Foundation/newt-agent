//! The **project map** — the untruncatable crate/package map rendered into the
//! frozen system-prompt head (#1284, epic #1277; spec `semantic-cheat.md` §5.2
//! / `ide-project-model.md`). The always-present navigation floor: one line per
//! unit, so a model (small ones especially) starts every turn oriented — it
//! knows the units exist and where they live before spending a single tool
//! round.
//!
//! Two sources, merged by name (SC-L1, later-wins):
//! 1. the **derived** project model ([`crate::project_model::scan_project`]) —
//!    the auto-derived structure of *any* build system (Cargo/pyproject/pubspec/
//!    package.json);
//! 2. the hand-written **seed** (`.newt/workspace-map.toml`) — curated one-line
//!    `purpose` cards that a derive can't infer.
//!
//! SC-L4·i: the map is a **verbatim projection** — never LLM-generated,
//! summarised, or paraphrased. It rides the frozen head, so it is never trimmed
//! by compaction (SC-L4·ii untruncatable). Pure [`render_project_map`] + a thin
//! fs [`ProjectMapProvider`].

use async_trait::async_trait;
use serde::Deserialize;
use std::path::Path;

use crate::memory::{MemMessage, MemoryProvider, SessionContext};
use crate::metrics::TurnMetrics;
use crate::project_model::{
    builtin_project_packs, scan_project, scan_project_cached, ProjectModel,
};

/// One curated pointer card from `.newt/workspace-map.toml` (spec §3.1). The
/// `purpose` is the human one-line description a derive cannot infer.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SeedCard {
    pub name: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub entry: Option<String>,
}

/// The `.newt/workspace-map.toml` document: `v = 1` + a `[[crate]]` list.
#[derive(Debug, Clone, Default, Deserialize)]
struct SeedDoc {
    #[serde(rename = "crate", default)]
    crates: Vec<SeedCard>,
}

/// Load the repo-local seed (`<workspace>/.newt/workspace-map.toml`). Missing or
/// malformed ⇒ no seed cards (the derived model stands alone). Thin fs wrapper.
#[must_use]
pub fn load_seed(workspace: &Path) -> Vec<SeedCard> {
    let path = workspace.join(".newt").join("workspace-map.toml");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<SeedDoc>(&text).ok())
        .map(|doc| doc.crates)
        .unwrap_or_default()
}

/// Render the untruncatable project-map block — **pure**. One line per unit; the
/// seed's curated `purpose` (merged by name) wins, else the derived structure
/// (source roots + dependency count). `None` when there are no units (a
/// non-project dir contributes nothing).
#[must_use]
pub fn render_project_map(model: &ProjectModel, seed: &[SeedCard]) -> Option<String> {
    if model.units.is_empty() {
        return None;
    }
    let mut out = format!(
        "[PROJECT MAP — {} units ({}) in this workspace, authoritative and \
         untruncatable. Use these names + paths to navigate; read_file a path for detail.]\n",
        model.units.len(),
        model.pack
    );
    for unit in &model.units {
        let card = seed.iter().find(|c| c.name == unit.name);
        out.push_str("- ");
        out.push_str(&unit.name);
        if unit.dir != "." {
            out.push_str(&format!(" ({})", unit.dir));
        }
        match card {
            // Curated purpose (SC-L1) — the human one-liner a derive can't infer.
            Some(c) if !c.purpose.is_empty() => {
                out.push_str(" — ");
                out.push_str(&c.purpose);
            }
            // Else the derived structure: source roots + dependency count.
            _ => {
                let roots = if unit.source_roots.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", unit.source_roots.join(", "))
                };
                out.push_str(&format!("{roots} · {} deps", unit.deps.len()));
            }
        }
        out.push('\n');
    }
    Some(out)
}

/// A [`MemoryProvider`] that renders the project map into the frozen head
/// (#1284). Scans the workspace once at session start (via the drift-cache when
/// a config dir resolves, else a plain scan), merges the seed, and returns the
/// untruncatable block — mirroring `ApiSurfaceProvider`.
#[derive(Default)]
pub struct ProjectMapProvider {
    block: Option<String>,
}

impl ProjectMapProvider {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MemoryProvider for ProjectMapProvider {
    fn name(&self) -> &str {
        "project_map"
    }

    async fn initialize(&mut self, ctx: &SessionContext) -> anyhow::Result<()> {
        let root = Path::new(&ctx.workspace);
        let packs = builtin_project_packs();
        // Prefer the drift-cached scan (fast on a re-launch); fall back to a
        // plain scan when the config dir is unavailable.
        let model: Option<ProjectModel> = crate::Config::user_config_dir()
            .and_then(|cfg| scan_project_cached(root, &packs, &cfg))
            .or_else(|| scan_project(root, &packs));
        self.block = model.and_then(|m| render_project_map(&m, &load_seed(root)));
        Ok(())
    }

    fn system_prompt_block(&self) -> Option<String> {
        self.block.clone()
    }

    /// A block-only provider (like `ApiSurfaceProvider`): it contributes the
    /// frozen-head map, no conversation messages and no per-turn state.
    fn build_messages(&self, _system_prompt: &str, _new_task: &str) -> Vec<MemMessage> {
        Vec::new()
    }

    async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_model::ProjectUnit;

    fn unit(name: &str, dir: &str, roots: &[&str], ndeps: usize) -> ProjectUnit {
        ProjectUnit {
            name: name.to_string(),
            dir: dir.to_string(),
            source_roots: roots.iter().map(|s| s.to_string()).collect(),
            deps: (0..ndeps).map(|i| format!("d{i}")).collect(),
            languages: vec!["rust".into()],
        }
    }

    fn model(units: Vec<ProjectUnit>) -> ProjectModel {
        ProjectModel {
            pack: "rust".into(),
            units,
        }
    }

    #[test]
    fn render_uses_seed_purpose_then_falls_back_to_derived_structure() {
        let m = model(vec![
            unit("newt-core", "newt-core", &["src"], 29),
            unit("newt-tui", "newt-tui", &["src"], 23),
        ]);
        let seed = vec![SeedCard {
            name: "newt-core".into(),
            purpose: "agent loop, tools, OCAP".into(),
            paths: vec![],
            entry: None,
        }];
        let block = render_project_map(&m, &seed).unwrap();
        // Seeded unit shows the curated purpose; unseeded shows derived structure.
        assert!(
            block.contains("- newt-core (newt-core) — agent loop, tools, OCAP"),
            "{block}"
        );
        assert!(
            block.contains("- newt-tui (newt-tui) [src] · 23 deps"),
            "{block}"
        );
        // The header names the pack and the count.
        assert!(block.contains("2 units"), "{block}");
    }

    #[test]
    fn render_is_none_for_an_empty_model() {
        assert!(render_project_map(&model(vec![]), &[]).is_none());
    }

    #[test]
    fn render_is_pure_and_deterministic() {
        let m = model(vec![unit("a", ".", &["src"], 1)]);
        assert_eq!(render_project_map(&m, &[]), render_project_map(&m, &[]));
        // A single-unit project at root omits the redundant "(.)" dir.
        let b = render_project_map(&m, &[]).unwrap();
        assert!(b.contains("- a [src] · 1 deps"), "{b}");
        assert!(!b.contains("(.)"), "{b}");
    }

    #[test]
    fn seed_doc_parses_the_workspace_map_shape() {
        let toml = "v = 1\n[[crate]]\nname = \"newt-core\"\npurpose = \"the loop\"\npaths = [\"newt-core/src/\"]\nentry = \"newt-core/src/lib.rs\"\n";
        let doc: SeedDoc = toml::from_str(toml).unwrap();
        assert_eq!(doc.crates.len(), 1);
        assert_eq!(doc.crates[0].name, "newt-core");
        assert_eq!(doc.crates[0].purpose, "the loop");
    }
}
