//! Recover tool calls a weak model emitted in its CONTENT instead of the native
//! `tool_calls` field.
//!
//! The #1 weak-model failure observed in the dgx1 A/B (see
//! `docs/research/weak-model-plan-mode-findings.md`) is *tool-call format*:
//! small models emit the call as text — a fenced/bare JSON object, or a
//! `<function=NAME><parameter=K>V</parameter></function>` block — while the
//! native `tool_calls` array is empty, so the harness never executes it
//! (`tool_calls=0` → nothing runs). Even `qwen3-coder:30b` did this on 2/6 runs.
//!
//! This module recovers those calls into the **native Ollama shape**
//! (`{"function": {"name": …, "arguments": {…}}}`) so they flow into the existing
//! executor and the `is_hallucination` / dup-guard / caveat path unchanged.
//!
//! Authority note (see `docs/decisions/structural_parsing_over_regex.md`): the
//! JSON form is parsed with `serde_json` (structural); the `<function=>` form is
//! parsed by a bounded tag scan (no regex → no ReDoS) with a charset-validated
//! name. Recovery only EXTRACTS `(name, args)`; the authority decision is made
//! downstream by `resolve_tool_alias` + `is_hallucination` + the caveat check, so
//! a spoofed name cannot gain authority here.

use serde_json::{json, Value};

/// Outcome of attempting to recover tool calls from model content.
#[derive(Debug, Default, PartialEq)]
pub struct Recovery {
    /// Recovered calls in native shape (`{"function":{"name","arguments"}}`).
    /// Empty when nothing parseable was found.
    pub calls: Vec<Value>,
    /// The content *looked* like a tool-call attempt (a `<function=` or a
    /// `"name"`+`"arguments"` JSON object) even if it did not fully parse — a
    /// format-hallucination signal the caller counts and steers on.
    pub tool_shaped: bool,
}

/// Charset-validate a recovered tool name (downstream still validates authority).
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Parse a parameter value: a JSON literal (array/object/number/bool/null) if it
/// parses as one, otherwise the trimmed string.
fn param_value(raw: &str) -> Value {
    let t = raw.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        if v.is_array() || v.is_object() || v.is_number() || v.is_boolean() || v.is_null() {
            return v;
        }
    }
    Value::String(t.to_string())
}

fn native(name: &str, args: Value) -> Value {
    json!({ "function": { "name": name, "arguments": args } })
}

/// Recover the `<function=NAME> <parameter=K>V</parameter> … </function>` form.
/// Bounded substring scan — no regex. Multiple blocks are recovered in order.
fn recover_function_tag(content: &str, out: &mut Vec<Value>) -> bool {
    let mut shaped = false;
    let mut rest = content;
    while let Some(open) = rest.find("<function=") {
        shaped = true;
        let after = &rest[open + "<function=".len()..];
        let Some(gt) = after.find('>') else { break };
        let name = after[..gt].trim();
        // Body runs to the matching </function> (or end of string).
        let body_start = open + "<function=".len() + gt + 1;
        let body_end = rest[body_start..]
            .find("</function>")
            .map(|e| body_start + e)
            .unwrap_or(rest.len());
        let body = &rest[body_start..body_end];
        if valid_name(name) {
            let mut args = serde_json::Map::new();
            let mut p = body;
            while let Some(po) = p.find("<parameter=") {
                let pa = &p[po + "<parameter=".len()..];
                let Some(pgt) = pa.find('>') else { break };
                let key = pa[..pgt].trim().to_string();
                let val_start = po + "<parameter=".len() + pgt + 1;
                let Some(pe) = p[val_start..].find("</parameter>") else {
                    break;
                };
                let val = &p[val_start..val_start + pe];
                if !key.is_empty() {
                    args.insert(key, param_value(val));
                }
                p = &p[val_start + pe + "</parameter>".len()..];
            }
            out.push(native(name, Value::Object(args)));
        }
        // Advance past this block.
        rest = &rest[body_end..];
        rest = rest
            .strip_prefix("</function>")
            .or(Some(rest))
            .unwrap_or(rest);
        if rest.is_empty() {
            break;
        }
    }
    shaped
}

/// Recover the JSON-object form: `{"name": "…", "arguments": {…}}`, whether bare
/// or fenced in a ``` block. Scans for balanced `{…}` spans and parses each.
fn recover_json(content: &str, out: &mut Vec<Value>) -> bool {
    let mut shaped = false;
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            // Find the matching close brace (string-aware).
            let mut depth = 0i32;
            let mut j = i;
            let mut in_str = false;
            let mut esc = false;
            while j < bytes.len() {
                let c = bytes[j];
                if in_str {
                    if esc {
                        esc = false;
                    } else if c == b'\\' {
                        esc = true;
                    } else if c == b'"' {
                        in_str = false;
                    }
                } else if c == b'"' {
                    in_str = true;
                } else if c == b'{' {
                    depth += 1;
                } else if c == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j += 1;
            }
            if depth == 0 && j < bytes.len() {
                let span = &content[i..=j];
                if let Ok(v) = serde_json::from_str::<Value>(span) {
                    if let (Some(name), args) =
                        (v.get("name").and_then(|n| n.as_str()), v.get("arguments"))
                    {
                        shaped = true;
                        if valid_name(name) {
                            let a = args.cloned().unwrap_or_else(|| json!({}));
                            // arguments may itself be a JSON string.
                            let a = match a {
                                Value::String(s) => serde_json::from_str(&s)
                                    .unwrap_or(Value::Object(Default::default())),
                                other => other,
                            };
                            out.push(native(name, a));
                        }
                    }
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    shaped
}

/// Recover tool calls from model content. Tries the `<function=>` tag form first
/// (it embeds JSON params), then the JSON-object form. Returns native-shaped
/// calls + whether the content looked tool-shaped at all.
#[must_use]
pub fn recover_tool_calls(content: &str) -> Recovery {
    let mut calls = Vec::new();
    let mut shaped = recover_function_tag(content, &mut calls);
    if calls.is_empty() {
        shaped |= recover_json(content, &mut calls);
    }
    Recovery {
        calls,
        tool_shaped: shaped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_function_tag_form_qwen3_coder_observed() {
        // The exact shape qwen3-coder:30b emitted (and the harness dropped).
        let content = "I'll create a plan.\n\n<function=plan_set>\n<parameter=steps>\n[\"a\", \"b\"]\n</parameter>\n</function>\n</tool_call>";
        let r = recover_tool_calls(content);
        assert!(r.tool_shaped);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0]["function"]["name"], "plan_set");
        assert_eq!(
            r.calls[0]["function"]["arguments"]["steps"],
            json!(["a", "b"])
        );
    }

    #[test]
    fn recovers_fenced_json_form_qwen25_coder_observed() {
        let content = "```json\n{\n  \"name\": \"edit_file\",\n  \"arguments\": {\"path\": \"service.py\", \"old_string\": \"x\", \"new_string\": \"y\"}\n}\n```";
        let r = recover_tool_calls(content);
        assert!(r.tool_shaped);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0]["function"]["name"], "edit_file");
        assert_eq!(r.calls[0]["function"]["arguments"]["path"], "service.py");
    }

    #[test]
    fn recovers_bare_json_object() {
        let content = "{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.rs\"}}";
        let r = recover_tool_calls(content);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0]["function"]["name"], "read_file");
    }

    #[test]
    fn arguments_as_json_string_is_reparsed() {
        let content = "{\"name\": \"read_file\", \"arguments\": \"{\\\"path\\\": \\\"a.rs\\\"}\"}";
        let r = recover_tool_calls(content);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0]["function"]["arguments"]["path"], "a.rs");
    }

    #[test]
    fn multiple_function_tags_recovered_in_order() {
        let content = "<function=plan_advance></function>\n<function=read_file><parameter=path>x.rs</parameter></function>";
        let r = recover_tool_calls(content);
        assert_eq!(r.calls.len(), 2);
        assert_eq!(r.calls[0]["function"]["name"], "plan_advance");
        assert_eq!(r.calls[1]["function"]["name"], "read_file");
        assert_eq!(r.calls[1]["function"]["arguments"]["path"], "x.rs");
    }

    #[test]
    fn plain_prose_is_not_tool_shaped() {
        let r = recover_tool_calls("I have finished editing the file and the tests pass.");
        assert!(r.calls.is_empty());
        assert!(!r.tool_shaped);
    }

    #[test]
    fn narrated_intent_to_act_is_not_a_dropped_call() {
        // Real weak-model (ornith:35b) narrations that ended a turn with no tool
        // call: a plan announcement, not a mis-formatted call. Recovery finds
        // nothing AND does not flag tool_shaped, so the loop's format-
        // hallucination path stays quiet — proving the "narrate-then-stop" stall
        // is a turn-termination gap, not a parse drop. The fix belongs in the
        // loop's `!has_tools` branch (see `narration_action_nudge`), not here.
        for prose in [
            "Now I have everything I need. Let me make the two edits: \
             1. newt-cli/src/lib.rs ... 2. newt-core/src/config.rs ... \
             Let me make both edits now.",
            "Now I'll add the --home flag to the Cli struct. \
             I'll place it near the other directory-related options:",
        ] {
            let r = recover_tool_calls(prose);
            assert!(
                r.calls.is_empty(),
                "narration must yield no call: {prose:?}"
            );
            assert!(
                !r.tool_shaped,
                "narration must not look tool-shaped: {prose:?}"
            );
        }
    }

    #[test]
    fn json_without_name_arguments_is_not_a_call() {
        // A JSON blob that isn't a tool call (e.g. a config snippet) is ignored.
        let r = recover_tool_calls("Here is config: {\"debug\": true, \"level\": 3}");
        assert!(r.calls.is_empty());
    }

    #[test]
    fn malformed_function_tag_is_shaped_but_yields_no_call() {
        // Names the attempt (tool_shaped) but the bad name yields nothing — the
        // caller counts this as a format-hallucination and steers.
        let r = recover_tool_calls("<function=not a valid name!>oops");
        assert!(r.tool_shaped);
        assert!(r.calls.is_empty());
    }

    #[test]
    fn name_charset_is_validated() {
        assert!(valid_name("edit_file"));
        assert!(valid_name("plan-advance"));
        assert!(!valid_name(""));
        assert!(!valid_name("rm -rf /"));
        assert!(!valid_name("a; drop table"));
    }
}
