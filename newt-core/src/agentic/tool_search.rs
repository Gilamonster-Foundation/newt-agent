//! The `tool_search` discovery seam (#725) — let the model find a *real* tool
//! by intent instead of fabricating a foreign name and burning a round.
//!
//! This is the structural complement to the alias seam (#716, which catches
//! foreign names *after* the fact) and the phantom-reach telemetry (#717, which
//! measures *which* names get fabricated). Where those react to a guess,
//! `tool_search` lets a model that only half-remembers a capability *look it up*
//! first: given a query it returns matching real tool names + a one-line
//! description (and required params) drawn from the session's own advertised
//! catalog — so `read_file` / `update_plan` / `crew` are discoverable by
//! description rather than by lucky recall of an exact name.
//!
//! Out of scope (per #725): lazy/deferred tool loading. newt advertises every
//! tool today; this is *discovery over the advertised set*, not lazy-loading.
//!
//! [`execute_tool_search`] is deliberately PURE — it takes the catalog as a
//! `&serde_json::Value` (the same shape [`super::tools::merged_tool_definitions`]
//! produces) plus the query, so it is unit-testable without a live session. The
//! dispatch arm in `tools.rs` builds the catalog for THIS session and does the
//! on-screen print; the matching logic lives here.

/// Model-facing contract for `tool_search`. Coaches a small local LLM that is
/// unsure of a tool's exact name to *look it up* rather than guess a name from
/// another harness.
const TOOL_SEARCH_DESCRIPTION: &str =
    "Search newt's available tools by intent/keyword — returns matching real \
     tool names + what they do. Use this when unsure of a tool's exact name \
     instead of guessing.";

/// Max number of ranked matches returned for a query (keeps the payload compact
/// for a small model's window; a too-broad query is told to refine).
const MAX_MATCHES: usize = 12;

/// Truncate a one-line description to this many chars (keeps each match to a
/// single readable line).
const DESC_MAX_CHARS: usize = 140;

/// The `tool_search` tool definition. Advertised ALWAYS (a discovery tool must
/// always be present, like `resume_context`) — `tools.rs` pushes it
/// unconditionally in `merged_tool_definitions`.
pub fn tool_search_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "tool_search",
            "description": TOOL_SEARCH_DESCRIPTION,
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Intent or keyword(s) to search the tool catalog by — \
                                        e.g. \"read a file\", \"plan\", \"delegate\"."
                    }
                },
                "required": ["query"]
            }
        }
    })
}

/// One advertised tool, projected out of a catalog entry for matching.
struct ToolRec<'a> {
    name: &'a str,
    desc: &'a str,
    params: Vec<&'a str>,
    required: Vec<&'a str>,
}

/// Collapse whitespace (incl. embedded newlines from wrapped string literals)
/// into single spaces and truncate to `max` chars, so each match renders on one
/// line. Pure.
fn one_line(s: &str, max: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max {
        let head: String = collapsed.chars().take(max).collect();
        format!("{}…", head.trim_end())
    } else {
        collapsed
    }
}

/// Project a catalog (`[{type,function:{name,description,parameters}}]`) into the
/// matchable [`ToolRec`]s, skipping `tool_search` itself (a discovery tool need
/// not return itself, which would only invite recursion confusion).
fn records(catalog: &serde_json::Value) -> Vec<ToolRec<'_>> {
    let entries = catalog.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let f = &e["function"];
        let Some(name) = f["name"].as_str() else {
            continue;
        };
        if name == "tool_search" {
            continue;
        }
        let desc = f["description"].as_str().unwrap_or("");
        let params: Vec<&str> = f["parameters"]["properties"]
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let required: Vec<&str> = f["parameters"]["required"]
            .as_array()
            .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        out.push(ToolRec {
            name,
            desc,
            params,
            required,
        });
    }
    out
}

/// Format one match line: `- name — one-line description [requires: a, b]`.
fn match_line(r: &ToolRec) -> String {
    let desc = one_line(r.desc, DESC_MAX_CHARS);
    if r.required.is_empty() {
        format!("- {} — {desc}", r.name)
    } else {
        format!(
            "- {} — {desc} [requires: {}]",
            r.name,
            r.required.join(", ")
        )
    }
}

/// Search the advertised tool `catalog` by `query` and render a compact, ranked
/// list of matching REAL tool names + what they do. Pure (no fs / no global) —
/// unit-tested directly.
///
/// Matching is case-insensitive over each tool's **name + description + param
/// names**: a query term in the name scores highest, a hit in the description or
/// a param name scores lower, summed across the query's terms. Matches are
/// returned ranked (score desc, then name asc for determinism), capped at
/// [`MAX_MATCHES`].
///
/// Discovery never dead-ends:
/// - an empty query lists every advertised tool with its description, and
/// - a query that matches nothing returns the full list of tool **names**
///   (so the model always gets a real next step, never an empty result).
pub(crate) fn execute_tool_search(query: &str, catalog: &serde_json::Value) -> String {
    let tools = records(catalog);

    let terms: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();

    // Empty / term-less query: list the whole catalog with descriptions. A blank
    // probe is a legitimate "what can you do?" — answer it fully.
    if terms.is_empty() {
        if tools.is_empty() {
            return "No tools are available this session.".to_string();
        }
        let body = tools.iter().map(match_line).collect::<Vec<_>>().join("\n");
        return format!("Available tools:\n{body}");
    }

    // Score each tool over name (×3) + description (×1) + param names (×1).
    let mut scored: Vec<(i64, &ToolRec)> = Vec::new();
    for r in &tools {
        let name_l = r.name.to_lowercase();
        let desc_l = r.desc.to_lowercase();
        let params_l: Vec<String> = r.params.iter().map(|p| p.to_lowercase()).collect();
        let mut score = 0i64;
        for term in &terms {
            if name_l.contains(term.as_str()) {
                score += 3;
            }
            if desc_l.contains(term.as_str()) {
                score += 1;
            }
            if params_l.iter().any(|p| p.contains(term.as_str())) {
                score += 1;
            }
        }
        if score > 0 {
            scored.push((score, r));
        }
    }

    // No match: return the full NAME list so discovery still succeeds.
    if scored.is_empty() {
        let names = tools.iter().map(|r| r.name).collect::<Vec<_>>().join(", ");
        if names.is_empty() {
            return format!("No tool matched \"{query}\" — no tools are available this session.");
        }
        return format!(
            "No tool matched \"{query}\". Available tools: {names}. Retry tool_search with a \
             different keyword, or call one of these directly."
        );
    }

    // Rank: score desc, then name asc (stable, deterministic).
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(b.1.name)));

    let mut out = format!("Tools matching \"{query}\":\n");
    for (_, r) in scored.iter().take(MAX_MATCHES) {
        out.push_str(&match_line(r));
        out.push('\n');
    }
    if scored.len() > MAX_MATCHES {
        out.push_str(&format!(
            "…and {} more — refine the query.",
            scored.len() - MAX_MATCHES
        ));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic::tools::merged_tool_definitions;
    use crate::agentic::NoMcp;

    /// The full advertised catalog with every presence gate ON, so crew /
    /// plan / scratchpad / etc. all appear — the richest catalog to search.
    fn full_catalog() -> serde_json::Value {
        merged_tool_definitions(&NoMcp, true, true, true, true, true, true, true, true, true)
    }

    #[test]
    fn search_read_includes_read_file() {
        let out = execute_tool_search("read", &full_catalog());
        assert!(out.contains("read_file"), "got: {out}");
    }

    #[test]
    fn search_plan_includes_update_plan() {
        // `update_plan` (the scheduled write tool) is advertised with the gate on.
        let out = execute_tool_search("plan", &full_catalog());
        assert!(out.contains("update_plan"), "got: {out}");
    }

    #[test]
    fn search_description_only_match() {
        // "team" appears in the crew / compose_roster DESCRIPTIONS but in no tool
        // NAME — proves description matching works, not just name matching.
        let out = execute_tool_search("team", &full_catalog());
        assert!(
            out.contains("crew") || out.contains("compose_roster"),
            "description-only match should find crew/compose_roster: {out}"
        );
    }

    #[test]
    fn no_match_returns_full_name_list_not_dead_end() {
        let out = execute_tool_search("zzzznotarealtoolname", &full_catalog());
        // Never empty / a dead end — the full name list is returned so the model
        // can still pick a real tool.
        assert!(out.contains("read_file"), "got: {out}");
        assert!(out.contains("write_file"), "got: {out}");
        assert!(!out.trim().is_empty());
    }

    #[test]
    fn search_is_case_insensitive() {
        let cat = full_catalog();
        let lower = execute_tool_search("read", &cat);
        let upper = execute_tool_search("READ", &cat);
        assert!(upper.contains("read_file"), "got: {upper}");
        // The header echoes the raw query (case preserved), so compare the match
        // bodies (everything after the first line) — they must be identical.
        let body = |s: &str| {
            s.split_once('\n')
                .map(|(_, b)| b.to_string())
                .unwrap_or_default()
        };
        assert_eq!(
            body(&lower),
            body(&upper),
            "matched tools must be identical regardless of query case"
        );
    }

    #[test]
    fn does_not_list_itself() {
        // A discovery tool need not return itself.
        let out = execute_tool_search("search", &full_catalog());
        assert!(
            !out.contains("- tool_search "),
            "tool_search should not appear in its own results: {out}"
        );
    }

    #[test]
    fn empty_query_lists_everything() {
        let out = execute_tool_search("", &full_catalog());
        assert!(out.contains("Available tools:"), "got: {out}");
        assert!(out.contains("read_file"), "got: {out}");
        assert!(out.contains("write_file"), "got: {out}");
    }

    #[test]
    fn match_line_renders_required_params() {
        // read_file requires `path`; its match line should surface that.
        let out = execute_tool_search("read_file", &full_catalog());
        assert!(out.contains("read_file"), "got: {out}");
        assert!(out.contains("requires:"), "required params shown: {out}");
    }

    #[test]
    fn one_line_collapses_whitespace_and_truncates() {
        let collapsed = one_line("a\n   b\t c", 100);
        assert_eq!(collapsed, "a b c");
        let truncated = one_line(&"x".repeat(200), 10);
        assert!(truncated.chars().count() <= 11, "got: {truncated}"); // 10 + ellipsis
        assert!(truncated.ends_with('…'));
    }
}
