//! Pure, non-executing, source-bound inspection of shell syntax accepted by Brush.

use brush_parser::ast::{
    AndOr, ArithmeticExpr, Command, CommandPrefixOrSuffixItem, CompoundCommand, CompoundList,
    IoFileRedirectKind, IoFileRedirectTarget, IoRedirect, SeparatorOperator, SimpleCommand,
    SourceLocation, Word,
};
use brush_parser::word::{
    self, Parameter, ParameterExpr, ParameterTransformOp, WordPiece, WordPieceWithSource,
};
use brush_parser::{Parser, ParserOptions};
use serde::Serialize;

const INSPECTION_SCHEMA_VERSION: u8 = 1;
pub(crate) const MAX_SOURCE_BYTES: usize = 32 * 1024;
const MAX_INSPECTION_DEPTH: usize = 32;

/// A stable, serializable, flattened inventory of a shell command's source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ShellInspection {
    /// Version of this stable projection.
    pub schema_version: u8,
    /// The exact shell source inspected.
    pub source: String,
    /// Simple commands in source order, without control-flow or pipeline edges.
    pub commands: Vec<InspectedCommand>,
    /// Dynamic shell constructs directly present in this source.
    pub constructs: Vec<InspectedConstruct>,
    /// Human-readable safety facts discovered during inspection.
    pub warnings: Vec<String>,
}

/// One simple shell command or pipeline stage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InspectedCommand {
    /// Source slice spanning this stage's words and redirect targets, excluding
    /// pipeline/list separators. The inspection root always retains the exact
    /// complete source; a redirect written before the first word may begin just
    /// outside this best-effort stage slice.
    pub source: String,
    /// Statically resolved executable/builtin name, when the node executes one.
    pub program: Option<String>,
    /// Shell words in argv position, before runtime expansion.
    pub argv: Vec<String>,
    /// Redirections attached to this stage.
    pub redirects: Vec<InspectedRedirect>,
    /// Commands delegated to a child program rather than spawned by Brush itself.
    pub descendant_execs: Vec<DescendantExec>,
}

/// One redirection attached to a command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InspectedRedirect {
    /// Explicit file descriptor, if one was written.
    pub fd: Option<i32>,
    /// The redirection operation.
    pub operation: RedirectOperation,
    /// The raw target word or descriptor, or inline here-document body.
    pub target: String,
}

/// A redirection operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedirectOperation {
    /// Read from a file.
    Read,
    /// Write to a file.
    Write,
    /// Append to a file.
    Append,
    /// Open a file for reading and writing.
    ReadWrite,
    /// Write while overriding noclobber.
    Clobber,
    /// Duplicate an input descriptor.
    DuplicateInput,
    /// Duplicate an output descriptor.
    DuplicateOutput,
    /// Supply a here-document.
    HereDocument,
    /// Supply a here-string.
    HereString,
    /// Redirect stdout and stderr together.
    OutputAndError,
    /// Append stdout and stderr together.
    AppendOutputAndError,
}

/// A command that an admitted external program can spawn outside Brush's interceptor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DescendantExec {
    /// Statically resolved child program name.
    pub program: String,
    /// Child argv in source form, including the program at index zero.
    pub argv: Vec<String>,
    /// Exact source for the delegating action.
    pub source: String,
    /// Why this is a distinct approval hazard.
    pub reason: String,
}

/// A shell expansion or substitution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InspectedConstruct {
    /// The kind of dynamic construct.
    pub kind: ShellConstructKind,
    /// Exact construct source, including delimiters.
    pub source: String,
    /// Construct body without its outer delimiters.
    pub body: String,
    /// Whether the construct occurs inside double quotes.
    pub quoted: bool,
    /// Recursive command inspection for command substitutions. State-free
    /// arithmetic expansions have no nested command inventory and use `None`.
    pub inspection: Option<Box<ShellInspection>>,
}

/// A dynamic construct recognized in a shell word.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellConstructKind {
    /// `$(...)`.
    CommandSubstitution,
    /// Legacy backquote substitution.
    BackquoteSubstitution,
    /// A state-free `$((...))` or `$[...]` expression. Arithmetic that reads or
    /// mutates shell state is rejected instead of emitting this projection.
    ArithmeticExpansion,
}

/// A fail-closed shell-inspection error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellInspectionError {
    message: String,
}

impl ShellInspectionError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Return the human-readable refusal reason.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ShellInspectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ShellInspectionError {}

/// Parse and inspect shell source without evaluating expansions or executing commands.
///
/// # Errors
///
/// Returns an error when the source is malformed, when the inspector cannot
/// statically identify an executable that could run, when arithmetic depends
/// on variables, arrays, assignments, or nested shell expansion, or when a
/// parameter expansion can reinterpret runtime state as shell syntax. Brush
/// builtins that evaluate deferred source (`trap` and `fc`) are refused, as is
/// every `printf -v` form: Brush reparses that option's assignment target, so
/// even an apparently simple target is outside this projection's safe subset.
pub fn inspect_shell(cmd: &str) -> Result<ShellInspection, ShellInspectionError> {
    inspect_shell_at_depth(cmd, 0)
}

fn inspect_shell_at_depth(
    source: &str,
    depth: usize,
) -> Result<ShellInspection, ShellInspectionError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(ShellInspectionError::new(format!(
            "shell inspection source exceeds the {MAX_SOURCE_BYTES}-byte limit"
        )));
    }
    if depth > MAX_INSPECTION_DEPTH {
        return Err(ShellInspectionError::new(format!(
            "shell inspection exceeded the maximum nesting depth of {MAX_INSPECTION_DEPTH}"
        )));
    }

    let options = ParserOptions::default();
    let mut parser = Parser::new(std::io::Cursor::new(source.as_bytes()), &options);
    let program = parser
        .parse_program()
        .map_err(|error| ShellInspectionError::new(format!("malformed shell syntax: {error}")))?;
    let mut nodes = Vec::new();
    let mut control_warnings = Vec::new();
    collect_inspection_nodes(&program, &mut nodes, &mut control_warnings)?;

    let mut constructs = Vec::new();
    let mut warnings = control_warnings;
    let mut commands = Vec::new();
    for node in nodes {
        match node {
            InspectNode::ControlWord(word) => {
                inspect_word_constructs(word, &options, depth, &mut constructs, &mut warnings)?;
            }
            InspectNode::Simple(simple) => {
                let stage_source = simple_command_source(simple, source)?;
                commands.push(inspect_simple_command(
                    simple,
                    stage_source,
                    source,
                    &options,
                    depth,
                    &mut constructs,
                    &mut warnings,
                )?);
            }
        }
    }

    Ok(ShellInspection {
        schema_version: INSPECTION_SCHEMA_VERSION,
        source: source.to_string(),
        commands,
        constructs,
        warnings,
    })
}

enum InspectNode<'a> {
    ControlWord(&'a Word),
    Simple(&'a SimpleCommand),
}

fn collect_inspection_nodes<'a>(
    program: &'a brush_parser::ast::Program,
    nodes: &mut Vec<InspectNode<'a>>,
    warnings: &mut Vec<String>,
) -> Result<(), ShellInspectionError> {
    for complete_command in &program.complete_commands {
        collect_compound_list(complete_command, nodes, warnings)?;
    }
    Ok(())
}

fn collect_compound_list<'a>(
    list: &'a CompoundList,
    nodes: &mut Vec<InspectNode<'a>>,
    warnings: &mut Vec<String>,
) -> Result<(), ShellInspectionError> {
    for item in &list.0 {
        if !item.0.additional.is_empty() {
            push_warning_once(
                warnings,
                "`&&`/`||` conditional relationship is retained in exact `source` but is not \
                 encoded in the flattened command inventory",
            );
        }
        collect_pipeline_nodes(&item.0.first, nodes, warnings)?;
        for additional in &item.0.additional {
            match additional {
                AndOr::And(pipeline) | AndOr::Or(pipeline) => {
                    collect_pipeline_nodes(pipeline, nodes, warnings)?;
                }
            }
        }
        if matches!(item.1, SeparatorOperator::Async) {
            push_warning_once(
                warnings,
                "`&` background lifecycle is retained in exact `source` but is not encoded in \
                 the flattened command inventory",
            );
        }
    }
    Ok(())
}

fn collect_pipeline_nodes<'a>(
    pipeline: &'a brush_parser::ast::Pipeline,
    nodes: &mut Vec<InspectNode<'a>>,
    warnings: &mut Vec<String>,
) -> Result<(), ShellInspectionError> {
    if pipeline.seq.len() > 1 {
        push_warning_once(
            warnings,
            "`|` pipeline topology is retained in exact `source` but is not encoded as edges in \
             the flattened command inventory",
        );
    }
    if pipeline.bang {
        push_warning_once(
            warnings,
            "`!` status negation is retained in exact `source` but is not encoded in the \
             flattened command inventory",
        );
    }
    if pipeline.timed.is_some() {
        push_warning_once(
            warnings,
            "`time` pipeline modifier is retained in exact `source` but is not encoded in the \
             flattened command inventory",
        );
    }
    for command in &pipeline.seq {
        match command {
            Command::Simple(simple) => nodes.push(InspectNode::Simple(simple)),
            Command::Compound(CompoundCommand::ForClause(for_clause), redirects) => {
                if redirects
                    .as_ref()
                    .is_some_and(|redirects| !redirects.0.is_empty())
                {
                    return Err(ShellInspectionError::new(
                        "redirections on a for loop are not yet represented by the stable inspection projection",
                    ));
                }
                if let Some(values) = &for_clause.values {
                    nodes.extend(values.iter().map(InspectNode::ControlWord));
                }
                warnings.push(format!(
                    "for-loop over `{}` is represented as a flattened control-flow projection; \
                     its value expansions are inspected before its body commands",
                    for_clause.variable_name
                ));
                collect_compound_list(&for_clause.body.list, nodes, warnings)?;
            }
            Command::Compound(CompoundCommand::ExtendedTest(_), _) => {
                return Err(ShellInspectionError::new(
                    "extended test commands are not yet represented by the stable inspection projection",
                ));
            }
            Command::Compound(_, _) => {
                return Err(ShellInspectionError::new(
                    "compound shell commands are not yet represented by the stable inspection projection",
                ));
            }
            Command::Function(_) => {
                return Err(ShellInspectionError::new(
                    "shell function definitions are not yet represented by the stable inspection projection",
                ));
            }
        }
    }
    Ok(())
}

fn push_warning_once(warnings: &mut Vec<String>, warning: &str) {
    if !warnings.iter().any(|existing| existing == warning) {
        warnings.push(warning.to_string());
    }
}

fn simple_command_source(
    simple: &SimpleCommand,
    source: &str,
) -> Result<String, ShellInspectionError> {
    let mut start = usize::MAX;
    let mut end = 0_usize;
    let mut include = |word: &Word| {
        if let Some(span) = word.location() {
            start = start.min(span.start.index);
            end = end.max(span.end.index);
        }
    };
    if let Some(prefix) = &simple.prefix {
        for item in &prefix.0 {
            include_item_words(item, &mut include);
        }
    }
    if let Some(word) = &simple.word_or_name {
        include(word);
    }
    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            include_item_words(item, &mut include);
        }
    }
    if start == usize::MAX || end < start {
        return Err(ShellInspectionError::new(
            "simple command has no complete source location",
        ));
    }
    Ok(slice_char_range(source, start, end)?.to_string())
}

fn include_item_words(item: &CommandPrefixOrSuffixItem, include: &mut impl FnMut(&Word)) {
    match item {
        CommandPrefixOrSuffixItem::Word(word)
        | CommandPrefixOrSuffixItem::AssignmentWord(_, word) => include(word),
        CommandPrefixOrSuffixItem::IoRedirect(redirect) => match redirect {
            IoRedirect::File(_, _, target) => match target {
                IoFileRedirectTarget::Filename(word) | IoFileRedirectTarget::Duplicate(word) => {
                    include(word)
                }
                IoFileRedirectTarget::Fd(_) | IoFileRedirectTarget::ProcessSubstitution(_, _) => {}
            },
            IoRedirect::HereString(_, word) | IoRedirect::OutputAndError(word, _) => include(word),
            IoRedirect::HereDocument(_, _) => {}
        },
        CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {}
    }
}

fn slice_char_range(source: &str, start: usize, end: usize) -> Result<&str, ShellInspectionError> {
    if start > end {
        return Err(ShellInspectionError::new(
            "parser returned an inverted source span",
        ));
    }
    let byte_start = char_index_to_byte(source, start).ok_or_else(|| {
        ShellInspectionError::new("parser returned a source-span start outside the command")
    })?;
    let byte_end = char_index_to_byte(source, end).ok_or_else(|| {
        ShellInspectionError::new("parser returned a source-span end outside the command")
    })?;
    source.get(byte_start..byte_end).ok_or_else(|| {
        ShellInspectionError::new("parser returned a source span that is not a UTF-8 boundary")
    })
}

fn char_index_to_byte(source: &str, index: usize) -> Option<usize> {
    if index == source.chars().count() {
        return Some(source.len());
    }
    source.char_indices().nth(index).map(|(byte, _)| byte)
}

#[allow(clippy::too_many_arguments)]
fn inspect_simple_command(
    simple: &SimpleCommand,
    stage_source: String,
    containing_source: &str,
    options: &ParserOptions,
    depth: usize,
    constructs: &mut Vec<InspectedConstruct>,
    warnings: &mut Vec<String>,
) -> Result<InspectedCommand, ShellInspectionError> {
    let mut argv = Vec::new();
    let mut argv_words = Vec::new();
    let mut redirects = Vec::new();

    if let Some(prefix) = &simple.prefix {
        for item in &prefix.0 {
            inspect_non_argv_item(item, options, depth, constructs, warnings, &mut redirects)?;
        }
    }

    let resolved_program = if let Some(command_word) = &simple.word_or_name {
        inspect_word_constructs(command_word, options, depth, constructs, warnings)?;
        let resolved = static_shell_word(command_word, options).map_err(|reason| {
            ShellInspectionError::new(format!(
                "dynamic executable name `{}` cannot be approved statically: {reason}",
                command_word.value
            ))
        })?;
        argv.push(command_word.value.clone());
        argv_words.push(command_word);
        Some(resolved)
    } else {
        None
    };
    if let Some((program, reason)) = resolved_program
        .as_deref()
        .filter(|program| !is_timeout_program(program))
        .and_then(|program| unprojectable_program_reason(program).map(|reason| (program, reason)))
    {
        return Err(ShellInspectionError::new(format!(
            "`{program}` cannot be projected statically: {reason}; submit the delegated command \
             directly when possible"
        )));
    }
    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            match item {
                CommandPrefixOrSuffixItem::Word(word)
                | CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                    inspect_word_constructs(word, options, depth, constructs, warnings)?;
                    argv.push(word.value.clone());
                    argv_words.push(word);
                }
                CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                    redirects.push(inspect_redirect(
                        redirect, options, depth, constructs, warnings,
                    )?);
                }
                CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {
                    return Err(ShellInspectionError::new(
                        "process substitution is not yet represented by the stable inspection projection",
                    ));
                }
            }
        }
    }

    if resolved_program.as_deref() == Some("printf") {
        ensure_brush_printf_is_projectable(&argv_words, options)?;
    }

    let descendant_execs = if resolved_program.as_deref().is_some_and(is_find_program) {
        inspect_find_descendants(&argv_words, containing_source, options, warnings)?
    } else if resolved_program.as_deref().is_some_and(is_timeout_program) {
        inspect_timeout_descendants(&argv_words, containing_source, options, warnings)?
    } else {
        Vec::new()
    };

    Ok(InspectedCommand {
        source: stage_source,
        program: resolved_program,
        argv,
        redirects,
        descendant_execs,
    })
}

fn inspect_non_argv_item(
    item: &CommandPrefixOrSuffixItem,
    options: &ParserOptions,
    depth: usize,
    constructs: &mut Vec<InspectedConstruct>,
    warnings: &mut Vec<String>,
    redirects: &mut Vec<InspectedRedirect>,
) -> Result<(), ShellInspectionError> {
    match item {
        CommandPrefixOrSuffixItem::Word(word)
        | CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
            inspect_word_constructs(word, options, depth, constructs, warnings)
        }
        CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
            redirects.push(inspect_redirect(
                redirect, options, depth, constructs, warnings,
            )?);
            Ok(())
        }
        CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => Err(ShellInspectionError::new(
            "process substitution is not yet represented by the stable inspection projection",
        )),
    }
}

fn inspect_redirect(
    redirect: &IoRedirect,
    options: &ParserOptions,
    depth: usize,
    constructs: &mut Vec<InspectedConstruct>,
    warnings: &mut Vec<String>,
) -> Result<InspectedRedirect, ShellInspectionError> {
    match redirect {
        IoRedirect::File(fd, kind, target) => {
            let operation = match kind {
                IoFileRedirectKind::Read => RedirectOperation::Read,
                IoFileRedirectKind::Write => RedirectOperation::Write,
                IoFileRedirectKind::Append => RedirectOperation::Append,
                IoFileRedirectKind::ReadAndWrite => RedirectOperation::ReadWrite,
                IoFileRedirectKind::Clobber => RedirectOperation::Clobber,
                IoFileRedirectKind::DuplicateInput => RedirectOperation::DuplicateInput,
                IoFileRedirectKind::DuplicateOutput => RedirectOperation::DuplicateOutput,
            };
            let target = match target {
                IoFileRedirectTarget::Filename(word) | IoFileRedirectTarget::Duplicate(word) => {
                    inspect_word_constructs(word, options, depth, constructs, warnings)?;
                    word.value.clone()
                }
                IoFileRedirectTarget::Fd(descriptor) => descriptor.to_string(),
                IoFileRedirectTarget::ProcessSubstitution(_, _) => {
                    return Err(ShellInspectionError::new(
                        "process substitution in a redirection is not yet represented by the stable inspection projection",
                    ));
                }
            };
            Ok(InspectedRedirect {
                fd: *fd,
                operation,
                target,
            })
        }
        IoRedirect::HereDocument(fd, document) => {
            if document.requires_expansion {
                // Heredoc quotes are literal characters, unlike argv words.
                // Reuse the same parser as Brush's basic heredoc expansion;
                // quoted delimiters leave the entire body opaque.
                let pieces =
                    word::parse_heredoc(&document.doc.value, options).map_err(|error| {
                        ShellInspectionError::new(format!(
                            "cannot inspect expanding here-document: {error}"
                        ))
                    })?;
                inspect_word_pieces(
                    &document.doc.value,
                    &pieces,
                    true,
                    depth,
                    constructs,
                    warnings,
                )?;
            }
            Ok(InspectedRedirect {
                fd: *fd,
                operation: RedirectOperation::HereDocument,
                target: document.doc.value.clone(),
            })
        }
        IoRedirect::HereString(fd, word) => {
            inspect_word_constructs(word, options, depth, constructs, warnings)?;
            Ok(InspectedRedirect {
                fd: *fd,
                operation: RedirectOperation::HereString,
                target: word.value.clone(),
            })
        }
        IoRedirect::OutputAndError(word, append) => {
            inspect_word_constructs(word, options, depth, constructs, warnings)?;
            Ok(InspectedRedirect {
                fd: None,
                operation: if *append {
                    RedirectOperation::AppendOutputAndError
                } else {
                    RedirectOperation::OutputAndError
                },
                target: word.value.clone(),
            })
        }
    }
}

fn inspect_word_constructs(
    word: &Word,
    options: &ParserOptions,
    depth: usize,
    constructs: &mut Vec<InspectedConstruct>,
    warnings: &mut Vec<String>,
) -> Result<(), ShellInspectionError> {
    let pieces = word::parse(&word.value, options).map_err(|error| {
        ShellInspectionError::new(format!(
            "cannot inspect shell word `{}`: {error}",
            word.value
        ))
    })?;
    inspect_word_pieces(&word.value, &pieces, false, depth, constructs, warnings)
}

fn inspect_word_pieces(
    raw_word: &str,
    pieces: &[WordPieceWithSource],
    quoted: bool,
    depth: usize,
    constructs: &mut Vec<InspectedConstruct>,
    warnings: &mut Vec<String>,
) -> Result<(), ShellInspectionError> {
    for piece_with_source in pieces {
        let source = raw_word
            .get(piece_with_source.start_index..piece_with_source.end_index)
            .ok_or_else(|| {
                ShellInspectionError::new(
                    "word parser returned a construct span that is not a UTF-8 boundary",
                )
            })?;
        match &piece_with_source.piece {
            WordPiece::DoubleQuotedSequence(nested)
            | WordPiece::GettextDoubleQuotedSequence(nested) => {
                inspect_word_pieces(raw_word, nested, true, depth, constructs, warnings)?;
            }
            WordPiece::CommandSubstitution(body) => {
                let inspection = inspect_shell_at_depth(body, depth + 1)?;
                constructs.push(InspectedConstruct {
                    kind: ShellConstructKind::CommandSubstitution,
                    source: source.to_string(),
                    body: body.clone(),
                    quoted,
                    inspection: Some(Box::new(inspection)),
                });
                warnings.push(substitution_value_warning(source, quoted));
            }
            WordPiece::BackquotedCommandSubstitution(body) => {
                let inspection = inspect_shell_at_depth(body, depth + 1)?;
                constructs.push(InspectedConstruct {
                    kind: ShellConstructKind::BackquoteSubstitution,
                    source: source.to_string(),
                    body: body.clone(),
                    quoted,
                    inspection: Some(Box::new(inspection)),
                });
                warnings.push(substitution_value_warning(source, quoted));
            }
            WordPiece::ArithmeticExpression(expression) => {
                if contains_nested_command_syntax(&expression.value) {
                    return Err(ShellInspectionError::new(
                        "command syntax nested inside arithmetic expansion cannot be represented safely",
                    ));
                }
                ensure_static_arithmetic_expression(&expression.value, "arithmetic expansion")?;
                constructs.push(InspectedConstruct {
                    kind: ShellConstructKind::ArithmeticExpansion,
                    source: source.to_string(),
                    body: expression.value.clone(),
                    quoted,
                    inspection: None,
                });
            }
            WordPiece::ParameterExpansion(expression) => {
                if contains_nested_command_syntax(source) {
                    return Err(ShellInspectionError::new(
                        "command syntax nested inside parameter expansion cannot be represented safely",
                    ));
                }
                ensure_parameter_expansion_is_non_executing(expression)?;
            }
            WordPiece::Text(_)
            | WordPiece::SingleQuotedText(_)
            | WordPiece::AnsiCQuotedText(_)
            | WordPiece::TildeExpansion(_)
            | WordPiece::EscapeSequence(_) => {}
        }
    }
    Ok(())
}

fn ensure_static_arithmetic_expression(
    expression: &str,
    context: &str,
) -> Result<(), ShellInspectionError> {
    let parsed = brush_parser::arithmetic::parse(expression).map_err(|_| {
        ShellInspectionError::new(format!(
            "{context} is not statically evaluable; only integer literals and operators without \
             shell expansions may be approved"
        ))
    })?;
    if is_state_free_arithmetic_expression(&parsed) {
        Ok(())
    } else {
        Err(ShellInspectionError::new(format!(
            "{context} depends on runtime shell state; variables, array references, and \
             assignments cannot be approved from source alone"
        )))
    }
}

fn is_state_free_arithmetic_expression(expression: &ArithmeticExpr) -> bool {
    match expression {
        ArithmeticExpr::Literal(_) => true,
        ArithmeticExpr::UnaryOp(_, operand) => is_state_free_arithmetic_expression(operand),
        ArithmeticExpr::BinaryOp(_, left, right) => {
            is_state_free_arithmetic_expression(left) && is_state_free_arithmetic_expression(right)
        }
        ArithmeticExpr::Conditional(condition, then_expression, else_expression) => {
            is_state_free_arithmetic_expression(condition)
                && is_state_free_arithmetic_expression(then_expression)
                && is_state_free_arithmetic_expression(else_expression)
        }
        ArithmeticExpr::Reference(_)
        | ArithmeticExpr::Assignment(_, _)
        | ArithmeticExpr::BinaryAssignment(_, _, _)
        | ArithmeticExpr::UnaryAssignment(_, _) => false,
    }
}

fn ensure_parameter_expansion_is_non_executing(
    expression: &ParameterExpr,
) -> Result<(), ShellInspectionError> {
    if let ParameterExpr::Transform {
        op: ParameterTransformOp::PromptExpand,
        ..
    } = expression
    {
        return Err(ShellInspectionError::new(
            "prompt-expanding parameter transformation `@P` can execute shell syntax obtained \
             from runtime state",
        ));
    }

    if let ParameterExpr::Substring { offset, length, .. } = expression {
        ensure_static_arithmetic_expression(&offset.value, "parameter substring offset")?;
        if let Some(length) = length {
            ensure_static_arithmetic_expression(&length.value, "parameter substring length")?;
        }
    }

    let Some((parameter, indirect)) = parameter_reference(expression) else {
        return Ok(());
    };
    if indirect {
        return Err(ShellInspectionError::new(
            "indirect parameter expansion can reinterpret runtime state as an array reference \
             and cannot be approved from source alone",
        ));
    }
    if let Parameter::NamedWithIndex { index, .. } = parameter {
        ensure_static_arithmetic_expression(index, "parameter array subscript")?;
    }
    Ok(())
}

#[allow(clippy::match_same_arms)]
fn parameter_reference(expression: &ParameterExpr) -> Option<(&Parameter, bool)> {
    match expression {
        ParameterExpr::Parameter {
            parameter,
            indirect,
        }
        | ParameterExpr::UseDefaultValues {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::AssignDefaultValues {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::IndicateErrorIfNullOrUnset {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::UseAlternativeValue {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::ParameterLength {
            parameter,
            indirect,
        }
        | ParameterExpr::RemoveSmallestSuffixPattern {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::RemoveLargestSuffixPattern {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::RemoveSmallestPrefixPattern {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::RemoveLargestPrefixPattern {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::Substring {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::Transform {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::UppercaseFirstChar {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::UppercasePattern {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::LowercaseFirstChar {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::LowercasePattern {
            parameter,
            indirect,
            ..
        }
        | ParameterExpr::ReplaceSubstring {
            parameter,
            indirect,
            ..
        } => Some((parameter, *indirect)),
        ParameterExpr::VariableNames { .. } | ParameterExpr::MemberKeys { .. } => None,
    }
}

fn contains_nested_command_syntax(source: &str) -> bool {
    source.contains("$(") || source.contains('`')
}

fn substitution_value_warning(source: &str, quoted: bool) -> String {
    if quoted {
        format!(
            "substitution `{source}` has no inspectable value until its inner command source runs; double quotes preserve the captured output as one shell field"
        )
    } else {
        format!(
            "substitution `{source}` has no inspectable value until its inner command source runs; unquoted output is subject to field splitting and pathname expansion"
        )
    }
}

pub(crate) fn static_shell_word(
    word: &Word,
    options: &ParserOptions,
) -> Result<String, ShellInspectionError> {
    if may_contain_brace_expansion(&word.value)
        && word::parse_brace_expansions(&word.value, options)
            .map_err(|error| {
                ShellInspectionError::new(format!(
                    "cannot analyze brace expansion in `{}`: {error}",
                    word.value
                ))
            })?
            .is_some()
    {
        return Err(ShellInspectionError::new(
            "brace expansion can produce multiple runtime words",
        ));
    }
    let pieces = word::parse(&word.value, options).map_err(|error| {
        ShellInspectionError::new(format!("cannot parse `{}`: {error}", word.value))
    })?;
    let mut value = String::new();
    append_static_pieces(&pieces, false, &mut value)?;
    Ok(value)
}

fn may_contain_brace_expansion(word: &str) -> bool {
    word.split_once('{').is_some_and(|(_, after_open)| {
        after_open
            .split_once('}')
            .is_some_and(|(inside, _)| inside.contains(',') || inside.contains(".."))
    })
}

fn append_static_pieces(
    pieces: &[WordPieceWithSource],
    quoted: bool,
    output: &mut String,
) -> Result<(), ShellInspectionError> {
    for piece_with_source in pieces {
        match &piece_with_source.piece {
            WordPiece::Text(text) => {
                if !quoted && has_pathname_expansion(text) {
                    return Err(ShellInspectionError::new(
                        "unquoted pathname expansion makes the runtime word dynamic",
                    ));
                }
                output.push_str(text);
            }
            WordPiece::SingleQuotedText(text) => output.push_str(text),
            WordPiece::DoubleQuotedSequence(nested) => {
                append_static_pieces(nested, true, output)?;
            }
            WordPiece::EscapeSequence(escaped) => {
                output.extend(escaped.chars().skip(1));
            }
            WordPiece::AnsiCQuotedText(_)
            | WordPiece::GettextDoubleQuotedSequence(_)
            | WordPiece::TildeExpansion(_)
            | WordPiece::ParameterExpansion(_)
            | WordPiece::CommandSubstitution(_)
            | WordPiece::BackquotedCommandSubstitution(_)
            | WordPiece::ArithmeticExpression(_) => {
                return Err(ShellInspectionError::new(
                    "runtime expansion determines the resulting word",
                ));
            }
        }
    }
    Ok(())
}

fn has_pathname_expansion(text: &str) -> bool {
    text.contains(['*', '?', '['])
}

/// Refuse Brush's assignment-producing `printf` mode. `assign_to_named_parameter`
/// reparses the `-v` target as shell assignment syntax, including array
/// subscripts. Rejecting every `-v` target is intentionally more conservative
/// than attempting to duplicate that runtime grammar here.
///
/// A dynamic first argument also fails closed: after expansion and field
/// splitting it could select `-v` (or disappear and expose a following `-v`).
fn ensure_brush_printf_is_projectable(
    argv_words: &[&Word],
    options: &ParserOptions,
) -> Result<(), ShellInspectionError> {
    let Some(first_argument) = argv_words.get(1) else {
        return Ok(());
    };
    let first_argument = static_shell_word(first_argument, options).map_err(|_| {
        ShellInspectionError::new(
            "Brush `printf` has a dynamic first argument that could select `-v`; assignment \
             targets that are reparsed from runtime shell state cannot be approved from source",
        )
    })?;

    if first_argument != "--" && first_argument.starts_with("-v") {
        return Err(ShellInspectionError::new(
            "Brush `printf -v` reparses its assignment target against runtime shell state and is \
             deliberately excluded from the stable inspection projection",
        ));
    }

    Ok(())
}

fn is_find_program(program: &str) -> bool {
    classification_basename(program) == "find"
}

fn is_timeout_program(program: &str) -> bool {
    classification_basename(program) == "timeout"
}

/// Project one literal timeout wrapper, retaining the original command. This
/// is an inventory, not a promise that Brush intercepts its native children.
fn inspect_timeout_descendants(
    words: &[&Word],
    source: &str,
    options: &ParserOptions,
    warnings: &mut Vec<String>,
) -> Result<Vec<DescendantExec>, ShellInspectionError> {
    let invalid = || {
        ShellInspectionError::new(
        "`timeout` requires recognized literal options, a nonnegative finite duration, and a static descendant executable",
    )
    };
    let literal = |index: usize| {
        words
            .get(index)
            .ok_or_else(invalid)
            .and_then(|word| static_shell_word(word, options).map_err(|_| invalid()))
    };
    let duration = |value: &str| {
        let number = value.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(value);
        number
            .parse::<f64>()
            .is_ok_and(|n| n.is_finite() && n >= 0.0)
    };
    let signal =
        |value: &str| !value.is_empty() && value.bytes().all(|c| c.is_ascii_alphanumeric());
    let mut index = 1;
    loop {
        let option = literal(index)?;
        match option.as_str() {
            "--" => {
                index += 1;
                break;
            }
            "--foreground" | "--preserve-status" | "--verbose" | "-f" | "-p" | "-v" => {
                index += 1;
            }
            "-s" | "--signal" | "-k" | "--kill-after" => {
                let value = literal(index + 1)?;
                let valid = if matches!(option.as_str(), "-s" | "--signal") {
                    signal(&value)
                } else {
                    duration(&value)
                };
                if !valid {
                    return Err(invalid());
                }
                index += 2;
            }
            _ if option.starts_with("--signal=") => {
                if !signal(&option["--signal=".len()..]) {
                    return Err(invalid());
                }
                index += 1;
            }
            _ if option.starts_with("--kill-after=") => {
                if !duration(&option["--kill-after=".len()..]) {
                    return Err(invalid());
                }
                index += 1;
            }
            _ if option.starts_with('-') => return Err(invalid()),
            _ => break,
        }
    }
    if !duration(&literal(index)?) {
        return Err(invalid());
    }
    let child_index = index + 1;
    let program = literal(child_index)?;
    if program.is_empty() {
        return Err(invalid());
    }
    // Do not hide a second dispatcher below the one represented here. Find's
    // existing explicit child inventory is folded below; other wrappers fail.
    if let Some(reason) = unprojectable_program_reason(&program) {
        return Err(ShellInspectionError::new(format!(
            "`timeout` descendant `{program}` cannot be projected statically: {reason}"
        )));
    }
    let reason = format!(
        "external `timeout` spawns `{program}` itself; Brush cannot independently intercept that descendant process"
    );
    warnings.push(reason.clone());
    let mut children = vec![DescendantExec {
        program: program.clone(),
        argv: words[child_index..]
            .iter()
            .map(|word| word.value.clone())
            .collect(),
        source: source_between_words(words[0], words[words.len() - 1], source)
            .ok_or_else(invalid)?,
        reason,
    }];
    if is_find_program(&program) {
        children.extend(inspect_find_descendants(
            &words[child_index..],
            source,
            options,
            warnings,
        )?);
    }
    Ok(children)
}

fn unprojectable_program_reason(program: &str) -> Option<&'static str> {
    let basename = classification_basename(program);

    if matches!(
        basename.as_str(),
        "eval"
            | "source"
            | "."
            | "trap"
            | "fc"
            | "builtin"
            | "command"
            | "env"
            | "xargs"
            | "exec"
            | "parallel"
            | "sudo"
            | "doas"
            | "nice"
            | "nohup"
            | "setsid"
            | "time"
            | "timeout"
            | "stdbuf"
            | "chroot"
            | "su"
            | "runuser"
            | "busybox"
            | "watch"
    ) {
        return Some(
            "this dispatcher can select, construct, or evaluate another command that is absent \
             from the flattened inventory",
        );
    }

    if is_shell_or_interpreter(&basename) {
        return Some(
            "this shell or interpreter can load or evaluate runtime code whose commands are \
             absent from the flattened inventory",
        );
    }

    None
}

/// Produce a classification-only basename for Windows and Unix spellings.
///
/// Windows executable lookup is case-insensitive and recognizes the standard
/// PATHEXT executable suffixes. This normalized copy is never written into the
/// public projection; `program`, `argv`, and exact source retain their original
/// spelling.
fn classification_basename(program: &str) -> String {
    let basename = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let lowercase = basename.to_ascii_lowercase();
    for suffix in [".exe", ".com", ".bat", ".cmd"] {
        if let Some(stem) = lowercase.strip_suffix(suffix) {
            if !stem.is_empty() {
                return stem.to_string();
            }
        }
    }
    lowercase
}

fn is_shell_or_interpreter(basename: &str) -> bool {
    matches!(
        basename,
        "sh" | "ash"
            | "bash"
            | "dash"
            | "zsh"
            | "ksh"
            | "ksh93"
            | "mksh"
            | "fish"
            | "csh"
            | "tcsh"
            | "nu"
            | "elvish"
            | "xonsh"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "python"
            | "python2"
            | "python3"
            | "pypy"
            | "pypy3"
            | "perl"
            | "ruby"
            | "php"
            | "lua"
            | "luajit"
            | "node"
            | "nodejs"
            | "deno"
            | "bun"
            | "tclsh"
            | "wish"
            | "awk"
            | "gawk"
            | "mawk"
            | "nawk"
            | "r"
            | "rscript"
            | "julia"
            | "osascript"
    ) || ["python", "pypy", "perl", "ruby", "php", "lua", "node"]
        .iter()
        .any(|stem| has_numeric_version_suffix(basename, stem))
}

fn has_numeric_version_suffix(basename: &str, stem: &str) -> bool {
    basename.strip_prefix(stem).is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix.chars().any(|character| character.is_ascii_digit())
            && suffix
                .chars()
                .all(|character| character.is_ascii_digit() || matches!(character, '.' | '-'))
    })
}

fn inspect_find_descendants(
    argv_words: &[&Word],
    containing_source: &str,
    options: &ParserOptions,
    warnings: &mut Vec<String>,
) -> Result<Vec<DescendantExec>, ShellInspectionError> {
    let argv = argv_words
        .iter()
        .map(|word| {
            static_shell_word(word, options).map_err(|reason| {
                ShellInspectionError::new(format!(
                    "dynamic `find` argument `{}` prevents proving whether it delegates child execution: {reason}",
                    word.value
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut descendants = Vec::new();
    let mut index = 1_usize;

    while index < argv.len() {
        let action = argv[index].as_str();
        if !matches!(action, "-exec" | "-execdir" | "-ok" | "-okdir") {
            index += 1;
            continue;
        }
        let program_index = index + 1;
        if program_index >= argv.len() {
            return Err(ShellInspectionError::new(format!(
                "`find {action}` is missing its descendant executable"
            )));
        }
        let terminator_index = (program_index + 1..argv.len())
            .find(|candidate| {
                argv[*candidate] == ";"
                    || (argv[*candidate] == "+"
                        && *candidate > program_index
                        && argv[*candidate - 1] == "{}")
            })
            .ok_or_else(|| {
                ShellInspectionError::new(format!(
                    "`find {action}` is missing its `;` or `+` terminator"
                ))
            })?;
        let descendant_argv = argv[program_index..terminator_index].to_vec();
        let program = descendant_argv.first().cloned().ok_or_else(|| {
            ShellInspectionError::new(format!(
                "`find {action}` is missing its descendant executable"
            ))
        })?;
        if program.is_empty() {
            return Err(ShellInspectionError::new(format!(
                "`find {action}` has an empty descendant executable"
            )));
        }
        if is_timeout_program(&program) {
            return Err(ShellInspectionError::new(
                "`find` delegating to `timeout` requires a nested native dispatcher projection; only a direct timeout wrapper is represented",
            ));
        }
        let source = source_between_words(
            argv_words[index],
            argv_words[terminator_index],
            containing_source,
        )
        .unwrap_or_else(|| {
            argv_words[index..=terminator_index]
                .iter()
                .map(|word| word.value.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        });
        let reason = format!(
            "external `find` interprets `{action}` and spawns `{program}` itself; Brush cannot independently intercept that descendant process"
        );
        warnings.push(format!("find {action}: {reason}"));
        descendants.push(DescendantExec {
            program,
            argv: descendant_argv,
            source,
            reason,
        });
        index = terminator_index + 1;
    }

    Ok(descendants)
}

fn source_between_words(first: &Word, last: &Word, source: &str) -> Option<String> {
    let first = first.location()?;
    let last = last.location()?;
    slice_char_range(source, first.start.index, last.end.index)
        .ok()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPOUND_EXAMPLE: &str = r#"ls -1 $(find . -name "*.rs" -type f -exec wc -l {} + 2>/dev/null | sort -nr | head -10)"#;

    #[test]
    fn timeout_projects_a_literal_descendant_without_rewriting_source() {
        for prefix in [
            "timeout 120",
            "'timeout' --foreground --preserve-status --verbose -s TERM -k 2s 0.5m",
            "/usr/bin/timeout --signal=9 --kill-after=1s -- 3h",
        ] {
            let source = format!("printf before; {prefix} 'cargo' check --offline 2>&1 | tail -30");
            let inspected = inspect_shell(&source).unwrap();
            assert_eq!(inspected.source, source);
            let wrapper = &inspected.commands[1];
            assert_eq!(wrapper.descendant_execs.len(), 1);
            let child = &wrapper.descendant_execs[0];
            assert_eq!(child.program, "cargo");
            assert_eq!(child.argv, ["'cargo'", "check", "--offline"]);
            assert!(child.source.contains(prefix));
            assert!(child.source.ends_with("'cargo' check --offline"));
            assert!(child.reason.contains("cannot independently intercept"));
        }
    }

    #[test]
    fn timeout_keeps_unproven_dispatch_out_of_the_inventory() {
        for source in [
            "timeout --unknown 2 cargo check",
            "timeout --help 2 cargo check",
            "timeout --version 2 cargo check",
            "timeout -s \"$SIGNAL\" 2 cargo check",
            "timeout $SECONDS cargo check",
            "timeout 2 \"$PROGRAM\" check",
            "timeout 2 sh -c 'cargo check'",
            "timeout 2 timeout 3 cargo check",
            "timeout NaN cargo check",
            "timeout -1 cargo check",
            "timeout 2",
        ] {
            assert!(inspect_shell(source).is_err(), "{source}");
        }
    }

    #[test]
    fn timeout_retains_find_delegation_and_nested_argument_constructs() {
        let source = "timeout 2 find . -exec git commit -m message \\;";
        let inspected = inspect_shell(source).unwrap();
        let children = &inspected.commands[0].descendant_execs;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].program, "find");
        assert_eq!(children[1].program, "git");
        let inspected =
            inspect_shell("timeout 2 cargo check --package \"$(printf package)\"").unwrap();
        assert_eq!(inspected.constructs.len(), 1);
        assert_eq!(inspected.commands[0].descendant_execs[0].program, "cargo");
    }

    #[test]
    fn find_cannot_hide_an_unprojected_timeout_descendant() {
        for source in [
            "find . -exec timeout 2 git commit -m delegated \\;",
            "timeout 3 find . -exec timeout 2 git commit -m delegated \\;",
            "git commit -m direct; find . -exec timeout 2 git commit -m delegated \\;",
        ] {
            assert!(inspect_shell(source).is_err(), "{source}");
        }
    }

    #[test]
    fn heredoc_quoted_body_is_inline_data_without_executable_words() {
        let body = "literal $(touch never) `touch neither` 'unmatched\n";
        let source = format!("cat <<'EOF'\n{body}EOF\n");
        let inspected = inspect_shell(&source).expect("quoted heredoc inspection");
        assert_eq!(inspected.commands.len(), 1);
        assert_eq!(inspected.commands[0].program.as_deref(), Some("cat"));
        assert_eq!(
            inspected.commands[0].redirects,
            vec![InspectedRedirect {
                fd: None,
                operation: RedirectOperation::HereDocument,
                target: body.to_owned(),
            }]
        );
        assert!(inspected.constructs.is_empty());
    }

    #[test]
    fn heredoc_unquoted_body_inspects_expansion_with_literal_quotes() {
        let source = "cat <<EOF\n'$(printf nested)' `printf backquoted`\nEOF\n";
        let inspected = inspect_shell(source).expect("expanding heredoc inspection");
        assert_eq!(inspected.constructs.len(), 2);
        assert_eq!(
            inspected.constructs[0].kind,
            ShellConstructKind::CommandSubstitution
        );
        assert_eq!(
            inspected.constructs[1].kind,
            ShellConstructKind::BackquoteSubstitution
        );
        for construct in &inspected.constructs {
            assert_eq!(
                construct.inspection.as_ref().unwrap().commands[0]
                    .program
                    .as_deref(),
                Some("printf")
            );
        }
        assert_eq!(
            inspected.commands[0].redirects[0].operation,
            RedirectOperation::HereDocument
        );
    }

    #[test]
    fn heredoc_nested_in_commit_preserves_complete_inspection() {
        let source = "git branch --show-current && git commit -m \"$(cat <<'EOF'\nsubject\n\nbody\nEOF\n)\" 2>&1 | tail -5";
        let inspected = inspect_shell(source).expect("compound heredoc inspection");
        assert_eq!(
            inspected
                .commands
                .iter()
                .map(|command| command.program.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("git"), Some("git"), Some("tail")]
        );
        let nested = inspected.constructs[0].inspection.as_ref().unwrap();
        assert_eq!(nested.commands[0].program.as_deref(), Some("cat"));
        assert_eq!(nested.commands[0].redirects[0].target, "subject\n\nbody\n");

        for source in [
            "cat <<EOF\n$(if)\nEOF\n",
            "cat <<EOF\n$(printf -v target value)\nEOF\n",
            "cat <<EOF\n${value@P}\nEOF\n",
        ] {
            assert!(
                inspect_shell(source).is_err(),
                "must retain nested refusal: {source}"
            );
        }
    }

    #[test]
    fn inspects_compound_substitution_pipeline_and_find_descendant() {
        let inspected = inspect_shell(COMPOUND_EXAMPLE).expect("inspection");

        assert_eq!(inspected.source, COMPOUND_EXAMPLE);
        assert_eq!(inspected.schema_version, INSPECTION_SCHEMA_VERSION);
        assert_eq!(inspected.commands.len(), 1);
        assert_eq!(inspected.commands[0].source, COMPOUND_EXAMPLE);
        assert_eq!(inspected.commands[0].program.as_deref(), Some("ls"));
        assert_eq!(
            inspected.commands[0].argv,
            vec![
                "ls",
                "-1",
                r#"$(find . -name "*.rs" -type f -exec wc -l {} + 2>/dev/null | sort -nr | head -10)"#
            ]
        );

        let construct = &inspected.constructs[0];
        assert_eq!(construct.kind, ShellConstructKind::CommandSubstitution);
        assert!(!construct.quoted);
        assert_eq!(
            construct.source,
            r#"$(find . -name "*.rs" -type f -exec wc -l {} + 2>/dev/null | sort -nr | head -10)"#
        );
        let inner = construct.inspection.as_deref().expect("inner inspection");
        assert_eq!(inner.commands.len(), 3);
        assert_eq!(
            inner
                .commands
                .iter()
                .map(|command| command.source.as_str())
                .collect::<Vec<_>>(),
            vec![
                r#"find . -name "*.rs" -type f -exec wc -l {} + 2>/dev/null"#,
                "sort -nr",
                "head -10"
            ]
        );
        assert_eq!(
            inner.commands[0].redirects,
            vec![InspectedRedirect {
                fd: Some(2),
                operation: RedirectOperation::Write,
                target: "/dev/null".to_string(),
            }]
        );
        assert_eq!(inner.commands[0].descendant_execs.len(), 1);
        assert_eq!(inner.commands[0].descendant_execs[0].program, "wc");
        assert_eq!(
            inner.commands[0].descendant_execs[0].argv,
            vec!["wc", "-l", "{}"]
        );
        assert_eq!(
            inner.commands[0].descendant_execs[0].source,
            "-exec wc -l {} +"
        );
        assert!(inner.warnings.iter().any(|warning| {
            warning.contains("find -exec") && warning.contains("cannot independently intercept")
        }));
    }

    #[test]
    fn preserves_nested_quoted_and_multiple_substitutions() {
        let source = r#"echo "$(printf '%s' "$(uname)")" $(pwd)"#;
        let inspected = inspect_shell(source).expect("inspection");

        assert_eq!(inspected.constructs.len(), 2);
        assert!(inspected.constructs[0].quoted);
        assert!(!inspected.constructs[1].quoted);
        let first_inner = inspected.constructs[0]
            .inspection
            .as_deref()
            .expect("first inner");
        assert_eq!(first_inner.constructs.len(), 1);
        assert!(first_inner.constructs[0].quoted);
        assert_eq!(first_inner.constructs[0].source, "$(uname)");
    }

    #[test]
    fn inspects_for_loop_values_before_flattened_body_commands() {
        let source = r#"for f in $(find . -name '*.rs' -type f); do wc -l "$f"; done"#;
        let inspected = inspect_shell(source).expect("inspection");

        assert_eq!(inspected.constructs.len(), 1);
        assert_eq!(
            inspected.constructs[0].kind,
            ShellConstructKind::CommandSubstitution
        );
        let inner = inspected.constructs[0]
            .inspection
            .as_deref()
            .expect("find substitution");
        assert_eq!(inner.commands[0].program.as_deref(), Some("find"));
        assert_eq!(inspected.commands.len(), 1);
        assert_eq!(inspected.commands[0].program.as_deref(), Some("wc"));
        assert_eq!(inspected.commands[0].source, r#"wc -l "$f""#);
        assert!(inspected
            .warnings
            .iter()
            .any(|warning| warning.contains("flattened control-flow projection")));
    }

    #[test]
    fn inspects_for_loop_piped_into_sort_and_head() {
        let source = r#"for f in $(find . -name '*.rs' -type f -not -path '*/target/*' -not -path '*/.git/*'); do wc -l "$f"; done | sort -nr | head -10"#;
        let inspected = inspect_shell(source).expect("inspection");

        assert_eq!(inspected.source, source);
        assert_eq!(inspected.constructs.len(), 1);
        assert_eq!(
            inspected.constructs[0].kind,
            ShellConstructKind::CommandSubstitution
        );
        assert!(!inspected.constructs[0].quoted);
        assert_eq!(
            inspected.constructs[0].body,
            r#"find . -name '*.rs' -type f -not -path '*/target/*' -not -path '*/.git/*'"#
        );
        let find = inspected.constructs[0]
            .inspection
            .as_deref()
            .expect("find substitution");
        assert_eq!(find.commands.len(), 1);
        assert_eq!(find.commands[0].program.as_deref(), Some("find"));
        assert_eq!(
            find.commands[0].argv,
            vec![
                "find",
                ".",
                "-name",
                "'*.rs'",
                "-type",
                "f",
                "-not",
                "-path",
                "'*/target/*'",
                "-not",
                "-path",
                "'*/.git/*'",
            ]
        );

        assert_eq!(
            inspected
                .commands
                .iter()
                .map(|command| command.program.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("wc"), Some("sort"), Some("head")]
        );
        assert_eq!(
            inspected
                .commands
                .iter()
                .map(|command| command.source.as_str())
                .collect::<Vec<_>>(),
            vec![r#"wc -l "$f""#, "sort -nr", "head -10"]
        );
        assert!(inspected
            .warnings
            .iter()
            .any(|warning| warning.contains("flattened control-flow projection")));
    }

    #[test]
    fn reports_backquotes_and_arithmetic_without_evaluating_them() {
        let inspected = inspect_shell(r#"echo "$((1 + 2))" `pwd`"#).expect("inspection");
        assert_eq!(
            inspected
                .constructs
                .iter()
                .map(|construct| (construct.kind, construct.quoted))
                .collect::<Vec<_>>(),
            vec![
                (ShellConstructKind::ArithmeticExpansion, true),
                (ShellConstructKind::BackquoteSubstitution, false),
            ]
        );
        assert!(inspected.constructs[0].inspection.is_none());
        assert!(inspected.constructs[1].inspection.is_some());
    }

    #[test]
    fn accepts_only_state_free_arithmetic_for_complete_preflight() {
        let source = r#"echo "$((1 + 2 * (3 - 4)))" "$((1 ? 2 : 3))" "$((16#ff & 7))" "$[4 + 5]""#;
        let inspected = inspect_shell(source).expect("static arithmetic inspection");

        assert_eq!(inspected.constructs.len(), 4);
        assert!(inspected.constructs.iter().all(|construct| {
            construct.kind == ShellConstructKind::ArithmeticExpansion
                && construct.inspection.is_none()
        }));
        assert_eq!(inspected.constructs[0].body, "1 + 2 * (3 - 4)");
        assert_eq!(inspected.constructs[1].body, "1 ? 2 : 3");
        assert_eq!(inspected.constructs[2].body, "16#ff & 7");
        assert_eq!(inspected.constructs[3].body, "4 + 5");
    }

    #[test]
    fn non_static_arithmetic_fails_closed_before_brush_dispatch() {
        for source in [
            "echo $((counter))",
            "echo $[counter]",
            "echo $((array[index]))",
            "echo $((counter += 1))",
            "echo $((++counter))",
            "counter=2; echo $((counter + 1))",
            r#"payload='array[$(printf SHOULD_NOT_RUN >&2)0]'; echo "$((payload))""#,
            r#"index='$(printf SHOULD_NOT_RUN >&2)'; echo "$((array[index]))""#,
        ] {
            let error =
                inspect_shell(source).expect_err("runtime-state arithmetic must fail closed");
            assert!(
                error.message().contains("runtime shell state"),
                "unexpected refusal for `{source}`: {error}"
            );
        }

        for source in [
            "echo $(( $counter + 1 ))",
            "echo $(( ${counter} + 1 ))",
            "echo $(( $(printf 1) + 1 ))",
            "echo $(( `printf 1` + 1 ))",
            "echo $((1 + $((2))))",
        ] {
            assert!(
                inspect_shell(source).is_err(),
                "nested expansion must fail closed: {source}"
            );
        }
    }

    #[test]
    fn parameter_arithmetic_and_runtime_reexpansion_fail_closed() {
        inspect_shell(r#"echo "${array[1 + 2]}" "${value:1 + 2:3}""#)
            .expect("state-free parameter arithmetic");

        for (source, expected_reason) in [
            (
                r#"echo "${array[index]}""#,
                "parameter array subscript depends on runtime shell state",
            ),
            (
                r#"echo "${value:offset:2}""#,
                "parameter substring offset depends on runtime shell state",
            ),
            (
                r#"echo "${value:1:length}""#,
                "parameter substring length depends on runtime shell state",
            ),
            (
                r#"payload='$(printf SHOULD_NOT_RUN >&2)'; echo "${payload@P}""#,
                "prompt-expanding parameter transformation",
            ),
            (
                r#"ref='array[$(printf SHOULD_NOT_RUN >&2)0]'; echo "${!ref}""#,
                "indirect parameter expansion",
            ),
        ] {
            let error = inspect_shell(source)
                .expect_err("runtime-derived parameter evaluation must fail closed");
            assert!(
                error.message().contains(expected_reason),
                "unexpected refusal for `{source}`: {error}"
            );
        }
    }

    #[test]
    fn unicode_before_and_inside_substitution_keeps_exact_sources() {
        let source = r#"printf -- λπ"$(echo café)""#;
        let inspected = inspect_shell(source).expect("inspection");
        assert_eq!(inspected.commands[0].source, source);
        assert_eq!(inspected.constructs[0].source, "$(echo café)");
        assert_eq!(inspected.constructs[0].body, "echo café");
        assert!(inspected.constructs[0].quoted);
    }

    #[test]
    fn malformed_syntax_and_dynamic_executables_fail_closed() {
        assert!(inspect_shell("echo $(uname").is_err());
        assert!(inspect_shell("$PROGRAM --version").is_err());
        assert!(inspect_shell("find . -exec \"$PROGRAM\" {} +").is_err());
        assert!(inspect_shell(r#"eval "$(generate-script)""#).is_err());
        assert!(inspect_shell(r#"source "$(choose-file)""#).is_err());
    }

    #[test]
    fn executable_dispatchers_and_interpreters_fail_closed() {
        for source in [
            r#"builtin eval "$(generate-script)""#,
            r#"command source "$(choose-file)""#,
            r#"command find . -exec rm {} +"#,
            r#"env MODE=test sh -c "run-hidden-command""#,
            r#"xargs rm"#,
            r#"/usr/bin/env python3 -c "run_hidden_command()""#,
            r#"/opt/tools/python3.12 -c "run_hidden_command()""#,
        ] {
            let error = inspect_shell(source)
                .expect_err("a dispatcher or interpreter must not bypass inspection");
            assert!(
                error.message().contains("cannot be projected statically"),
                "unexpected refusal for `{source}`: {error}"
            );
        }
    }

    #[test]
    fn deferred_brush_builtins_fail_closed() {
        for source in [r#"trap 'run-hidden-command' EXIT"#, "fc -s"] {
            let error =
                inspect_shell(source).expect_err("a deferred evaluator must not be projected");
            assert!(
                error.message().contains("cannot be projected statically"),
                "unexpected refusal for `{source}`: {error}"
            );
        }
    }

    #[test]
    fn brush_printf_assignment_targets_fail_closed() {
        for source in [
            r#"printf -v result '%s' value"#,
            r#"printf -vresult '%s' value"#,
            r#"printf '-v' result '%s' value"#,
        ] {
            let error =
                inspect_shell(source).expect_err("Brush printf -v must not reparse a target");
            assert!(
                error.message().contains("`printf -v`")
                    && error.message().contains("reparses its assignment target"),
                "unexpected refusal for `{source}`: {error}"
            );
        }

        let dynamic = inspect_shell(r#"option=-v; printf "$option" result '%s' value"#)
            .expect_err("a dynamic first argument could select -v");
        assert!(
            dynamic.message().contains("dynamic first argument")
                && dynamic.message().contains("could select `-v`"),
            "unexpected dynamic-option refusal: {dynamic}"
        );

        inspect_shell(r#"printf -- -v literal"#).expect("`--` makes -v the literal format operand");
        inspect_shell(r#"printf '%s' -v literal"#)
            .expect("arguments after the format operand cannot select -v");
    }

    #[test]
    fn windows_executable_spellings_are_classified_without_changing_projection() {
        for (program, expected) in [
            ("python.exe", "python"),
            ("PowerShell.EXE", "powershell"),
            ("pwsh.exe", "pwsh"),
            ("find.exe", "find"),
            ("cmd.exe", "cmd"),
            ("python.COM", "python"),
            ("python.BAT", "python"),
            ("python.CMD", "python"),
        ] {
            assert_eq!(
                classification_basename(program),
                expected,
                "unexpected classification for {program}"
            );
        }

        for source in [
            r#"C:/Tools/PYTHON.EXE -c "run_hidden_command()""#,
            r#"C:/Tools/PowerShell.ExE -Command "run-hidden-command""#,
            r#"C:/Tools/PwSh.ExE -Command "run-hidden-command""#,
            r#"C:/Windows/System32/CmD.ExE /C "run-hidden-command""#,
        ] {
            let error =
                inspect_shell(source).expect_err("a Windows-spelled interpreter must fail closed");
            assert!(
                error.message().contains("cannot be projected statically"),
                "unexpected refusal for `{source}`: {error}"
            );
        }

        let source = r#"C:/Windows/System32/FIND.EXE . -exec WC.EXE -l {} +"#;
        let inspected = inspect_shell(source).expect("Windows-spelled find inspection");
        let find = &inspected.commands[0];
        assert_eq!(
            find.program.as_deref(),
            Some("C:/Windows/System32/FIND.EXE")
        );
        assert_eq!(find.argv[0], "C:/Windows/System32/FIND.EXE");
        assert_eq!(find.descendant_execs[0].program, "WC.EXE");
        assert_eq!(find.descendant_execs[0].source, "-exec WC.EXE -l {} +");
    }

    #[test]
    fn flattened_inventory_warns_when_control_relationships_are_not_encoded() {
        let inspected =
            inspect_shell("printf a | head -1 && printf b &").expect("flattened inspection");

        assert!(inspected
            .warnings
            .iter()
            .any(|warning| warning.contains("pipeline topology")));
        assert!(inspected
            .warnings
            .iter()
            .any(|warning| warning.contains("conditional relationship")));
        assert!(inspected
            .warnings
            .iter()
            .any(|warning| warning.contains("background lifecycle")));
    }

    #[test]
    fn find_exec_plus_is_a_terminator_only_after_placeholder() {
        let inspected =
            inspect_shell(r#"find . -exec printf '[%s]\n' + {} \;"#).expect("inspection");
        let descendant = &inspected.commands[0].descendant_execs[0];
        assert_eq!(
            descendant.argv,
            vec!["printf", "[%s]\\n", "+", "{}"],
            "a literal plus argument must not truncate the disclosed child argv"
        );
    }

    #[test]
    fn inspection_never_executes_substitutions() {
        let mut suffix = 0_u32;
        let marker = loop {
            let candidate = std::env::temp_dir().join(format!(
                "agent-bridle-shell-inspect-no-exec-{}-{suffix}",
                std::process::id()
            ));
            if !candidate.exists() {
                break candidate;
            }
            suffix += 1;
        };
        let source = format!("echo $(touch '{}')", marker.display());

        let inspected = inspect_shell(&source).expect("inspection");

        assert_eq!(inspected.constructs.len(), 1);
        assert!(!marker.exists(), "inspection executed the substitution");
    }

    #[test]
    fn projection_serializes_with_stable_field_names() {
        let value = serde_json::to_value(inspect_shell("printf ok").expect("inspection"))
            .expect("serialize");
        assert_eq!(value["source"], "printf ok");
        assert_eq!(value["schema_version"], INSPECTION_SCHEMA_VERSION);
        assert_eq!(value["commands"][0]["program"], "printf");
        assert_eq!(value["commands"][0]["argv"][0], "printf");
        assert!(value["commands"][0]["redirects"].is_array());
        assert!(value["commands"][0]["descendant_execs"].is_array());
        assert!(value["constructs"].is_array());
        assert!(value["warnings"].is_array());
    }
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
