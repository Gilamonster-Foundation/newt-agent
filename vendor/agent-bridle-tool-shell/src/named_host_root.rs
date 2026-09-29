//! Trusted-parent selection for an explicitly granted literal host root.

use std::path::{Component, Path};

use agent_bridle_core::{Caveats, Scope};
use brush_parser::ast::{Command, CommandPrefixOrSuffixItem, SeparatorOperator};
use brush_parser::{Parser, ParserOptions};

use crate::shell_inspect::{static_shell_word, ShellInspectionError, MAX_SOURCE_BYTES};

/// Lower one synchronous, literal, absolute command without evaluating it.
///
/// Returns the root spelling and arguments after `argv[0]`. This describes
/// syntax only and grants no authority. The parent must still require the
/// exact root in the effective exec allowlist and obtain NamedRoot admission.
/// Valid compound or dynamic syntax returns `Ok(None)` and keeps the ordinary
/// Brush route. Parent-directory traversal likewise selects no new operation.
///
/// # Errors
///
/// Returns the normal parse error for malformed Brush syntax.
pub fn lower_named_host_root_command(
    source: &str,
) -> Result<Option<(String, Vec<String>)>, ShellInspectionError> {
    // Keep large requests on the ordinary route, which owns its existing
    // worker input bounds, rather than adding unbounded parent-side parsing.
    if source.len() > MAX_SOURCE_BYTES {
        return Ok(None);
    }
    let options = ParserOptions::default();
    let mut parser = Parser::new(std::io::Cursor::new(source.as_bytes()), &options);
    let program = parser
        .parse_program()
        .map_err(|error| ShellInspectionError::new(format!("malformed shell syntax: {error}")))?;
    let [complete] = program.complete_commands.as_slice() else {
        return Ok(None);
    };
    let [item] = complete.0.as_slice() else {
        return Ok(None);
    };
    if matches!(item.1, SeparatorOperator::Async) || !item.0.additional.is_empty() {
        return Ok(None);
    }
    let pipeline = &item.0.first;
    if pipeline.bang || pipeline.timed.is_some() {
        return Ok(None);
    }
    let [Command::Simple(simple)] = pipeline.seq.as_slice() else {
        return Ok(None);
    };
    if simple
        .prefix
        .as_ref()
        .is_some_and(|prefix| !prefix.0.is_empty())
    {
        return Ok(None);
    }
    let Some(word) = &simple.word_or_name else {
        return Ok(None);
    };
    let Ok(root) = static_shell_word(word, &options) else {
        return Ok(None);
    };
    if !Path::new(&root).is_absolute()
        || Path::new(&root)
            .components()
            .any(|part| part == Component::ParentDir)
    {
        return Ok(None);
    }
    let mut argv = Vec::new();
    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            let CommandPrefixOrSuffixItem::Word(word) = item else {
                return Ok(None);
            };
            let Ok(value) = static_shell_word(word, &options) else {
                return Ok(None);
            };
            argv.push(value);
        }
    }
    Ok(Some((root, argv)))
}

pub(crate) fn select_named_host_root(
    enabled: bool,
    source: &str,
    caveats: &Caveats,
) -> Result<Option<(String, Vec<String>)>, ShellInspectionError> {
    if !enabled {
        return Ok(None);
    }
    let Some((root, argv)) = lower_named_host_root_command(source)? else {
        return Ok(None);
    };
    if !matches!(&caveats.exec, Scope::Only(grants) if grants.contains(&root)) {
        return Ok(None);
    }
    Ok(Some((root, argv)))
}

#[cfg(test)]
#[path = "named_host_root_tests.rs"]
mod tests;

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
