//! PURE parsing + mapping behind `newt providers import-hermes` — no fs, no
//! env, no subprocess (the `dgx_pull.rs` discipline; the IO shell is
//! `providers_cmd.rs`).
//!
//! Hermes Agent model-provider plugins are Python files
//! (`$HERMES_HOME/plugins/model-providers/<name>/__init__.py`). Most are
//! DECLARATIVE — a `ProviderProfile(...)` call with literal keyword arguments,
//! passed to `register_provider(...)` — and those transpose onto newt's
//! [`ProviderPreset`] field-for-field. This module reads that declarative
//! subset with a hand-rolled tokenizer + recursive-descent parser over a
//! WHITELIST Python-literal grammar. It NEVER evaluates anything and rejects
//! by default: any statement or expression outside the whitelist — a
//! `ProviderProfile` subclass (a hook-bearing plugin), `def`, decorators,
//! f-strings, names-as-values, foreign imports — becomes a typed
//! [`SkipReason`] that renders as one honest human line.
//!
//! Accepted file shape: any interleaving of comments, blank lines, module
//! docstrings, the two known imports (`from providers import
//! register_provider` / `from providers.base import ProviderProfile`,
//! combined form tolerated), `IDENT = ProviderProfile(<kwargs>)` assignments,
//! and `register_provider(IDENT)` / `register_provider(ProviderProfile(...))`
//! calls. Values are literals only: strings (single/double/triple quotes,
//! `\n \t \r \\ \' \" \xNN \uNNNN` escapes, adjacent-string and `+`
//! concatenation), ints, floats, `True`/`False`/`None`, tuples, lists, and
//! string-keyed dicts — all with trailing commas.

use std::collections::HashSet;

use newt_core::provider_preset::{
    preset_support, ApiMode, AuthType, PresetSupport, ProviderPreset,
};

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// A parsed Python literal from the whitelist grammar. Tuples and lists both
/// land in [`Self::Seq`] (newt has no positional/immutable distinction to
/// keep); dict keys must be string literals.
#[derive(Debug, Clone, PartialEq)]
pub enum PyLiteral {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    None,
    Seq(Vec<Self>),
    Map(Vec<(String, Self)>),
}

/// Why a plugin file (or one profile in it) cannot be imported. Every variant
/// renders as ONE honest human line via `Display`.
#[derive(Debug, Clone, PartialEq)]
pub enum SkipReason {
    /// A `class X(ProviderProfile)` statement — hook-bearing plugins carry
    /// code newt will not execute.
    SubclassesProviderProfile { class_name: String },
    /// A statement outside the whitelist (`def`, decorator, `if`, `try`, …).
    UnsupportedStatement { line: usize, what: String },
    /// An expression outside the literal whitelist (f-string, name-as-value,
    /// other calls, `**kwargs`, comprehension, positional arg, …).
    UnsupportedExpression { line: usize, what: String },
    /// An import of anything but `providers` / `providers.base`.
    ForeignImport { line: usize, module: String },
    /// The file parsed but registered no `ProviderProfile`.
    NoProviderProfile,
    /// A profile variable was assigned but never registered.
    NotRegistered { var: String },
    /// `name` or `base_url` absent (or not a string literal).
    MissingRequiredField { field: String },
    /// An `auth_type` newt cannot drive (unknown string, or a Hermes auth
    /// flow newt has no machinery for).
    UnsupportedAuth { auth_type: String },
    /// An `api_mode` newt has no transport for (unknown string, or
    /// `bedrock_converse`).
    UnsupportedApiMode { api_mode: String },
    /// A base URL newt's transports cannot route (e.g. Gemini's
    /// non-`/v1`-shaped OpenAI-compat path).
    BaseUrlShape { base_url: String, detail: String },
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SubclassesProviderProfile { class_name } => write!(
                f,
                "class `{class_name}` subclasses ProviderProfile — a hook-bearing (code) \
                 plugin newt will not execute; port it by hand"
            ),
            Self::UnsupportedStatement { line, what } => write!(
                f,
                "line {line}: unsupported statement ({what}) — only declarative \
                 ProviderProfile files can be imported as data"
            ),
            Self::UnsupportedExpression { line, what } => write!(
                f,
                "line {line}: unsupported expression ({what}) — only literal values can \
                 be imported as data"
            ),
            Self::ForeignImport { line, module } => write!(
                f,
                "line {line}: import of `{module}` — only `providers` / `providers.base` \
                 imports appear in a declarative plugin"
            ),
            Self::NoProviderProfile => write!(f, "no registered ProviderProfile found"),
            Self::NotRegistered { var } => write!(
                f,
                "profile `{var}` is assigned but never passed to register_provider"
            ),
            Self::MissingRequiredField { field } => write!(
                f,
                "required field `{field}` is missing (or is not a string literal)"
            ),
            Self::UnsupportedAuth { auth_type } => write!(
                f,
                "auth_type `{auth_type}` is not supported — newt only has api_key auth \
                 today (no OAuth / Copilot / AWS machinery)"
            ),
            Self::UnsupportedApiMode { api_mode } => {
                write!(f, "api_mode `{api_mode}` has no newt transport")
            }
            Self::BaseUrlShape { base_url, detail } => {
                write!(f, "base_url `{base_url}`: {detail}")
            }
        }
    }
}

impl std::error::Error for SkipReason {}

/// All REGISTERED profiles' kwargs, in registration (file) order.
pub fn extract_profiles(source: &str) -> Result<Vec<Vec<(String, PyLiteral)>>, SkipReason> {
    extract_inner(source).map_err(|reason| {
        // Refine the rejection: a hook-bearing plugin usually trips on its
        // support imports (`from typing import Any`) lines before its class
        // statement, but "it subclasses ProviderProfile" is the truthful
        // one-line story. The scan only ever REFINES an already-decided
        // rejection — it never accepts anything.
        if matches!(reason, SkipReason::SubclassesProviderProfile { .. }) {
            return reason;
        }
        match find_subclass(source) {
            Some(class_name) => SkipReason::SubclassesProviderProfile { class_name },
            None => reason,
        }
    })
}

/// A `class X(...ProviderProfile...)` header anywhere in the source.
fn find_subclass(source: &str) -> Option<String> {
    for line in source.lines() {
        let Some(rest) = line.trim_start().strip_prefix("class ") else {
            continue;
        };
        let Some((name, bases)) = rest.split_once('(') else {
            continue;
        };
        if bases.contains("ProviderProfile") {
            return Some(name.trim().to_string());
        }
    }
    None
}

fn extract_inner(source: &str) -> Result<Vec<Vec<(String, PyLiteral)>>, SkipReason> {
    let toks = tokenize(source)?;
    let mut p = Parser { toks, pos: 0 };
    let mut assigned: Assigned = Vec::new();
    let mut registered: Vec<Kwargs> = Vec::new();
    let mut registered_vars: HashSet<String> = HashSet::new();
    while p.peek().is_some() {
        if p.eat(&Tok::Newline) {
            continue;
        }
        p.parse_statement(&mut assigned, &mut registered, &mut registered_vars)?;
    }
    // Strict by design: a defined-but-unregistered profile is a red flag
    // (Hermes plugins always register), not something to silently drop.
    for (var, _) in &assigned {
        if !registered_vars.contains(var) {
            return Err(SkipReason::NotRegistered { var: var.clone() });
        }
    }
    if registered.is_empty() {
        return Err(SkipReason::NoProviderProfile);
    }
    Ok(registered)
}

/// kwargs → (preset, carried extra lines). Extras are kwargs newt doesn't
/// mirror (`fixed_temperature`, `default_aux_model`, unknown future fields),
/// each pre-rendered as a `# hermes: <k> = <v> (not used by newt)` comment
/// line for the emitted TOML. `None` values are dropped (Python's default).
/// The auth/api-mode/base-url verdict comes from
/// [`newt_core::provider_preset::preset_support`] on the built candidate —
/// that logic is not duplicated here.
pub fn preset_from_kwargs(
    kwargs: &[(String, PyLiteral)],
) -> Result<(ProviderPreset, Vec<String>), SkipReason> {
    let mut p = ProviderPreset::default();
    let mut extras = Vec::new();
    for (key, value) in kwargs {
        if matches!(value, PyLiteral::None) {
            continue;
        }
        match (key.as_str(), value) {
            ("name", PyLiteral::Str(s)) => p.name = s.clone(),
            ("display_name", PyLiteral::Str(s)) => p.display_name = Some(s.clone()),
            ("description", PyLiteral::Str(s)) => p.description = Some(s.clone()),
            ("signup_url", PyLiteral::Str(s)) => p.signup_url = Some(s.clone()),
            ("base_url", PyLiteral::Str(s)) => p.base_url = s.clone(),
            ("models_url", PyLiteral::Str(s)) => p.models_url = Some(s.clone()),
            ("aliases", v) if str_vec(v).is_some() => {
                p.aliases = str_vec(v).unwrap_or_default();
            }
            ("env_vars", v) if str_vec(v).is_some() => {
                p.env_vars = str_vec(v).unwrap_or_default();
            }
            ("fallback_models", v) if str_vec(v).is_some() => {
                p.fallback_models = str_vec(v).unwrap_or_default();
            }
            ("auth_type", PyLiteral::Str(s)) => match enum_from_str::<AuthType>(s) {
                Some(auth) => p.auth_type = auth,
                None => {
                    return Err(SkipReason::UnsupportedAuth {
                        auth_type: s.clone(),
                    })
                }
            },
            ("api_mode", PyLiteral::Str(s)) => match enum_from_str::<ApiMode>(s) {
                Some(mode) => p.api_mode = mode,
                None => {
                    return Err(SkipReason::UnsupportedApiMode {
                        api_mode: s.clone(),
                    })
                }
            },
            ("default_headers", PyLiteral::Map(pairs))
                if pairs.iter().all(|(_, v)| matches!(v, PyLiteral::Str(_))) =>
            {
                for (k, v) in pairs {
                    if let PyLiteral::Str(s) = v {
                        p.default_headers.insert(k.clone(), s.clone());
                    }
                }
            }
            ("default_max_tokens", PyLiteral::Int(i)) if u32::try_from(*i).is_ok() => {
                p.default_max_tokens = u32::try_from(*i).ok();
            }
            // Everything else — including a mirrored key with a shape newt
            // can't hold — is carried visibly, never silently dropped.
            _ => extras.push(extra_line(key, value)),
        }
    }
    if p.name.trim().is_empty() {
        return Err(SkipReason::MissingRequiredField {
            field: "name".to_string(),
        });
    }
    if p.base_url.trim().is_empty() {
        return Err(SkipReason::MissingRequiredField {
            field: "base_url".to_string(),
        });
    }
    match preset_support(&p) {
        PresetSupport::Supported { .. } => Ok((p, extras)),
        PresetSupport::Unsupported { reason } => {
            if p.auth_type != AuthType::ApiKey {
                Err(SkipReason::UnsupportedAuth {
                    auth_type: serde_name(&p.auth_type),
                })
            } else if p.api_mode == ApiMode::BedrockConverse {
                Err(SkipReason::UnsupportedApiMode {
                    api_mode: serde_name(&p.api_mode),
                })
            } else {
                Err(SkipReason::BaseUrlShape {
                    base_url: p.base_url.clone(),
                    detail: reason,
                })
            }
        }
    }
}

/// Serialize a preset to the drop-in TOML body, with the carried-extra
/// comment lines appended. The body keeps the original `name` (the drop-in
/// loader's stem-wins rule tolerates it).
pub fn render_preset_toml(preset: &ProviderPreset, extras: &[String]) -> String {
    let body = toml::to_string(preset)
        .expect("a ProviderPreset always serializes to TOML (string keys only)");
    let mut out = String::from("# imported from Hermes Agent by `newt providers import-hermes`\n");
    out.push_str(&body);
    if !extras.is_empty() {
        out.push('\n');
        for line in extras {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Mapping helpers
// ---------------------------------------------------------------------------

/// The serde (snake_case) name of a unit enum variant, via serde_json.
pub(crate) fn serde_name<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn enum_from_str<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

fn str_vec(v: &PyLiteral) -> Option<Vec<String>> {
    let PyLiteral::Seq(items) = v else {
        return None;
    };
    items
        .iter()
        .map(|it| match it {
            PyLiteral::Str(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn extra_line(key: &str, value: &PyLiteral) -> String {
    format!("# hermes: {key} = {} (not used by newt)", render_py(value))
}

/// Single-line Python-ish rendering for carried extras (strings escaped, so
/// multi-line values stay one comment line).
fn render_py(v: &PyLiteral) -> String {
    match v {
        PyLiteral::Str(s) => format!("{s:?}"),
        PyLiteral::Int(i) => i.to_string(),
        PyLiteral::Float(x) => x.to_string(),
        PyLiteral::Bool(true) => "True".to_string(),
        PyLiteral::Bool(false) => "False".to_string(),
        PyLiteral::None => "None".to_string(),
        PyLiteral::Seq(items) => format!(
            "[{}]",
            items.iter().map(render_py).collect::<Vec<_>>().join(", ")
        ),
        PyLiteral::Map(pairs) => format!(
            "{{{}}}",
            pairs
                .iter()
                .map(|(k, v)| format!("{k:?}: {}", render_py(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    /// Logical end-of-statement: a newline at bracket depth 0.
    Newline,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Eq,
    Colon,
    Plus,
    Minus,
    Star,
    StarStar,
    At,
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    line: usize,
}

fn unsupported_expr(line: usize, what: impl Into<String>) -> SkipReason {
    SkipReason::UnsupportedExpression {
        line,
        what: what.into(),
    }
}

fn unsupported_stmt(line: usize, what: impl Into<String>) -> SkipReason {
    SkipReason::UnsupportedStatement {
        line,
        what: what.into(),
    }
}

/// Tokenize with Python's implicit line joining: newlines inside `()`/`[]`/
/// `{}` are whitespace; at depth 0 they end a statement. `\`-newline joins.
fn tokenize(source: &str) -> Result<Vec<Token>, SkipReason> {
    let chars: Vec<char> = source.chars().collect();
    let mut toks: Vec<Token> = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut depth = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' => i += 1,
            '\n' => {
                if depth == 0 && toks.last().is_some_and(|t| t.tok != Tok::Newline) {
                    toks.push(Token {
                        tok: Tok::Newline,
                        line,
                    });
                }
                line += 1;
                i += 1;
            }
            '\\' if chars.get(i + 1) == Some(&'\n') => {
                line += 1;
                i += 2;
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '"' | '\'' => {
                let start_line = line;
                let s = read_string(&chars, &mut i, &mut line)?;
                toks.push(Token {
                    tok: Tok::Str(s),
                    line: start_line,
                });
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let ident: String = chars[start..i].iter().collect();
                // A string-prefix letter glued to a quote is a prefixed
                // string literal — f-strings interpolate (evaluation), the
                // rest are outside the whitelist too.
                if matches!(chars.get(i), Some(&('"' | '\'')))
                    && ident.len() <= 2
                    && ident.chars().all(|ch| "fFrRbBuU".contains(ch))
                {
                    if ident.to_ascii_lowercase().contains('f') {
                        return Err(unsupported_expr(line, "f-string"));
                    }
                    return Err(unsupported_expr(
                        line,
                        format!("`{ident}`-prefixed string literal"),
                    ));
                }
                toks.push(Token {
                    tok: Tok::Ident(ident),
                    line,
                });
            }
            c if c.is_ascii_digit() => {
                let tok = read_number(&chars, &mut i, line)?;
                toks.push(Token { tok, line });
            }
            '(' => {
                depth += 1;
                toks.push(Token {
                    tok: Tok::LParen,
                    line,
                });
                i += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                toks.push(Token {
                    tok: Tok::RParen,
                    line,
                });
                i += 1;
            }
            '[' => {
                depth += 1;
                toks.push(Token {
                    tok: Tok::LBracket,
                    line,
                });
                i += 1;
            }
            ']' => {
                depth = depth.saturating_sub(1);
                toks.push(Token {
                    tok: Tok::RBracket,
                    line,
                });
                i += 1;
            }
            '{' => {
                depth += 1;
                toks.push(Token {
                    tok: Tok::LBrace,
                    line,
                });
                i += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                toks.push(Token {
                    tok: Tok::RBrace,
                    line,
                });
                i += 1;
            }
            ',' => {
                toks.push(Token {
                    tok: Tok::Comma,
                    line,
                });
                i += 1;
            }
            '=' => {
                if chars.get(i + 1) == Some(&'=') {
                    return Err(unsupported_expr(line, "`==` comparison"));
                }
                toks.push(Token { tok: Tok::Eq, line });
                i += 1;
            }
            ':' => {
                toks.push(Token {
                    tok: Tok::Colon,
                    line,
                });
                i += 1;
            }
            '+' => {
                toks.push(Token {
                    tok: Tok::Plus,
                    line,
                });
                i += 1;
            }
            '-' => {
                toks.push(Token {
                    tok: Tok::Minus,
                    line,
                });
                i += 1;
            }
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    toks.push(Token {
                        tok: Tok::StarStar,
                        line,
                    });
                    i += 2;
                } else {
                    toks.push(Token {
                        tok: Tok::Star,
                        line,
                    });
                    i += 1;
                }
            }
            '@' => {
                toks.push(Token { tok: Tok::At, line });
                i += 1;
            }
            '.' => {
                toks.push(Token {
                    tok: Tok::Ident(".".to_string()),
                    line,
                });
                i += 1;
            }
            other => {
                return Err(unsupported_expr(
                    line,
                    format!("unsupported character `{other}`"),
                ))
            }
        }
    }
    Ok(toks)
}

/// Read a string literal at `chars[*i]` (a quote). Handles single/double/
/// triple quotes and the whitelist escapes.
fn read_string(chars: &[char], i: &mut usize, line: &mut usize) -> Result<String, SkipReason> {
    let quote = chars[*i];
    let start_line = *line;
    let triple = chars.get(*i + 1) == Some(&quote) && chars.get(*i + 2) == Some(&quote);
    *i += if triple { 3 } else { 1 };
    let mut out = String::new();
    loop {
        let Some(&c) = chars.get(*i) else {
            return Err(unsupported_expr(start_line, "unterminated string literal"));
        };
        if triple {
            if c == quote && chars.get(*i + 1) == Some(&quote) && chars.get(*i + 2) == Some(&quote)
            {
                *i += 3;
                return Ok(out);
            }
        } else if c == quote {
            *i += 1;
            return Ok(out);
        }
        if c == '\n' {
            if !triple {
                return Err(unsupported_expr(start_line, "unterminated string literal"));
            }
            *line += 1;
            out.push('\n');
            *i += 1;
            continue;
        }
        if c == '\\' {
            *i += 1;
            let Some(&esc) = chars.get(*i) else {
                return Err(unsupported_expr(start_line, "unterminated string literal"));
            };
            match esc {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '\\' => out.push('\\'),
                '\'' => out.push('\''),
                '"' => out.push('"'),
                '\n' => *line += 1, // escaped newline: joined away
                'x' => {
                    let ch = hex_escape(chars, *i + 1, 2)
                        .ok_or_else(|| unsupported_expr(*line, "invalid `\\xNN` escape"))?;
                    out.push(ch);
                    *i += 2;
                }
                'u' => {
                    let ch = hex_escape(chars, *i + 1, 4)
                        .ok_or_else(|| unsupported_expr(*line, "invalid `\\uNNNN` escape"))?;
                    out.push(ch);
                    *i += 4;
                }
                other => {
                    return Err(unsupported_expr(
                        *line,
                        format!("unsupported string escape `\\{other}`"),
                    ))
                }
            }
            *i += 1;
            continue;
        }
        out.push(c);
        *i += 1;
    }
}

/// Decode `len` hex digits starting at `chars[at]` into a char.
fn hex_escape(chars: &[char], at: usize, len: usize) -> Option<char> {
    let digits: String = chars.get(at..at + len)?.iter().collect();
    let code = u32::from_str_radix(&digits, 16).ok()?;
    char::from_u32(code)
}

/// Read an int or float starting at a digit.
fn read_number(chars: &[char], i: &mut usize, line: usize) -> Result<Tok, SkipReason> {
    let start = *i;
    let mut is_float = false;
    while *i < chars.len() && chars[*i].is_ascii_digit() {
        *i += 1;
    }
    if chars.get(*i) == Some(&'.') {
        is_float = true;
        *i += 1;
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            *i += 1;
        }
    }
    if matches!(chars.get(*i), Some(&('e' | 'E'))) {
        is_float = true;
        *i += 1;
        if matches!(chars.get(*i), Some(&('+' | '-'))) {
            *i += 1;
        }
        if !chars.get(*i).is_some_and(char::is_ascii_digit) {
            return Err(unsupported_expr(line, "malformed number literal"));
        }
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            *i += 1;
        }
    }
    let text: String = chars[start..*i].iter().collect();
    if is_float {
        text.parse::<f64>()
            .map(Tok::Float)
            .map_err(|_| unsupported_expr(line, format!("malformed float literal `{text}`")))
    } else {
        text.parse::<i64>()
            .map(Tok::Int)
            .map_err(|_| unsupported_expr(line, format!("integer literal `{text}` out of range")))
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// One profile's keyword arguments, in source order.
type Kwargs = Vec<(String, PyLiteral)>;
/// `IDENT = ProviderProfile(...)` bindings, in source order.
type Assigned = Vec<(String, Kwargs)>;

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

/// Statement keywords that immediately identify a non-declarative file.
const REJECT_KEYWORDS: &[&str] = &[
    "if", "elif", "else", "for", "while", "try", "except", "finally", "with", "return", "raise",
    "pass", "assert", "del", "global", "nonlocal", "lambda", "yield", "async", "match",
];

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.tok)
    }

    fn peek2(&self) -> Option<&Tok> {
        self.toks.get(self.pos + 1).map(|t| &t.tok)
    }

    /// The line of the current token (or the last token at EOF).
    fn line(&self) -> usize {
        self.toks
            .get(self.pos)
            .or_else(|| self.toks.last())
            .map_or(1, |t| t.line)
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|t| t.tok.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// A statement must end at a logical newline (or EOF).
    fn end_of_statement(&mut self) -> Result<(), SkipReason> {
        let line = self.line();
        if self.peek().is_none() || self.eat(&Tok::Newline) {
            Ok(())
        } else {
            Err(unsupported_stmt(
                line,
                "trailing tokens after the statement",
            ))
        }
    }

    fn parse_statement(
        &mut self,
        assigned: &mut Assigned,
        registered: &mut Vec<Kwargs>,
        registered_vars: &mut HashSet<String>,
    ) -> Result<(), SkipReason> {
        let line = self.line();
        match self.peek() {
            Some(Tok::At) => Err(unsupported_stmt(line, "decorator")),
            // A bare string statement is a docstring — pure data, ignored.
            Some(Tok::Str(_)) => {
                self.bump();
                self.end_of_statement()
            }
            Some(Tok::Ident(ident)) => {
                let ident = ident.clone();
                match ident.as_str() {
                    "from" => self.parse_from_import(),
                    "import" => {
                        self.bump();
                        let module = self.parse_dotted_name()?;
                        Err(SkipReason::ForeignImport { line, module })
                    }
                    "class" => self.parse_class(),
                    "def" => Err(unsupported_stmt(line, "function definition (def)")),
                    kw if REJECT_KEYWORDS.contains(&kw) => {
                        Err(unsupported_stmt(line, format!("`{kw}` statement")))
                    }
                    "register_provider" => {
                        self.bump();
                        self.parse_register(assigned, registered, registered_vars)
                    }
                    _ => self.parse_assignment(&ident, assigned),
                }
            }
            Some(_) => Err(unsupported_stmt(line, "expression statement")),
            None => Ok(()),
        }
    }

    /// `IDENT.IDENT.IDENT` — used for module paths.
    fn parse_dotted_name(&mut self) -> Result<String, SkipReason> {
        let line = self.line();
        let Some(Tok::Ident(first)) = self.bump() else {
            return Err(unsupported_stmt(line, "malformed import"));
        };
        let mut name = first;
        while self.eat(&Tok::Ident(".".to_string())) {
            let Some(Tok::Ident(part)) = self.bump() else {
                return Err(unsupported_stmt(line, "malformed import"));
            };
            name.push('.');
            name.push_str(&part);
        }
        Ok(name)
    }

    /// `from providers import register_provider[, ProviderProfile]` /
    /// `from providers.base import ProviderProfile` — anything else rejects.
    fn parse_from_import(&mut self) -> Result<(), SkipReason> {
        let line = self.line();
        self.bump(); // `from`
        let module = self.parse_dotted_name()?;
        if self.bump() != Some(Tok::Ident("import".to_string())) {
            return Err(unsupported_stmt(line, "malformed `from` import"));
        }
        if module != "providers" && module != "providers.base" {
            return Err(SkipReason::ForeignImport { line, module });
        }
        loop {
            match self.bump() {
                Some(Tok::Ident(name))
                    if name == "register_provider" || name == "ProviderProfile" => {}
                Some(Tok::Ident(name)) => {
                    return Err(unsupported_stmt(
                        line,
                        format!("import of `{name}` from `{module}`"),
                    ))
                }
                _ => return Err(unsupported_stmt(line, "malformed import")),
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.end_of_statement()
    }

    /// Any `class` statement rejects; one subclassing `ProviderProfile` gets
    /// the targeted hook-bearing-plugin reason.
    fn parse_class(&mut self) -> Result<(), SkipReason> {
        let line = self.line();
        self.bump(); // `class`
        let Some(Tok::Ident(class_name)) = self.bump() else {
            return Err(unsupported_stmt(line, "class definition"));
        };
        while let Some(tok) = self.bump() {
            match tok {
                Tok::Ident(ref base) if base == "ProviderProfile" => {
                    return Err(SkipReason::SubclassesProviderProfile { class_name });
                }
                Tok::Colon | Tok::Newline => break,
                _ => {}
            }
        }
        Err(unsupported_stmt(
            line,
            format!("class `{class_name}` definition"),
        ))
    }

    /// `register_provider(IDENT)` / `register_provider(ProviderProfile(...))`.
    fn parse_register(
        &mut self,
        assigned: &mut Assigned,
        registered: &mut Vec<Kwargs>,
        registered_vars: &mut HashSet<String>,
    ) -> Result<(), SkipReason> {
        let line = self.line();
        if !self.eat(&Tok::LParen) {
            return Err(unsupported_stmt(line, "bare `register_provider`"));
        }
        match self.peek() {
            Some(Tok::Ident(name))
                if name == "ProviderProfile" && self.peek2() == Some(&Tok::LParen) =>
            {
                self.bump();
                self.bump();
                let kwargs = self.parse_kwargs()?;
                registered.push(kwargs);
            }
            Some(Tok::Ident(var)) => {
                let var = var.clone();
                self.bump();
                match assigned.iter().find(|(n, _)| *n == var) {
                    Some((_, kwargs)) => {
                        registered.push(kwargs.clone());
                        registered_vars.insert(var);
                    }
                    None => {
                        return Err(unsupported_expr(
                            line,
                            format!(
                                "register_provider argument `{var}` is not a ProviderProfile \
                                 assigned in this file"
                            ),
                        ))
                    }
                }
            }
            _ => {
                return Err(unsupported_expr(
                    line,
                    "register_provider takes a ProviderProfile (or a variable bound to one)",
                ))
            }
        }
        self.eat(&Tok::Comma); // trailing comma tolerated
        if !self.eat(&Tok::RParen) {
            return Err(unsupported_expr(line, "malformed register_provider call"));
        }
        self.end_of_statement()
    }

    /// `IDENT = ProviderProfile(<kwargs>)` — the only assignment allowed.
    fn parse_assignment(&mut self, name: &str, assigned: &mut Assigned) -> Result<(), SkipReason> {
        let line = self.line();
        self.bump(); // the target IDENT
        match self.peek() {
            Some(Tok::Eq) => {
                self.bump();
                match self.peek() {
                    Some(Tok::Ident(callee)) if callee == "ProviderProfile" => {
                        self.bump();
                        if !self.eat(&Tok::LParen) {
                            return Err(unsupported_expr(
                                line,
                                "`ProviderProfile` used as a value (not called)",
                            ));
                        }
                        let kwargs = self.parse_kwargs()?;
                        self.end_of_statement()?;
                        match assigned.iter_mut().find(|(n, _)| n == name) {
                            Some(slot) => slot.1 = kwargs,
                            None => assigned.push((name.to_string(), kwargs)),
                        }
                        Ok(())
                    }
                    Some(Tok::Ident(other)) => {
                        let other = other.clone();
                        if self.peek2() == Some(&Tok::LParen) {
                            Err(unsupported_expr(line, format!("call to `{other}`")))
                        } else {
                            Err(unsupported_expr(line, format!("name `{other}` as a value")))
                        }
                    }
                    _ => Err(unsupported_expr(
                        line,
                        "assignment value is not a ProviderProfile(...) call",
                    )),
                }
            }
            Some(Tok::LParen) => Err(unsupported_expr(line, format!("call to `{name}`"))),
            _ => Err(unsupported_stmt(
                line,
                format!("statement starting with `{name}`"),
            )),
        }
    }

    /// Keyword-only argument list; the opening `(` is already consumed and
    /// the matching `)` is consumed on success.
    fn parse_kwargs(&mut self) -> Result<Vec<(String, PyLiteral)>, SkipReason> {
        let mut out = Vec::new();
        loop {
            if self.eat(&Tok::RParen) {
                return Ok(out);
            }
            let line = self.line();
            match self.peek() {
                Some(Tok::StarStar) => return Err(unsupported_expr(line, "**kwargs")),
                Some(Tok::Star) => return Err(unsupported_expr(line, "*args")),
                Some(Tok::Ident(key)) if self.peek2() == Some(&Tok::Eq) => {
                    let key = key.clone();
                    self.pos += 2;
                    let value = self.parse_value()?;
                    out.push((key, value));
                }
                _ => return Err(unsupported_expr(line, "positional argument")),
            }
            let line = self.line();
            if self.eat(&Tok::Comma) {
                continue;
            }
            if self.eat(&Tok::RParen) {
                return Ok(out);
            }
            return Err(unsupported_expr(
                line,
                "expected `,` or `)` in the argument list",
            ));
        }
    }

    /// A literal value, including `+` concatenation of string literals.
    fn parse_value(&mut self) -> Result<PyLiteral, SkipReason> {
        let mut value = self.parse_atom()?;
        while self.eat(&Tok::Plus) {
            let line = self.line();
            let rhs = self.parse_atom()?;
            match (&mut value, rhs) {
                (PyLiteral::Str(a), PyLiteral::Str(b)) => a.push_str(&b),
                _ => {
                    return Err(unsupported_expr(
                        line,
                        "`+` concatenation of non-string literals",
                    ))
                }
            }
        }
        Ok(value)
    }

    fn parse_atom(&mut self) -> Result<PyLiteral, SkipReason> {
        let line = self.line();
        match self.bump() {
            Some(Tok::Str(s)) => {
                // Implicit adjacent-string concatenation: "a" "b" == "ab".
                let mut s = s;
                while let Some(Tok::Str(next)) = self.peek() {
                    let next = next.clone();
                    self.pos += 1;
                    s.push_str(&next);
                }
                Ok(PyLiteral::Str(s))
            }
            Some(Tok::Int(i)) => Ok(PyLiteral::Int(i)),
            Some(Tok::Float(x)) => Ok(PyLiteral::Float(x)),
            Some(Tok::Minus) => match self.bump() {
                Some(Tok::Int(i)) => Ok(PyLiteral::Int(-i)),
                Some(Tok::Float(x)) => Ok(PyLiteral::Float(-x)),
                _ => Err(unsupported_expr(line, "unary `-` on a non-number")),
            },
            Some(Tok::Ident(name)) => match name.as_str() {
                "True" => Ok(PyLiteral::Bool(true)),
                "False" => Ok(PyLiteral::Bool(false)),
                "None" => Ok(PyLiteral::None),
                _ => {
                    if self.peek() == Some(&Tok::LParen) {
                        Err(unsupported_expr(line, format!("call to `{name}`")))
                    } else {
                        Err(unsupported_expr(line, format!("name `{name}` as a value")))
                    }
                }
            },
            Some(Tok::LParen) => self.parse_tuple_or_paren(),
            Some(Tok::LBracket) => self.parse_list(),
            Some(Tok::LBrace) => self.parse_dict(),
            _ => Err(unsupported_expr(line, "expected a literal value")),
        }
    }

    /// After a sequence element: `,` continues, the closer ends, `for` is a
    /// comprehension (rejected), anything else is malformed.
    fn seq_sep(&mut self, closer: &Tok) -> Result<bool, SkipReason> {
        let line = self.line();
        if self.eat(&Tok::Comma) {
            return Ok(false);
        }
        if self.eat(closer) {
            return Ok(true);
        }
        if let Some(Tok::Ident(kw)) = self.peek() {
            if kw == "for" {
                return Err(unsupported_expr(line, "comprehension"));
            }
        }
        Err(unsupported_expr(
            line,
            "expected `,` or the closing bracket",
        ))
    }

    /// `(` already consumed: `()`, `(v)` (grouping), or `(v, ...)` (tuple).
    fn parse_tuple_or_paren(&mut self) -> Result<PyLiteral, SkipReason> {
        if self.eat(&Tok::RParen) {
            return Ok(PyLiteral::Seq(Vec::new()));
        }
        let first = self.parse_value()?;
        if self.eat(&Tok::RParen) {
            return Ok(first); // parenthesized single value
        }
        let mut items = vec![first];
        loop {
            if self.seq_sep(&Tok::RParen)? || self.eat(&Tok::RParen) {
                return Ok(PyLiteral::Seq(items));
            }
            items.push(self.parse_value()?);
        }
    }

    /// `[` already consumed.
    fn parse_list(&mut self) -> Result<PyLiteral, SkipReason> {
        let mut items = Vec::new();
        if self.eat(&Tok::RBracket) {
            return Ok(PyLiteral::Seq(items));
        }
        loop {
            items.push(self.parse_value()?);
            if self.seq_sep(&Tok::RBracket)? || self.eat(&Tok::RBracket) {
                return Ok(PyLiteral::Seq(items));
            }
        }
    }

    /// `{` already consumed; keys must be string literals.
    fn parse_dict(&mut self) -> Result<PyLiteral, SkipReason> {
        let mut pairs = Vec::new();
        if self.eat(&Tok::RBrace) {
            return Ok(PyLiteral::Map(pairs));
        }
        loop {
            let line = self.line();
            let key = match self.parse_value()? {
                PyLiteral::Str(s) => s,
                _ => return Err(unsupported_expr(line, "non-string dict key")),
            };
            if !self.eat(&Tok::Colon) {
                return Err(unsupported_expr(line, "expected `:` in dict literal"));
            }
            let value = self.parse_value()?;
            pairs.push((key, value));
            if self.seq_sep(&Tok::RBrace)? || self.eat(&Tok::RBrace) {
                return Ok(PyLiteral::Map(pairs));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — pure tables, no fs, no env
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::provider_preset::parse_preset_file;

    /// The canonical declarative plugin from the Nous Research docs, verbatim.
    const ACME: &str = r#"from providers import register_provider
from providers.base import ProviderProfile

acme = ProviderProfile(
    name="acme-inference",
    aliases=("acme",),
    display_name="Acme Inference",
    signup_url="https://acme.example.com/keys",
    env_vars=("ACME_API_KEY", "ACME_BASE_URL"),
    base_url="https://api.acme.example.com/v1",
    auth_type="api_key",
    default_aux_model="acme-small-fast",
    fallback_models=("acme-large-v3", "acme-small-fast"),
)
register_provider(acme)
"#;

    // --- accept cases ---

    #[test]
    fn canonical_acme_file_extracts_and_maps() {
        let profiles = extract_profiles(ACME).unwrap();
        assert_eq!(profiles.len(), 1);
        let (preset, extras) = preset_from_kwargs(&profiles[0]).unwrap();
        assert_eq!(preset.name, "acme-inference");
        assert_eq!(preset.aliases, vec!["acme"]);
        assert_eq!(preset.display_name.as_deref(), Some("Acme Inference"));
        assert_eq!(
            preset.signup_url.as_deref(),
            Some("https://acme.example.com/keys")
        );
        assert_eq!(preset.env_vars, vec!["ACME_API_KEY", "ACME_BASE_URL"]);
        assert_eq!(preset.base_url, "https://api.acme.example.com/v1");
        assert_eq!(preset.auth_type, AuthType::ApiKey);
        assert_eq!(preset.api_mode, ApiMode::ChatCompletions);
        assert_eq!(
            preset.fallback_models,
            vec!["acme-large-v3", "acme-small-fast"]
        );
        // default_aux_model is not mirrored — carried as one comment line.
        assert_eq!(
            extras,
            vec![r#"# hermes: default_aux_model = "acme-small-fast" (not used by newt)"#]
        );
        assert!(matches!(
            preset_support(&preset),
            PresetSupport::Supported { .. }
        ));
    }

    #[test]
    fn two_profiles_one_file_in_registration_order() {
        let src = r#"
from providers import register_provider, ProviderProfile

first = ProviderProfile(name="first", base_url="https://a.example.com/v1")
second = ProviderProfile(name="second", base_url="https://b.example.com/v1")
register_provider(first)
register_provider(second)
"#;
        let profiles = extract_profiles(src).unwrap();
        assert_eq!(profiles.len(), 2);
        assert_eq!(
            profiles[0][0],
            ("name".to_string(), PyLiteral::Str("first".to_string()))
        );
        assert_eq!(
            profiles[1][0],
            ("name".to_string(), PyLiteral::Str("second".to_string()))
        );
    }

    #[test]
    fn inline_register_provider_call_is_accepted() {
        let src = r#"
from providers import register_provider
from providers.base import ProviderProfile

register_provider(ProviderProfile(
    name="inline",
    base_url="https://inline.example.com/v1",
))
"#;
        let profiles = extract_profiles(src).unwrap();
        assert_eq!(profiles.len(), 1);
        let (preset, extras) = preset_from_kwargs(&profiles[0]).unwrap();
        assert_eq!(preset.name, "inline");
        assert!(extras.is_empty());
    }

    #[test]
    fn dict_headers_map_onto_default_headers() {
        let src = r#"
from providers.base import ProviderProfile
from providers import register_provider

p = ProviderProfile(
    name="headed",
    base_url="https://headed.example.com/v1",
    default_headers={"X-Title": "newt", "HTTP-Referer": "https://example.com",},
    default_max_tokens=8192,
)
register_provider(p)
"#;
        let profiles = extract_profiles(src).unwrap();
        let (preset, extras) = preset_from_kwargs(&profiles[0]).unwrap();
        assert_eq!(preset.default_headers.get("X-Title").unwrap(), "newt");
        assert_eq!(
            preset.default_headers.get("HTTP-Referer").unwrap(),
            "https://example.com"
        );
        assert_eq!(preset.default_max_tokens, Some(8192));
        assert!(extras.is_empty());
    }

    #[test]
    fn negative_float_bool_and_none_values_parse() {
        let src = r#"
from providers import register_provider, ProviderProfile
p = ProviderProfile(
    name="numeric",
    base_url="https://n.example.com/v1",
    fixed_temperature=0.6,
    priority=-2,
    exponent=1.5e-3,
    streaming=True,
    plain=False,
    organization=None,
)
register_provider(p)
"#;
        let profiles = extract_profiles(src).unwrap();
        let kwargs = &profiles[0];
        assert!(kwargs.contains(&("fixed_temperature".to_string(), PyLiteral::Float(0.6))));
        assert!(kwargs.contains(&("priority".to_string(), PyLiteral::Int(-2))));
        assert!(kwargs.contains(&("exponent".to_string(), PyLiteral::Float(1.5e-3))));
        assert!(kwargs.contains(&("streaming".to_string(), PyLiteral::Bool(true))));
        assert!(kwargs.contains(&("plain".to_string(), PyLiteral::Bool(false))));
        assert!(kwargs.contains(&("organization".to_string(), PyLiteral::None)));
        let (_, extras) = preset_from_kwargs(kwargs).unwrap();
        // None dropped (Python default); the rest are carried, one line each.
        assert_eq!(extras.len(), 5);
        assert!(extras
            .iter()
            .any(|l| l == "# hermes: fixed_temperature = 0.6 (not used by newt)"));
        assert!(extras
            .iter()
            .any(|l| l == "# hermes: streaming = True (not used by newt)"));
    }

    #[test]
    fn triple_quoted_and_escaped_strings_parse() {
        let src = "from providers import register_provider, ProviderProfile\np = ProviderProfile(\n    name=\"esc\",\n    base_url=\"https://esc.example.com/v1\",\n    description=\"\"\"multi\nline \\x41\\u00e9 \\\"quoted\\\" tab\\there\"\"\",\n)\nregister_provider(p)\n";
        let profiles = extract_profiles(src).unwrap();
        let (preset, _) = preset_from_kwargs(&profiles[0]).unwrap();
        assert_eq!(
            preset.description.as_deref(),
            Some("multi\nline A\u{e9} \"quoted\" tab\there")
        );
    }

    #[test]
    fn adjacent_and_plus_string_concatenation() {
        let src = r#"
from providers import register_provider, ProviderProfile
p = ProviderProfile(
    name="concat",
    base_url="https://api." "concat.example" + ".com/v1",
)
register_provider(p)
"#;
        let profiles = extract_profiles(src).unwrap();
        let (preset, _) = preset_from_kwargs(&profiles[0]).unwrap();
        assert_eq!(preset.base_url, "https://api.concat.example.com/v1");
    }

    #[test]
    fn lists_tuples_trailing_commas_and_docstrings_are_tolerated() {
        let src = r#""""Module docstring — a real-world plugin habit."""
# comment
from providers import register_provider
from providers.base import ProviderProfile

p = ProviderProfile(
    name="mixed",
    base_url="https://m.example.com/v1",
    aliases=["m", "mx",],
    fallback_models=("m-large",),
)
register_provider(p,)
"#;
        let profiles = extract_profiles(src).unwrap();
        let (preset, _) = preset_from_kwargs(&profiles[0]).unwrap();
        assert_eq!(preset.aliases, vec!["m", "mx"]);
        assert_eq!(preset.fallback_models, vec!["m-large"]);
    }

    // --- skip cases: the exact SkipReason variant ---

    #[test]
    fn subclass_plugin_is_rejected_with_class_name() {
        let src = r#"
from providers.base import ProviderProfile

class FancyProvider(ProviderProfile):
    def fetch_models(self):
        return ["fancy-large"]
"#;
        assert_eq!(
            extract_profiles(src).unwrap_err(),
            SkipReason::SubclassesProviderProfile {
                class_name: "FancyProvider".to_string()
            }
        );
    }

    /// The real gemini/anthropic plugin shape: support imports (`typing`,
    /// `json`) come BEFORE the class statement. The subclass diagnosis must
    /// win over the incidental foreign-import one.
    #[test]
    fn subclass_reason_wins_over_earlier_foreign_imports() {
        let src = r#""""Google Gemini provider profiles."""

from typing import Any

from providers import register_provider
from providers.base import ProviderProfile


class GeminiProfile(ProviderProfile):
    def build_extra_body(self, **context: Any):
        return {}
"#;
        assert_eq!(
            extract_profiles(src).unwrap_err(),
            SkipReason::SubclassesProviderProfile {
                class_name: "GeminiProfile".to_string()
            }
        );
    }

    /// Real-world unmirrored kwargs (xiaomi/kilocode habits) must carry as
    /// extras, never error.
    #[test]
    fn real_world_extra_kwargs_carry_as_comments() {
        let src = r#"
from providers import register_provider
from providers.base import ProviderProfile

xiaomi = ProviderProfile(
    name="xiaomi",
    base_url="https://api.xiaomimimo.com/v1",
    supports_health_check=False,  # /v1/models returns 401 even with valid key
    supports_vision=True,
    supports_vision_tool_messages=False,
    hostname="api.xiaomimimo.com",
    default_aux_model="mimo-small",
    fixed_temperature=0.3,
)
register_provider(xiaomi)
"#;
        let profiles = extract_profiles(src).unwrap();
        let (preset, extras) = preset_from_kwargs(&profiles[0]).unwrap();
        assert_eq!(preset.name, "xiaomi");
        assert_eq!(extras.len(), 6);
        for key in [
            "supports_health_check",
            "supports_vision",
            "supports_vision_tool_messages",
            "hostname",
            "default_aux_model",
            "fixed_temperature",
        ] {
            assert!(
                extras
                    .iter()
                    .any(|l| l.contains(&format!("# hermes: {key} = "))),
                "missing extra for {key}"
            );
        }
    }

    #[test]
    fn def_statement_is_rejected() {
        let src = "def helper():\n    return 1\n";
        assert!(matches!(
            extract_profiles(src).unwrap_err(),
            SkipReason::UnsupportedStatement { .. }
        ));
    }

    #[test]
    fn decorator_is_rejected() {
        let src = "@register_provider\ndef thing():\n    pass\n";
        assert!(matches!(
            extract_profiles(src).unwrap_err(),
            SkipReason::UnsupportedStatement { .. }
        ));
    }

    #[test]
    fn if_statement_is_rejected() {
        let src = "if True:\n    pass\n";
        assert!(matches!(
            extract_profiles(src).unwrap_err(),
            SkipReason::UnsupportedStatement { .. }
        ));
    }

    #[test]
    fn f_string_is_rejected() {
        let src = "p = ProviderProfile(name=f\"acme-{region}\")\n";
        let err = extract_profiles(src).unwrap_err();
        assert_eq!(
            err,
            SkipReason::UnsupportedExpression {
                line: 1,
                what: "f-string".to_string()
            }
        );
    }

    #[test]
    fn name_as_value_is_rejected() {
        let src = "p = ProviderProfile(name=\"x\", base_url=BASE)\nregister_provider(p)\n";
        let err = extract_profiles(src).unwrap_err();
        assert!(
            matches!(err, SkipReason::UnsupportedExpression { ref what, .. } if what.contains("BASE")),
            "{err}"
        );
    }

    #[test]
    fn other_calls_are_rejected() {
        let src = "p = os.getenv(\"X\")\n";
        assert!(matches!(
            extract_profiles(src).unwrap_err(),
            SkipReason::UnsupportedExpression { .. } | SkipReason::UnsupportedStatement { .. }
        ));
        let src2 = "p = ProviderProfile(name=\"x\", base_url=make_url())\n";
        let err = extract_profiles(src2).unwrap_err();
        assert!(
            matches!(err, SkipReason::UnsupportedExpression { ref what, .. } if what.contains("make_url")),
            "{err}"
        );
    }

    #[test]
    fn positional_argument_is_rejected() {
        let src = "p = ProviderProfile(\"acme\")\n";
        let err = extract_profiles(src).unwrap_err();
        assert!(
            matches!(err, SkipReason::UnsupportedExpression { ref what, .. } if what == "positional argument"),
            "{err}"
        );
    }

    #[test]
    fn star_star_kwargs_is_rejected() {
        let src = "p = ProviderProfile(name=\"x\", **common)\n";
        let err = extract_profiles(src).unwrap_err();
        assert!(
            matches!(err, SkipReason::UnsupportedExpression { ref what, .. } if what == "**kwargs"),
            "{err}"
        );
    }

    #[test]
    fn comprehension_is_rejected() {
        let src = "p = ProviderProfile(name=\"x\", fallback_models=[\"m\" for m in models])\n";
        let err = extract_profiles(src).unwrap_err();
        assert!(
            matches!(err, SkipReason::UnsupportedExpression { ref what, .. } if what == "comprehension"),
            "{err}"
        );
    }

    #[test]
    fn foreign_import_is_rejected() {
        assert_eq!(
            extract_profiles("import os\n").unwrap_err(),
            SkipReason::ForeignImport {
                line: 1,
                module: "os".to_string()
            }
        );
        assert_eq!(
            extract_profiles("from pathlib import Path\n").unwrap_err(),
            SkipReason::ForeignImport {
                line: 1,
                module: "pathlib".to_string()
            }
        );
    }

    #[test]
    fn unregistered_profile_var_is_rejected() {
        let src = "from providers.base import ProviderProfile\np = ProviderProfile(name=\"x\", base_url=\"https://x.example.com/v1\")\n";
        assert_eq!(
            extract_profiles(src).unwrap_err(),
            SkipReason::NotRegistered {
                var: "p".to_string()
            }
        );
    }

    #[test]
    fn empty_and_import_only_files_have_no_profile() {
        assert_eq!(
            extract_profiles("").unwrap_err(),
            SkipReason::NoProviderProfile
        );
        assert_eq!(
            extract_profiles("# just a comment\n\n").unwrap_err(),
            SkipReason::NoProviderProfile
        );
        assert_eq!(
            extract_profiles("from providers import register_provider\n").unwrap_err(),
            SkipReason::NoProviderProfile
        );
    }

    #[test]
    fn missing_name_and_base_url_are_typed() {
        let no_name = vec![(
            "base_url".to_string(),
            PyLiteral::Str("https://x.example.com/v1".to_string()),
        )];
        assert_eq!(
            preset_from_kwargs(&no_name).unwrap_err(),
            SkipReason::MissingRequiredField {
                field: "name".to_string()
            }
        );
        let no_base = vec![("name".to_string(), PyLiteral::Str("x".to_string()))];
        assert_eq!(
            preset_from_kwargs(&no_base).unwrap_err(),
            SkipReason::MissingRequiredField {
                field: "base_url".to_string()
            }
        );
    }

    #[test]
    fn unknown_auth_string_is_unsupported_auth() {
        let kwargs = vec![
            ("name".to_string(), PyLiteral::Str("x".to_string())),
            (
                "base_url".to_string(),
                PyLiteral::Str("https://x.example.com/v1".to_string()),
            ),
            (
                "auth_type".to_string(),
                PyLiteral::Str("oauth_magic".to_string()),
            ),
        ];
        assert_eq!(
            preset_from_kwargs(&kwargs).unwrap_err(),
            SkipReason::UnsupportedAuth {
                auth_type: "oauth_magic".to_string()
            }
        );
    }

    #[test]
    fn known_but_undriveable_auth_is_unsupported_auth() {
        let kwargs = vec![
            ("name".to_string(), PyLiteral::Str("x".to_string())),
            (
                "base_url".to_string(),
                PyLiteral::Str("https://x.example.com/v1".to_string()),
            ),
            (
                "auth_type".to_string(),
                PyLiteral::Str("oauth_device_code".to_string()),
            ),
        ];
        assert_eq!(
            preset_from_kwargs(&kwargs).unwrap_err(),
            SkipReason::UnsupportedAuth {
                auth_type: "oauth_device_code".to_string()
            }
        );
    }

    #[test]
    fn bedrock_converse_is_unsupported_api_mode() {
        let kwargs = vec![
            ("name".to_string(), PyLiteral::Str("x".to_string())),
            (
                "base_url".to_string(),
                PyLiteral::Str("https://x.example.com/v1".to_string()),
            ),
            (
                "api_mode".to_string(),
                PyLiteral::Str("bedrock_converse".to_string()),
            ),
        ];
        assert_eq!(
            preset_from_kwargs(&kwargs).unwrap_err(),
            SkipReason::UnsupportedApiMode {
                api_mode: "bedrock_converse".to_string()
            }
        );
    }

    #[test]
    fn gemini_style_base_url_is_a_shape_error() {
        let kwargs = vec![
            ("name".to_string(), PyLiteral::Str("gemini".to_string())),
            (
                "base_url".to_string(),
                PyLiteral::Str(
                    "https://generativelanguage.googleapis.com/v1beta/openai/".to_string(),
                ),
            ),
        ];
        let err = preset_from_kwargs(&kwargs).unwrap_err();
        assert!(
            matches!(err, SkipReason::BaseUrlShape { ref detail, .. } if detail.contains("not /v1-shaped")),
            "{err}"
        );
    }

    #[test]
    fn every_skip_reason_renders_one_line() {
        let reasons = [
            SkipReason::SubclassesProviderProfile {
                class_name: "X".into(),
            },
            SkipReason::UnsupportedStatement {
                line: 3,
                what: "`if` statement".into(),
            },
            SkipReason::UnsupportedExpression {
                line: 4,
                what: "f-string".into(),
            },
            SkipReason::ForeignImport {
                line: 1,
                module: "os".into(),
            },
            SkipReason::NoProviderProfile,
            SkipReason::NotRegistered { var: "p".into() },
            SkipReason::MissingRequiredField {
                field: "name".into(),
            },
            SkipReason::UnsupportedAuth {
                auth_type: "copilot".into(),
            },
            SkipReason::UnsupportedApiMode {
                api_mode: "bedrock_converse".into(),
            },
            SkipReason::BaseUrlShape {
                base_url: "https://x".into(),
                detail: "nope".into(),
            },
        ];
        for r in &reasons {
            let line = r.to_string();
            assert!(!line.is_empty());
            assert!(!line.contains('\n'), "one line, always: {line}");
        }
    }

    // --- render + round-trip ---

    #[test]
    fn rendered_toml_round_trips_through_the_dropin_parser() {
        let profiles = extract_profiles(ACME).unwrap();
        let (preset, extras) = preset_from_kwargs(&profiles[0]).unwrap();
        let body = render_preset_toml(&preset, &extras);
        // Extras appear as trailing comment lines.
        assert!(body
            .trim_end()
            .ends_with(r#"# hermes: default_aux_model = "acme-small-fast" (not used by newt)"#));
        let parsed = parse_preset_file("acme-inference", "toml", &body).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0], preset, "field-for-field round trip");
    }

    #[test]
    fn rendered_toml_with_headers_round_trips() {
        let kwargs = vec![
            ("name".to_string(), PyLiteral::Str("headed".to_string())),
            (
                "base_url".to_string(),
                PyLiteral::Str("https://headed.example.com/v1".to_string()),
            ),
            (
                "default_headers".to_string(),
                PyLiteral::Map(vec![(
                    "X-Title".to_string(),
                    PyLiteral::Str("newt".to_string()),
                )]),
            ),
        ];
        let (preset, extras) = preset_from_kwargs(&kwargs).unwrap();
        assert!(extras.is_empty());
        let body = render_preset_toml(&preset, &extras);
        let parsed = parse_preset_file("headed", "toml", &body).unwrap();
        assert_eq!(parsed[0], preset);
    }
}
