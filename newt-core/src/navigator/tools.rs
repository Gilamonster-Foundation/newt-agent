//! Narrow navigator tools (#1387) — advertised with `Gate::Always`, degrade
//! honestly when session indexes are absent. `code_search` / `where_is` keep
//! their existing names for compatibility.

use serde_json::json;

use crate::agentic::semantic::IndexStatus;
use crate::project_model::ProjectModel;
use crate::where_is::WhereIsIndex;

use super::{
    find_callees, find_callers, find_hierarchy, find_implementations, find_references, find_tests,
    goto_definition, impact_analysis, inspect_type, text_search, GotoDefinitionArgs, GraphIndex,
    NavResult, UsageIndex,
};

/// Tool names registered for navigation (#1387).
pub const NAV_TOOL_NAMES: &[&str] = &[
    "goto_definition",
    "text_search",
    "find_references",
    "find_tests",
    "find_callers",
    "find_callees",
    "find_implementations",
    "find_hierarchy",
    "inspect_type",
    "impact",
];

fn tool(name: &str, description: &str, prop: &str, prop_desc: &str) -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {
                    prop: { "type": "string", "description": prop_desc }
                },
                "required": [prop]
            }
        }
    })
}

#[must_use]
pub fn goto_definition_tool_definition() -> serde_json::Value {
    tool(
        "goto_definition",
        "Locate exactly where a symbol is DEFINED ([SYMBOL]). Typed verdict via where_is — not a ranked guess. Prefer over grep when you know the name.",
        "symbol",
        "Exact symbol name, e.g. run_crew",
    )
}

#[must_use]
pub fn text_search_tool_definition() -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": "text_search",
            "description": "Lexical regex search across the workspace ([LEXICAL]). Use for exact strings/patterns; use code_search for meaning. Scope with `path` to cut noise — hits inside string literals are tagged as quoted text, not code.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Regex pattern to search for" },
                    "path": { "type": "string", "description": "Optional workspace-relative file or directory to scope the search (e.g. 'newt-core/src' or 'src/lib.rs'). Omit for the whole workspace." }
                },
                "required": ["query"]
            }
        }
    })
}

#[must_use]
pub fn find_references_tool_definition() -> serde_json::Value {
    tool(
        "find_references",
        "Find heuristic USAGE sites of a symbol ([SYMBOL] usage-index). Regex-floor name matches — not compiler-resolved references.",
        "symbol",
        "Symbol whose uses to find",
    )
}

#[must_use]
pub fn find_tests_tool_definition() -> serde_json::Value {
    tool(
        "find_tests",
        "Find heuristic test files/sites related to a symbol ([SYMBOL]). Path/name heuristic — not a test-runner inventory.",
        "symbol",
        "Symbol to find tests for",
    )
}

#[must_use]
pub fn find_callers_tool_definition() -> serde_json::Value {
    tool(
        "find_callers",
        "Heuristic GRAPH callers of a function ([GRAPH], analyzer=regex-floor). Call-name match — complete=false; not a typechecker.",
        "symbol",
        "Callee symbol name",
    )
}

#[must_use]
pub fn find_callees_tool_definition() -> serde_json::Value {
    tool(
        "find_callees",
        "Heuristic GRAPH callees inside a function body ([GRAPH], analyzer=regex-floor). complete=false when weak.",
        "symbol",
        "Caller symbol whose body to scan",
    )
}

#[must_use]
pub fn find_implementations_tool_definition() -> serde_json::Value {
    tool(
        "find_implementations",
        "Heuristic GRAPH impl…for… rows for a trait or type ([GRAPH], regex-floor).",
        "symbol",
        "Trait or type name",
    )
}

#[must_use]
pub fn find_hierarchy_tool_definition() -> serde_json::Value {
    tool(
        "find_hierarchy",
        "Heuristic GRAPH hierarchy via impl…for… projection ([GRAPH]). No supertrait expansion.",
        "symbol",
        "Trait or type name",
    )
}

#[must_use]
pub fn inspect_type_tool_definition() -> serde_json::Value {
    tool(
        "inspect_type",
        "Show defining snippet + kind + file:line for a symbol. NOT typechecker-proved (regex-floor).",
        "symbol",
        "Symbol / type name to inspect",
    )
}

#[must_use]
pub fn impact_tool_definition() -> serde_json::Value {
    tool(
        "impact",
        "Outbound + reverse deps from the project model for a crate/unit, plus optional lcov join. Not a full call graph; #1282 persistent index omitted.",
        "unit",
        "Crate / package / unit name",
    )
}

/// Session handles needed to execute navigator tools.
#[derive(Clone, Copy)]
pub struct NavToolCtx<'a> {
    pub workspace: &'a str,
    pub where_is: Option<&'a WhereIsIndex>,
    pub usage: Option<&'a UsageIndex>,
    pub graph: Option<&'a GraphIndex>,
    pub project: Option<&'a ProjectModel>,
    pub files: Option<&'a [(String, String)]>,
    pub status: Option<&'a IndexStatus>,
}

fn index_id(ctx: &NavToolCtx<'_>) -> String {
    ctx.status
        .map(|s| s.index_id())
        .or_else(|| ctx.usage.map(|u| u.index_id().to_string()))
        .or_else(|| ctx.graph.map(|g| g.index_id().to_string()))
        .unwrap_or_else(|| "gen0".into())
}

fn missing(tool: &str, need: &str) -> String {
    format!(
        "error: {tool} unavailable — {need} not built this session (try a turn first, or /new to re-index)"
    )
}

/// Execute a navigator tool by name. Returns `None` when `name` is not a nav tool.
#[must_use]
pub fn execute_nav_tool(
    name: &str,
    args: &serde_json::Value,
    ctx: &NavToolCtx<'_>,
) -> Option<String> {
    if !NAV_TOOL_NAMES.contains(&name) {
        return None;
    }
    let id = index_id(ctx);
    let out: NavResult = match name {
        "goto_definition" => {
            let symbol = args["symbol"].as_str().unwrap_or("").trim();
            let Some(idx) = ctx.where_is else {
                return Some(missing(name, "where_is index"));
            };
            goto_definition(
                idx,
                GotoDefinitionArgs {
                    symbol,
                    kind: args["kind"].as_str(),
                    index_id: &id,
                    files: ctx.files,
                },
            )
        }
        "text_search" => {
            let query = args["query"].as_str().unwrap_or("").trim();
            // Iteration #6: honor the model's `path` scope (fenced; honest
            // warning when missing) instead of silently searching everything.
            super::text::text_search_scoped(
                query,
                std::path::Path::new(ctx.workspace),
                args["path"].as_str(),
                &id,
            )
        }
        "find_references" => {
            let symbol = args["symbol"].as_str().unwrap_or("").trim();
            let Some(idx) = ctx.usage else {
                return Some(missing(name, "usage index"));
            };
            find_references(idx, symbol)
        }
        "find_tests" => {
            let symbol = args["symbol"].as_str().unwrap_or("").trim();
            let Some(idx) = ctx.usage else {
                return Some(missing(name, "usage index"));
            };
            find_tests(idx, symbol)
        }
        "find_callers" => {
            let symbol = args["symbol"].as_str().unwrap_or("").trim();
            let Some(idx) = ctx.graph else {
                return Some(missing(name, "graph index"));
            };
            find_callers(idx, symbol)
        }
        "find_callees" => {
            let symbol = args["symbol"].as_str().unwrap_or("").trim();
            let Some(idx) = ctx.graph else {
                return Some(missing(name, "graph index"));
            };
            find_callees(idx, symbol)
        }
        "find_implementations" => {
            let symbol = args["symbol"].as_str().unwrap_or("").trim();
            let Some(idx) = ctx.graph else {
                return Some(missing(name, "graph index"));
            };
            find_implementations(idx, symbol)
        }
        "find_hierarchy" => {
            let symbol = args["symbol"].as_str().unwrap_or("").trim();
            let Some(idx) = ctx.graph else {
                return Some(missing(name, "graph index"));
            };
            find_hierarchy(idx, symbol)
        }
        "inspect_type" => {
            let symbol = args["symbol"].as_str().unwrap_or("").trim();
            let files = ctx.files.unwrap_or(&[]);
            inspect_type(symbol, files, ctx.where_is, &id)
        }
        "impact" => {
            let unit = args["unit"]
                .as_str()
                .or_else(|| args["symbol"].as_str())
                .unwrap_or("")
                .trim();
            let Some(model) = ctx.project else {
                return Some(missing(name, "project model"));
            };
            let files = ctx.files.unwrap_or(&[]);
            let report = impact_analysis(unit, model, files, std::path::Path::new(ctx.workspace));
            return Some(report.render());
        }
        _ => return None,
    };
    Some(out.render())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_tool_definitions_named() {
        for (name, def) in [
            ("goto_definition", goto_definition_tool_definition()),
            ("text_search", text_search_tool_definition()),
            ("find_references", find_references_tool_definition()),
            ("find_tests", find_tests_tool_definition()),
            ("find_callers", find_callers_tool_definition()),
            ("find_callees", find_callees_tool_definition()),
            (
                "find_implementations",
                find_implementations_tool_definition(),
            ),
            ("find_hierarchy", find_hierarchy_tool_definition()),
            ("inspect_type", inspect_type_tool_definition()),
            ("impact", impact_tool_definition()),
        ] {
            assert_eq!(def["function"]["name"], name);
        }
        assert_eq!(NAV_TOOL_NAMES.len(), 10);
    }
}
