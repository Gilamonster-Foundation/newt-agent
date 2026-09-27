//! Safe-subset command-line parsing for the confined shell engine (ADR 0005 D3).
//!
//! `agent-bridle` is the **exec funnel**: rather than hand a string to a shell
//! interpreter, the engine parses it itself and runs only what it can confine.
//! This covers the safe-subset engine — a sequence of pipelines joined by `&&`,
//! `||` and `;`, each pipeline simple commands (`a | b | c`) with quoted
//! arguments, file redirections (`> out`, `>> out`, `< in`, `2> err`, `2>&1`),
//! filename globbing (`*`, `?`, `[…]`), and **variable expansion** (`$VAR` /
//! `${VAR}`, including mixed words like `$HOME/config` and references inside
//! double quotes). The parser only *marks* an argument ([`Arg::Glob`] /
//! [`Arg::Var`] segments); the filesystem read (globbing) and the env read +
//! allowlist check (variables) are the executor's job.
//!
//! Refused **by design** (security, [`Refusal::Dynamic`]): command substitution
//! `$(…)`, arithmetic expansion `$((…))`, backticks, and subshells — the
//! undecidable interiors ADR 0001 says may never be statically cleared. Refused for now
//! ([`Refusal::Unsupported`]): a glob in a redirect target, and fd redirections
//! other than `1>`/`2>`/`0<`/`2>&1` (e.g. `3>`, `1>&2`). Variable-bearing
//! redirect targets (`> $TMPDIR/out`) and glob+variable words (`$DIR/*.rs`) ARE
//! supported — expanded (allowlisted) + leash-checked at admission (#46).
//!
//! Quoting is honored, so a metacharacter **inside quotes is a literal
//! argument** — only *unquoted* operators and constructs are recognized:
//! `echo "a*b"` is one literal arg, `echo a*` is a glob, `echo $HOME` is a
//! variable, and `a && b` is two pipelines joined by `&&`.

use std::fmt;
use std::iter::Peekable;
use std::mem::take;
use std::str::Chars;

/// Why the confined engine refused to run a `cmd` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Refused **by design** (security): the construct's interior is dynamic and
    /// cannot be statically confined, so the engine never interprets it
    /// (command substitution, arithmetic expansion, backticks, subshells). For
    /// a full shell, use the embedder's unbridled/`--yolo` allowance (ADR 0003 /
    /// 0005 D5).
    Dynamic(&'static str),
    /// A construct the safe-subset engine will support but **does not yet** in
    /// this increment (mixed/quoted variable expansion, fd-number redirections).
    /// Tracked on agent-bridle#34.
    Unsupported(&'static str),
    /// The input could not be parsed (unterminated quote, trailing backslash,
    /// empty command/stage, missing redirection target, dangling separator).
    Malformed(String),
}

impl Refusal {
    /// A short label for the offending construct (the envelope `target`).
    #[must_use]
    pub fn construct(&self) -> String {
        match self {
            Self::Dynamic(c) | Self::Unsupported(c) => (*c).to_string(),
            Self::Malformed(_) => "malformed input".to_string(),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dynamic(c) => write!(
                f,
                "refused by design: {c} is a dynamic construct the confined shell does not \
                 interpret (use the embedder's unbridled/--yolo path for a full shell)"
            ),
            Self::Unsupported(c) => write!(
                f,
                "not yet supported by the confined shell engine: {c} (tracked on agent-bridle#34)"
            ),
            Self::Malformed(why) => write!(f, "malformed command: {why}"),
        }
    }
}

/// A piece of a variable-bearing word: a literal run or a `$NAME` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    /// A literal run of characters.
    Lit(String),
    /// A `$NAME` / `${NAME}` reference (the variable name only).
    Var(String),
}

/// One argument word: a literal, a glob pattern, or a word that contains one or
/// more `$VAR` references (possibly mixed with literals, e.g. `$HOME/config`).
/// The executor lowers globs (a filesystem read) and variables (an env read +
/// allowlist check) into literals — both leash-/policy-checked first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    /// A literal word (quoting already resolved).
    Lit(String),
    /// A word containing unquoted `*`/`?`/`[…]` — a glob pattern.
    Glob(String),
    /// A word with `$VAR` reference(s) (segments concatenated at expansion time).
    Var(Vec<Seg>),
    /// A word that is **both** variable-bearing **and** a glob (e.g. `$DIR/*.rs`).
    /// Admission expands the `$VAR` (allowlisted) and lowers it to a [`Arg::Glob`]
    /// over the resolved string — but only when every variable stays in the
    /// directory *prefix* (a variable in the glob basename is refused, so a var
    /// value can never inject a glob metachar; re-injection guard).
    VarGlob(Vec<Seg>),
}

/// A redirection on one command stage. The target is a [`Seg`] list so it may
/// contain `$VAR` (e.g. `> $TMPDIR/out`); `agent-bridle` expands it (allowlisted,
/// via the env seam) and performs the open itself, so the **resolved** file
/// target is leash-checked (`fs_write` for stdout/stderr, `fs_read` for stdin)
/// before the stage runs. After admission lowers the `$VAR`, the path is a single
/// [`Seg::Lit`] — [`Command`]'s accessors surface that resolved literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redirect {
    /// `> path` / `>> path` (also `1>`/`1>>`) — stdout (fd 1).
    Stdout { path: Vec<Seg>, append: bool },
    /// `< path` (also `0<`) — stdin (fd 0).
    Stdin { path: Vec<Seg> },
    /// `2> path` / `2>> path` — stderr (fd 2).
    Stderr { path: Vec<Seg>, append: bool },
    /// `2>&1` — stderr is merged into stdout's destination.
    StderrToStdout,
}

/// The resolved literal path of a redirect **after admission lowering** (a single
/// [`Seg::Lit`]). `None` for an unexpanded multi-segment path — which the spawner
/// never sees, because lowering resolves every redirect target first.
#[must_use]
pub(crate) fn seg_literal(segs: &[Seg]) -> Option<&str> {
    match segs {
        [Seg::Lit(s)] => Some(s.as_str()),
        _ => None,
    }
}

/// Where a stage's stderr goes (resolved from its redirects; last one wins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StderrTo {
    /// No stderr redirect — capture it separately.
    Capture,
    /// `2> path` / `2>> path`.
    File { path: String, append: bool },
    /// `2>&1` — merge into the stdout destination.
    Stdout,
}

/// One command stage: its argv (literals/globs/variables) plus any redirections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The program (argv[0]) and arguments.
    pub argv: Vec<Arg>,
    /// File redirections, in source order (last redirect for a given fd wins).
    pub redirects: Vec<Redirect>,
}

impl Command {
    /// The effective stdin redirect path, if any (last one wins). The path is the
    /// resolved literal (post-lowering); an unexpanded `$VAR` redirect yields
    /// `None` here and is never reached by the spawner.
    #[must_use]
    pub fn stdin_path(&self) -> Option<&str> {
        self.redirects.iter().rev().find_map(|r| match r {
            Redirect::Stdin { path } => seg_literal(path),
            _ => None,
        })
    }

    /// The effective stdout redirect (resolved path, append), if any (last wins).
    #[must_use]
    pub fn stdout_redirect(&self) -> Option<(&str, bool)> {
        self.redirects.iter().rev().find_map(|r| match r {
            Redirect::Stdout { path, append } => seg_literal(path).map(|p| (p, *append)),
            _ => None,
        })
    }

    /// The effective stderr disposition (last stderr-affecting redirect wins).
    #[must_use]
    pub fn stderr_disposition(&self) -> StderrTo {
        self.redirects
            .iter()
            .rev()
            .find_map(|r| match r {
                Redirect::Stderr { path, append } => seg_literal(path).map(|p| StderrTo::File {
                    path: p.to_string(),
                    append: *append,
                }),
                Redirect::StderrToStdout => Some(StderrTo::Stdout),
                _ => None,
            })
            .unwrap_or(StderrTo::Capture)
    }
}

/// A pipeline: one or more `|`-joined command stages.
pub type Pipeline = Vec<Command>;

/// How a pipeline is gated by the *previous* pipeline's exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sep {
    /// `;` — run unconditionally. (The first pipeline always carries `Seq`.)
    Seq,
    /// `&&` — run only if the previous status was success (0).
    And,
    /// `||` — run only if the previous status was failure (non-0).
    Or,
}

/// One step of a [`Script`]: a pipeline plus the separator that gates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptItem {
    /// The separator preceding this pipeline (the first item's is [`Sep::Seq`]).
    pub sep: Sep,
    /// The pipeline to (conditionally) run.
    pub pipeline: Pipeline,
}

/// A parsed command line: a sequence of pipelines joined by `&&`/`||`/`;`.
pub type Script = Vec<ScriptItem>;

/// Parse a `cmd` string into a [`Script`], or a [`Refusal`].
pub fn classify(input: &str) -> Result<Script, Refusal> {
    let mut chars = input.chars().peekable();
    let mut script: Script = Vec::new();
    let mut pending_sep = Sep::Seq; // separator for the NEXT finalized pipeline
    let mut stages: Vec<Command> = Vec::new(); // current pipeline's stages
    let mut argv: Vec<Arg> = Vec::new(); // current stage's args
    let mut redirects: Vec<Redirect> = Vec::new(); // current stage's redirects

    loop {
        skip_whitespace(&mut chars);
        let Some(&c) = chars.peek() else { break };
        match c {
            '|' => {
                chars.next();
                if chars.peek() == Some(&'|') {
                    chars.next();
                    push_pipeline(
                        &mut script,
                        &mut stages,
                        &mut argv,
                        &mut redirects,
                        &mut pending_sep,
                        Sep::Or,
                    )?;
                } else {
                    if argv.is_empty() {
                        return Err(Refusal::Malformed(
                            "empty pipeline stage (nothing before `|`)".into(),
                        ));
                    }
                    stages.push(Command {
                        argv: take(&mut argv),
                        redirects: take(&mut redirects),
                    });
                }
            }
            '&' => {
                chars.next();
                if chars.peek() == Some(&'&') {
                    chars.next();
                    push_pipeline(
                        &mut script,
                        &mut stages,
                        &mut argv,
                        &mut redirects,
                        &mut pending_sep,
                        Sep::And,
                    )?;
                } else {
                    return Err(Refusal::Unsupported("background `&`"));
                }
            }
            ';' => {
                chars.next();
                push_pipeline(
                    &mut script,
                    &mut stages,
                    &mut argv,
                    &mut redirects,
                    &mut pending_sep,
                    Sep::Seq,
                )?;
            }
            '(' | ')' => return Err(Refusal::Dynamic("subshell `( )`")),
            '`' => return Err(Refusal::Dynamic("command substitution (backticks)")),
            '>' => {
                chars.next();
                let append = chars.peek() == Some(&'>');
                if append {
                    chars.next();
                }
                if chars.peek() == Some(&'&') {
                    return Err(Refusal::Unsupported("`>&` (fd duplication)"));
                }
                let path = read_redirect_target(&mut chars)?;
                redirects.push(Redirect::Stdout { path, append });
            }
            '<' => {
                chars.next();
                if chars.peek() == Some(&'<') {
                    return Err(Refusal::Unsupported("heredoc/herestring `<<`"));
                }
                if chars.peek() == Some(&'&') {
                    return Err(Refusal::Unsupported("`<&` (fd duplication)"));
                }
                let path = read_redirect_target(&mut chars)?;
                redirects.push(Redirect::Stdin { path });
            }
            // Words (literals, globs, `$VAR`) are read by `read_word`. A bare
            // number touching a redirect op (`2>`, `0<`, …) is an fd redirect.
            _ => {
                let arg = read_word(&mut chars)?;
                if let Arg::Lit(s) = &arg {
                    if !s.is_empty()
                        && s.bytes().all(|b| b.is_ascii_digit())
                        && matches!(chars.peek(), Some('>') | Some('<'))
                    {
                        read_fd_redirect(s, &mut chars, &mut redirects)?;
                        continue;
                    }
                }
                argv.push(arg);
            }
        }
    }

    // Finalize the trailing pipeline (no following separator).
    match finalize_pipeline(&mut stages, &mut argv, &mut redirects)? {
        Some(pipeline) => script.push(ScriptItem {
            sep: pending_sep,
            pipeline,
        }),
        None => {
            if script.is_empty() {
                return Err(Refusal::Malformed("empty command".into()));
            }
            if !matches!(pending_sep, Sep::Seq) {
                return Err(Refusal::Malformed(
                    "expected a command after `&&` or `||`".into(),
                ));
            }
        }
    }
    Ok(script)
}

/// Finalize the current stage + pipeline and push it to the script under
/// `pending_sep`, then arm `pending_sep` for the next pipeline.
fn push_pipeline(
    script: &mut Script,
    stages: &mut Vec<Command>,
    argv: &mut Vec<Arg>,
    redirects: &mut Vec<Redirect>,
    pending_sep: &mut Sep,
    next_sep: Sep,
) -> Result<(), Refusal> {
    match finalize_pipeline(stages, argv, redirects)? {
        Some(pipeline) => {
            script.push(ScriptItem {
                sep: *pending_sep,
                pipeline,
            });
            *pending_sep = next_sep;
            Ok(())
        }
        None => Err(Refusal::Malformed(
            "empty command around a separator (`&&`/`||`/`;`)".into(),
        )),
    }
}

/// Finalize the current stage into the current pipeline and return the pipeline.
/// `Ok(None)` means the pipeline was entirely empty.
fn finalize_pipeline(
    stages: &mut Vec<Command>,
    argv: &mut Vec<Arg>,
    redirects: &mut Vec<Redirect>,
) -> Result<Option<Pipeline>, Refusal> {
    if argv.is_empty() {
        if !stages.is_empty() {
            return Err(Refusal::Malformed(
                "empty pipeline stage (nothing after `|`)".into(),
            ));
        }
        if !redirects.is_empty() {
            return Err(Refusal::Malformed("redirection without a command".into()));
        }
        return Ok(None);
    }
    stages.push(Command {
        argv: take(argv),
        redirects: take(redirects),
    });
    Ok(Some(take(stages)))
}

/// Consume any run of unquoted whitespace.
fn skip_whitespace(chars: &mut Peekable<Chars>) {
    while matches!(chars.peek(), Some(' ' | '\t' | '\n' | '\r')) {
        chars.next();
    }
}

/// Read the next word as an [`Arg`]. Stops at unquoted whitespace or an operator.
/// Single quotes are literal; double quotes are literal except `$`/backtick still
/// expand; an unquoted backslash escapes the next char. An unquoted `*`/`?`/`[`
/// marks the word a glob; a `$NAME`/`${NAME}` (unquoted or inside double quotes)
/// contributes a [`Seg::Var`] so a word can mix literals and variables
/// (`$HOME/config`). A word that is BOTH a glob and variable-bearing becomes an
/// [`Arg::VarGlob`] (`$DIR/*.rs`), expanded + re-injection-guarded at admission.
fn read_word(chars: &mut Peekable<Chars>) -> Result<Arg, Refusal> {
    let mut cur = String::new(); // current literal run
    let mut segs: Vec<Seg> = Vec::new(); // pieces flushed when a `$VAR` appears
    let mut is_glob = false;

    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\n' | '\r' => break,
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '`' => break,
            '$' => {
                chars.next(); // consume '$'
                let name = read_var_name(chars)?;
                if !cur.is_empty() {
                    segs.push(Seg::Lit(std::mem::take(&mut cur)));
                }
                segs.push(Seg::Var(name));
            }
            '*' | '?' | '[' => {
                cur.push(c);
                chars.next();
                is_glob = true;
            }
            '\'' => {
                chars.next();
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err(Refusal::Malformed("unterminated single quote".into())),
                    }
                }
            }
            '"' => {
                chars.next();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.peek() {
                            Some(&n) if matches!(n, '"' | '\\' | '$' | '`') => {
                                cur.push(n);
                                chars.next();
                            }
                            _ => cur.push('\\'),
                        },
                        // `$VAR` inside double quotes expands (a literal `$` is `\$`).
                        Some('$') => {
                            let name = read_var_name(chars)?;
                            if !cur.is_empty() {
                                segs.push(Seg::Lit(std::mem::take(&mut cur)));
                            }
                            segs.push(Seg::Var(name));
                        }
                        Some('`') => {
                            return Err(Refusal::Dynamic("command substitution (backticks)"));
                        }
                        Some(ch) => cur.push(ch),
                        None => return Err(Refusal::Malformed("unterminated double quote".into())),
                    }
                }
            }
            '\\' => {
                chars.next();
                match chars.next() {
                    Some(n) => cur.push(n),
                    None => return Err(Refusal::Malformed("trailing backslash".into())),
                }
            }
            _ => {
                cur.push(c);
                chars.next();
            }
        }
    }

    if segs.is_empty() {
        // Pure literal/glob word. (A bare digit-word touching a redirect op is
        // handled by the caller as an fd redirect.)
        return Ok(if is_glob {
            Arg::Glob(cur)
        } else {
            Arg::Lit(cur)
        });
    }
    // A variable-bearing word. A trailing literal run (which may carry the glob
    // metachars, e.g. the `/*.rs` in `$DIR/*.rs`) is the last segment.
    if !cur.is_empty() {
        segs.push(Seg::Lit(cur));
    }
    if is_glob {
        Ok(Arg::VarGlob(segs))
    } else {
        Ok(Arg::Var(segs))
    }
}

/// Read a variable name after a consumed `$`: `NAME` (bare) or `{NAME}`.
/// Command substitution `$(` and arithmetic expansion `$((` are dynamic and
/// refused with distinct construct labels; a `$` not followed by a name/brace
/// is a refused bare `$` (escape as `\$` for a literal dollar).
fn read_var_name(chars: &mut Peekable<Chars>) -> Result<String, Refusal> {
    let name = match chars.peek() {
        Some('(') => {
            let mut lookahead = chars.clone();
            lookahead.next();
            let construct = if lookahead.peek() == Some(&'(') {
                "arithmetic expansion `$((`"
            } else {
                "command substitution `$(`"
            };
            return Err(Refusal::Dynamic(construct));
        }
        Some('{') => {
            chars.next(); // consume '{'
            let mut name = String::new();
            loop {
                match chars.next() {
                    Some('}') => break,
                    Some(ch) => name.push(ch),
                    None => return Err(Refusal::Malformed("unterminated `${`".into())),
                }
            }
            name
        }
        Some(&c) if c == '_' || c.is_ascii_alphabetic() => {
            let mut name = String::new();
            while let Some(&ch) = chars.peek() {
                if ch == '_' || ch.is_ascii_alphanumeric() {
                    name.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            name
        }
        _ => {
            return Err(Refusal::Unsupported(
                "bare `$` (escape as `\\$` for a literal dollar)",
            ));
        }
    };
    if !is_valid_var_name(&name) {
        return Err(Refusal::Unsupported("unsupported variable name"));
    }
    Ok(name)
}

/// A valid environment variable name: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_valid_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Read a redirection target (the word after `>`/`>>`/`<`) as a [`Seg`] list:
/// a plain literal, or a word with `$VAR` (e.g. `$TMPDIR/out`), expanded
/// (allowlisted) and leash-checked at admission. A **glob** in a redirect target
/// is still refused (an ambiguous, multi-match target).
fn read_redirect_target(chars: &mut Peekable<Chars>) -> Result<Vec<Seg>, Refusal> {
    skip_whitespace(chars);
    match chars.peek().copied() {
        None | Some('|' | '&' | ';' | '<' | '>' | '(' | ')') => {
            return Err(Refusal::Malformed("missing redirection target".into()));
        }
        _ => {}
    }
    match read_word(chars)? {
        Arg::Lit(s) if !s.is_empty() => Ok(vec![Seg::Lit(s)]),
        Arg::Lit(_) => Err(Refusal::Malformed("empty redirection target".into())),
        Arg::Glob(_) | Arg::VarGlob(_) => Err(Refusal::Unsupported("glob in a redirection target")),
        Arg::Var(segs) => Ok(segs),
    }
}

/// Parse an fd-prefixed redirect, given the already-read fd digits and the
/// upcoming `>`/`<`. Supported: `1>`/`1>>`/`0<` (aliases of the bare forms),
/// `2>`/`2>>` (stderr to file), and `2>&1` (merge stderr into stdout). Other fds
/// and dup forms (e.g. `1>&2`, `3>`) are refused.
fn read_fd_redirect(
    fd: &str,
    chars: &mut Peekable<Chars>,
    redirects: &mut Vec<Redirect>,
) -> Result<(), Refusal> {
    // Input redirect: only `0<` is supported.
    if chars.peek() == Some(&'<') {
        chars.next();
        if fd != "0" {
            return Err(Refusal::Unsupported("fd-number input redirection"));
        }
        if chars.peek() == Some(&'<') {
            return Err(Refusal::Unsupported("heredoc/herestring `<<`"));
        }
        if chars.peek() == Some(&'&') {
            return Err(Refusal::Unsupported("`<&` (fd duplication)"));
        }
        redirects.push(Redirect::Stdin {
            path: read_redirect_target(chars)?,
        });
        return Ok(());
    }

    // Output redirect (`>` / `>>`).
    chars.next(); // consume the first '>'
    let append = chars.peek() == Some(&'>');
    if append {
        chars.next();
    }

    // Dup form: `N>&M` (only `2>&1` is supported).
    if chars.peek() == Some(&'&') {
        chars.next(); // consume '&'
        let mut target = String::new();
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                target.push(d);
                chars.next();
            } else {
                break;
            }
        }
        if fd == "2" && target == "1" {
            redirects.push(Redirect::StderrToStdout);
            return Ok(());
        }
        return Err(Refusal::Unsupported(
            "fd duplication (only `2>&1` is supported)",
        ));
    }

    let path = read_redirect_target(chars)?;
    match fd {
        "1" => redirects.push(Redirect::Stdout { path, append }),
        "2" => redirects.push(Redirect::Stderr { path, append }),
        _ => {
            return Err(Refusal::Unsupported(
                "fd-number redirection (only 1>, 2>, 0<, 2>&1)",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> Arg {
        Arg::Lit(s.to_string())
    }
    fn glob(s: &str) -> Arg {
        Arg::Glob(s.to_string())
    }
    /// A single-variable word, e.g. `$HOME`.
    fn var(name: &str) -> Arg {
        Arg::Var(vec![Seg::Var(name.to_string())])
    }
    fn slit(s: &str) -> Seg {
        Seg::Lit(s.to_string())
    }
    fn svar(s: &str) -> Seg {
        Seg::Var(s.to_string())
    }
    fn stage(args: Vec<Arg>) -> Command {
        Command {
            argv: args,
            redirects: vec![],
        }
    }
    fn one_stage(args: Vec<Arg>) -> Script {
        vec![ScriptItem {
            sep: Sep::Seq,
            pipeline: vec![stage(args)],
        }]
    }

    // ── carried behavior (1–5) ──────────────────────────────────────────────

    #[test]
    fn literals_globs_pipelines_redirects_sequencing() {
        assert_eq!(
            classify("echo hi").unwrap(),
            one_stage(vec![lit("echo"), lit("hi")])
        );
        assert_eq!(
            classify("ls *.rs").unwrap(),
            one_stage(vec![lit("ls"), glob("*.rs")])
        );
        assert_eq!(
            classify("echo \"a*b\"").unwrap(),
            one_stage(vec![lit("echo"), lit("a*b")])
        );
        assert!(classify("a && b").is_ok());
        let p = classify("echo hi > out").unwrap();
        assert_eq!(p[0].pipeline[0].stdout_redirect(), Some(("out", false)));
    }

    // ── variable expansion (increment 6) ────────────────────────────────────

    #[test]
    fn standalone_variables_are_marked() {
        assert_eq!(
            classify("echo $HOME").unwrap(),
            one_stage(vec![lit("echo"), var("HOME")])
        );
        assert_eq!(
            classify("echo ${HOME}").unwrap(),
            one_stage(vec![lit("echo"), var("HOME")])
        );
        assert_eq!(
            classify("ls $TMPDIR").unwrap(),
            one_stage(vec![lit("ls"), var("TMPDIR")])
        );
        // A variable followed by an operator is still a whole word (here `|`,
        // so this is a single pipeline with two stages).
        assert_eq!(
            classify("cat $FILE | wc").unwrap(),
            vec![ScriptItem {
                sep: Sep::Seq,
                pipeline: vec![stage(vec![lit("cat"), var("FILE")]), stage(vec![lit("wc")])],
            }]
        );
    }

    #[test]
    fn mixed_and_quoted_variables_are_segments() {
        // `$HOME/x` → variable + literal segments.
        assert_eq!(
            classify("echo $HOME/x").unwrap(),
            one_stage(vec![lit("echo"), Arg::Var(vec![svar("HOME"), slit("/x")])])
        );
        // `foo$HOME` → literal + variable.
        assert_eq!(
            classify("echo foo$HOME").unwrap(),
            one_stage(vec![lit("echo"), Arg::Var(vec![slit("foo"), svar("HOME")])])
        );
        // Inside double quotes the variable still expands.
        assert_eq!(
            classify("echo \"$HOME/x\"").unwrap(),
            one_stage(vec![lit("echo"), Arg::Var(vec![svar("HOME"), slit("/x")])])
        );
        // Multiple segments: literal, var, literal.
        assert_eq!(
            classify("echo pre${X}.log").unwrap(),
            one_stage(vec![
                lit("echo"),
                Arg::Var(vec![slit("pre"), svar("X"), slit(".log")]),
            ])
        );
    }

    #[test]
    fn glob_and_variable_in_one_word_parses_as_varglob() {
        // `$DIR/*.rs` is now a VarGlob (was refused); admission expands the var
        // and lowers it to a resolved glob, with the basename re-injection guard.
        let p = classify("ls $DIR/*.rs").unwrap();
        assert_eq!(
            p[0].pipeline[0].argv,
            vec![lit("ls"), Arg::VarGlob(vec![svar("DIR"), slit("/*.rs")])]
        );
    }

    #[test]
    fn invalid_variable_forms_are_refused() {
        // Positional/invalid variable form.
        assert!(matches!(classify("echo $1"), Err(Refusal::Unsupported(_))));
        // Bare dollar.
        assert!(matches!(classify("echo $"), Err(Refusal::Unsupported(_))));
        // Escaped `$` is a literal dollar, not a variable.
        assert_eq!(
            classify("echo \\$HOME").unwrap(),
            one_stage(vec![lit("echo"), lit("$HOME")])
        );
    }

    #[test]
    fn command_substitution_and_arithmetic_expansion_have_distinct_refusals() {
        for source in ["echo $(id)", r#"echo "$(id)""#] {
            let refusal = classify(source).expect_err("command substitution must be refused");
            assert_eq!(
                refusal,
                Refusal::Dynamic("command substitution `$(`"),
                "{source}"
            );
            assert_eq!(refusal.construct(), "command substitution `$(`");
        }

        for source in ["echo $((1 + 2))", r#"echo "$((1 + 2))""#] {
            let refusal = classify(source).expect_err("arithmetic expansion must be refused");
            assert_eq!(
                refusal,
                Refusal::Dynamic("arithmetic expansion `$((`"),
                "{source}"
            );
            assert_eq!(refusal.construct(), "arithmetic expansion `$((`");
        }
    }

    #[test]
    fn variables_parse_in_redirect_targets() {
        // $VAR in a redirect target now PARSES into segments (expanded +
        // leash-checked at admission, #46) rather than being refused.
        let p = classify("echo hi > $HOME/out").unwrap();
        assert_eq!(
            p[0].pipeline[0].redirects,
            vec![Redirect::Stdout {
                path: vec![svar("HOME"), slit("/out")],
                append: false,
            }]
        );
        // A glob in a redirect target is still refused (ambiguous target).
        assert!(matches!(
            classify("echo > *.log"),
            Err(Refusal::Unsupported(_))
        ));
    }

    // ── fd redirects (issue #45) ────────────────────────────────────────────

    #[test]
    fn stderr_to_file_and_append() {
        let p = classify("cmd 2> err").unwrap();
        assert_eq!(
            p[0].pipeline[0].stderr_disposition(),
            StderrTo::File {
                path: "err".into(),
                append: false
            }
        );
        let p = classify("cmd 2>> err").unwrap();
        assert_eq!(
            p[0].pipeline[0].stderr_disposition(),
            StderrTo::File {
                path: "err".into(),
                append: true
            }
        );
    }

    #[test]
    fn merge_and_fd_aliases() {
        // `2>&1` → merge into stdout.
        assert_eq!(
            classify("cmd 2>&1").unwrap()[0].pipeline[0].stderr_disposition(),
            StderrTo::Stdout
        );
        // `1>`/`0<` are aliases of the bare forms.
        let p = classify("cmd 1> out").unwrap();
        assert_eq!(p[0].pipeline[0].stdout_redirect(), Some(("out", false)));
        let p = classify("cmd 0< in").unwrap();
        assert_eq!(p[0].pipeline[0].stdin_path(), Some("in"));
        // `>file 2>&1` — both go to the file destination (common form).
        let p = classify("cmd > out 2>&1").unwrap();
        assert_eq!(p[0].pipeline[0].stdout_redirect(), Some(("out", false)));
        assert_eq!(p[0].pipeline[0].stderr_disposition(), StderrTo::Stdout);
        // A digit that is NOT touching a redirect op is a normal argument.
        assert_eq!(
            classify("head -2").unwrap(),
            one_stage(vec![lit("head"), lit("-2")])
        );
    }

    #[test]
    fn unsupported_fd_forms_refused() {
        assert!(matches!(classify("cmd 3> f"), Err(Refusal::Unsupported(_)))); // fd ≥ 3
        assert!(matches!(classify("cmd 1>&2"), Err(Refusal::Unsupported(_)))); // other dup
        assert!(matches!(classify("cmd 2<&1"), Err(Refusal::Unsupported(_)))); // fd input
    }

    // ── other refusals still hold ───────────────────────────────────────────

    #[test]
    fn dynamic_and_malformed_still_refused() {
        assert!(matches!(classify("echo `id`"), Err(Refusal::Dynamic(_))));
        assert!(matches!(classify("(echo hi)"), Err(Refusal::Dynamic(_))));
        assert!(matches!(classify(""), Err(Refusal::Malformed(_))));
        assert!(matches!(classify("a &&"), Err(Refusal::Malformed(_))));
    }
}
