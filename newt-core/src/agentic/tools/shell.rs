//! Shell execution for run_command and lifecycle: environment, confinement,
//! host-process lifetime, and shell-envelope interpretation.

use super::super::content_spill::{self, SpillStore};
use super::super::display::ToolPresentation;
use super::super::permissions::{
    DenialKind, PermissionDecision, PermissionGate, PermissionRequest,
};
use super::live_output::{LiveOutputRelay, LiveOutputSession};
use super::output_budget::{
    self, cap_model_output, cap_model_output_with_handle, max_output_tokens, output_head_tokens,
};
use super::{denial_recovery_hint, full_access_requested, ocap_disabled};
use crate::ExecOutcome;

pub fn venv_cmd_prefix() -> Option<String> {
    let venv = std::env::var("NEWT_VENV")
        .or_else(|_| std::env::var("VIRTUAL_ENV"))
        .ok();
    let exec_paths = std::env::var("NEWT_EXEC_PATHS").ok();

    if venv.is_none() && exec_paths.is_none() {
        return None;
    }

    // sh single-quoting: wrap in '', escape any ' as '\''
    let q = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));

    // Build a list of dirs to prepend to PATH (venv/bin first, then exec-paths).
    let mut path_dirs: Vec<String> = Vec::new();
    let mut prefix = String::new();

    if let Some(ref venv) = venv {
        let venv_bin = format!("{venv}/bin");
        prefix.push_str(&format!("export VIRTUAL_ENV={}; ", q(venv)));
        path_dirs.push(venv_bin);
    }
    if let Some(ref paths) = exec_paths {
        for dir in paths.split(':') {
            if !dir.is_empty() {
                path_dirs.push(dir.to_string());
            }
        }
    }

    if !path_dirs.is_empty() {
        let quoted: Vec<String> = path_dirs.iter().map(|d| q(d)).collect();
        prefix.push_str(&format!("export PATH={}:\"$PATH\"; ", quoted.join(":")));
    }

    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// Build the venv/exec-path environment as a `{KEY:VALUE}` map for the confined
/// shell's structured `env` seam (agent-bridle, newt #783).
///
/// Same inputs as [`venv_cmd_prefix`] — `NEWT_VENV` (preferred) or
/// `$VIRTUAL_ENV`, plus `NEWT_EXEC_PATHS` — but delivered as host-supplied env
/// vars set directly on the spawned child instead of `export …;` text prepended
/// to the command. The `export` form is the #783 root cause: `export` is a
/// shell builtin, not a program, so the confined safe-subset engine refuses it
/// on a compound command (`a; b | c`). Passing the vars through the env seam
/// sidesteps that entirely and never touches the command text.
///
/// `PATH` is the venv `bin` (then any `NEWT_EXEC_PATHS` dirs) *prepended* to the
/// inherited host `PATH`: the env seam sets the value additively over the
/// child's ambient environment, so we read the host `PATH` here and build the
/// full string rather than relying on a `$PATH` expansion inside the value.
/// Returns an empty map when neither input is set (no env key is sent).
pub(super) fn venv_env_map() -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();

    // Env passthrough: the confined shell has NO ambient shell variables, so
    // without this brush cannot expand `~` (it resolves `~` from its `HOME` shell
    // var, erroring "HOME not set") and the command silently used a literal
    // `~/…` path — leaving `<cwd>/~/…` debris on disk. Seed a minimal,
    // operator-configurable allow-list from the process env (default HOME+USER+TZ;
    // widened via `[shell] env_passthrough`, published as
    // NEWT_SHELL_ENV_PASSTHROUGH). Each var is set only when present, so nothing
    // is fabricated and the default stays narrow (the confined shell is a trust
    // boundary — a wide passthrough would leak secrets into a sandboxed command).
    for var in shell_env_passthrough() {
        if let Ok(val) = std::env::var(&var) {
            map.insert(var, val);
        }
    }
    // File-sourced import (#1243 Leg 2): the `~/.newt/shell-env/` drop-in dir —
    // deliberate, allowlisted tokens/support vars whose VALUES live in files,
    // never in config.toml or newt's own process env. Merged over the ambient
    // passthrough (explicit operator intent wins); the engine-critical vars
    // (SHELL/VIRTUAL_ENV/PATH) set below still win over a same-named token file.
    if let Some(config_path) = crate::Config::user_config_path() {
        map.extend(crate::shell_env::from_config_dir(&config_path));
    }
    // The child's temp dir, matching the write fence's scratch root. The brush
    // engine runs `do_not_inherit_env(true)`, so the ambient `TMPDIR` is not
    // inherited: this seam entry is what the child sees. Without it the child's
    // tools fall back to the platform temp dir, which a configured fence may not
    // grant. `NEWT_CHILD_TMPDIR` is published by the headless confined
    // lane from the SAME value the fence is built from (one owner, no second
    // resolution); unset everywhere else, so other lanes are unchanged.
    if let Ok(tmp) = std::env::var("NEWT_CHILD_TMPDIR") {
        if !tmp.trim().is_empty() {
            map.insert("TMPDIR".to_string(), tmp);
        }
    }
    // Model-run git: no ambient config, the hardening overrides, and the
    // agent as author (see `git_hardening::sandbox_git_env`).
    // Resolved per dispatch, so a `/settings` change applies to the next
    // command rather than the next session.
    let identity = crate::AgentIdentity::resolve().unwrap_or_default();
    let (name, email) = identity.sandbox_author();
    map.extend(crate::git_hardening::sandbox_git_env(
        (&name, &email),
        &identity.git.config,
    ));

    // Identify the confined engine so `env` / scripts can tell they're in newt's
    // shell (e.g. `SHELL=safe-subset` / `brush` / `host`), not the login shell.
    map.insert("SHELL".to_string(), shell_engine().as_str().to_string());

    let venv = std::env::var("NEWT_VENV")
        .or_else(|_| std::env::var("VIRTUAL_ENV"))
        .ok();
    let exec_paths = std::env::var("NEWT_EXEC_PATHS").ok();

    // Dirs to prepend to PATH (venv/bin first, then any exec-paths), mirroring
    // venv_cmd_prefix's ordering.
    let mut path_dirs: Vec<String> = Vec::new();
    if let Some(ref venv) = venv {
        map.insert("VIRTUAL_ENV".to_string(), venv.clone());
        path_dirs.push(format!("{venv}/bin"));
    }
    if let Some(ref paths) = exec_paths {
        for dir in paths.split(':') {
            if !dir.is_empty() {
                path_dirs.push(dir.to_string());
            }
        }
    }

    // Resolve Apple's real developer tools before the /usr/bin xcrun shims.
    // The shims need ambient per-user caches; direct tools keep workspace-only
    // writes. Explicit venv and operator executable roots retain precedence.
    #[cfg(target_os = "macos")]
    if let Some(developer) = crate::confined_exec::selected_developer_directory() {
        path_dirs.push(format!("{developer}/usr/bin"));
    }

    if !path_dirs.is_empty() {
        let prepend = path_dirs.join(":");
        let path = match std::env::var("PATH") {
            Ok(inherited) if !inherited.is_empty() => format!("{prepend}:{inherited}"),
            _ => prepend,
        };
        map.insert("PATH".to_string(), path);
    }

    map
}

/// The confined-shell env passthrough list: `NEWT_SHELL_ENV_PASSTHROUGH`
/// (colon-separated, published from `[shell] env_passthrough`) or the minimal
/// default (`HOME`, `USER`, `TZ`). Empty names are dropped; empty values survive.
fn shell_env_passthrough() -> Vec<String> {
    match std::env::var("NEWT_SHELL_ENV_PASSTHROUGH") {
        Ok(s) if !s.trim().is_empty() => s
            .split(':')
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect(),
        _ => crate::config::shell_env_passthrough_default(),
    }
}

/// Build the dispatch args for agent-bridle's confined `shell` tool (#783): the
/// RAW user command (free-form `cmd` mode) plus the venv carried through the
/// structured `env` seam ([`venv_env_map`]). Deliberately NO `export …;` prefix
/// on `cmd` — that is what the confined safe-subset engine refuses on a
/// compound command (the #783 root cause); the env seam sets `VIRTUAL_ENV` /
/// `PATH` on the spawned child instead. The host-bypass (`--yolo`) path keeps
/// the prefix form because it runs on a real `/bin/sh` where `export` works.
/// Resolve an optional model-supplied `cwd` (#1159) against the workspace: a
/// relative dir joins under the workspace root; an absolute one is taken as-is
/// (the confined shell's fs fence rejects any path that escapes the workspace,
/// so this never widens reach). `None` runs at the workspace root, as before.
pub(super) fn resolve_exec_cwd(workspace: &str, cwd: Option<&str>) -> String {
    match cwd.map(str::trim).filter(|c| !c.is_empty()) {
        None => workspace.to_string(),
        Some(c) if std::path::Path::new(c).is_absolute() => c.to_string(),
        Some(c) => std::path::Path::new(workspace)
            .join(c)
            .to_string_lossy()
            .into_owned(),
    }
}

/// Split a leading `cd <path> &&` (or `cd <path> ;`) off a `run_command`
/// string so the `cd` — a shell **builtin**, not an executable — never reaches
/// the confined exec layer, which would try to `execvp("cd")` and fail (on
/// macOS: `sandbox-exec: execvp() of 'cd' failed`). The models reliably prefix
/// `cd <workspace> && <real command>` out of habit; folding the `cd` into the
/// command's cwd runs the real command where the model meant, and — as a bonus
/// — the OCAP prompt then names the *real* capability (`git checkout -b …`)
/// instead of the opaque `cd … && git …`.
///
/// Only a SINGLE leading `cd` at the very start is folded (not `cd a && cd b`,
/// and not a `cd` deeper in a pipeline — those are left for the shell engine).
/// Folding happens only when the path is followed by a sequential connective
/// (`&&` / `;`) or end-of-string; an unusual `cd x | y` is left whole.
/// Returns `(cd_path, remainder)`; `remainder` is empty for a bare `cd <path>`.
pub(super) fn split_leading_cd(cmd: &str) -> (Option<String>, String) {
    let rest = match cmd.trim_start().strip_prefix("cd") {
        Some(r) if r.starts_with(char::is_whitespace) => r.trim_start(),
        _ => return (None, cmd.to_string()),
    };
    let (path, after) = parse_cd_path(rest);
    if path.is_empty() {
        return (None, cmd.to_string());
    }
    let after = after.trim_start();
    let remainder = if let Some(r) = after.strip_prefix("&&") {
        r.trim_start().to_string()
    } else if let Some(r) = after.strip_prefix(';') {
        r.trim_start().to_string()
    } else if after.is_empty() {
        String::new()
    } else {
        // `cd <path>` followed by something we don't confidently understand
        // (a pipe, `||`, a redirection) — don't fold; hand the whole string to
        // the engine.
        return (None, cmd.to_string());
    };
    (Some(path), remainder)
}

/// Parse the first path token of a `cd` argument: a single/double-quoted string
/// (returned unquoted) or an unquoted run up to the next whitespace. Returns
/// `(path, remainder_after_the_token)`.
fn parse_cd_path(s: &str) -> (String, &str) {
    let first = s.chars().next();
    if let Some(q @ ('"' | '\'')) = first {
        if let Some(end) = s[1..].find(q) {
            return (s[1..=end].to_string(), &s[end + 2..]);
        }
    }
    match s.find(char::is_whitespace) {
        Some(i) => (s[..i].to_string(), &s[i..]),
        None => (s.to_string(), ""),
    }
}

/// #2558 (HANDOFF item 2): `cmd f > f` — the shell truncates `f` before the
/// command reads it, destroying the input. Run 6 of the refactor test lost a
/// file this way (`awk … f > f`). Per #2558 ("make the wrong mutation
/// impossible, don't explain it afterwards"), refuse BEFORE any exec: nothing
/// runs, nothing changes. Called once, at the top of [`exec_confined_command`]
/// — the ONE function both the confined lane and the `--yolo` host-bypass
/// lane route through (the branch between them happens INSIDE it), so a
/// single call site covers both **for model-typed shell text**: `run_command`,
/// the routed-build fallback (whose redirect would be `SHELL_META` and never
/// route in the first place), and the justfile-missing fallback. It does
/// NOT cover `run_confined_build_lane`'s `sh -c <joined>` lane
/// (`build_check_argv`, used by `lifecycle action=build` / the #2541
/// escalation): that runs RESOLVED phase commands from `[lifecycle]`/pack
/// config, not the model's own typed text, and is reachable only if the
/// model edits that config — accepted, not "every shell lane".
///
/// "Reads" = a plain operand of any command in the pipeline, or `< f` —
/// narrowed by two rules (#2560 round 2) so a harmless first instinct is not
/// refused: a [`NON_READING_COMMANDS`] stage (`echo`, `printf`, …) adds no
/// reads at all, and a [`SUBCOMMAND_DISPATCHERS`] stage's FIRST operand (the
/// verb — `build`, `diff`, `test`) is skipped, so `cargo build > build` and
/// `git diff > diff` run. "Writes" = the target of `>`, `>|`, or `>>`
/// (append is unsafe too, for a command that streams) — PLUS every non-flag
/// operand of a `tee` stage, which opens each with `O_TRUNC` at startup
/// regardless of any redirect (`sort f | tee f`). Paths are resolved against
/// `cwd` via [`resolve_exec_cwd`] — the SAME #2551 resolution
/// `run_command`/`lifecycle` already use, not a second one — then
/// lexically normalized (`crate::caveats::lexically_normalize`) so `cat ./f
/// > f` compares equal to `cat f > f`.
///
/// Deliberately NOT a shell parser: a single pipeline of plain tokens, with
/// minimal single/double-quote handling so `sed 's/a/b/' f > f` still reads
/// as `sed`, `f`, `>`, `f`. A token touched by an expansion/glob character
/// (`$` `` ` `` `*` `?` `[` `~`) or an unterminated quote is OPAQUE: it is
/// never treated as a read OR a write target, so an ambiguous form is left
/// exactly as today (never refused, never silently trusted as "different
/// file") rather than risk a false refusal. `&&` / `;` / `||` start a fresh
/// pipeline scope (each side of `f > f.tmp && mv f.tmp f` is checked on its
/// own, so that rewrite is never refused); `|` keeps the same scope (a later
/// stage's write can still collide with an earlier stage's read).
///
/// **Stated limits (#2560 round 2 review) — miss, but no worse than today:**
/// - **Subshells and brace groups**: `(cat f) > f` / `{ cat f; } > f` — `(cat`
///   becomes an opaque-free but wrong "command word", and the brace form
///   splits its own scope at `;`. Out of scope for a single-pipeline parser.
/// - **`dd if=f of=f`**: no redirect operator at all; `of=` truncates unless
///   `conv=notrunc`. Not special-cased — rare for this model.
/// - **Symlinks**: `cat link > f` where `link` points at `f` is lexical-only
///   and invisible here by design (the brief asked for no canonicalization;
///   canonicalizing would also require the path to already exist).
/// - **No general flag-skipping**: only `tee`'s operands and a subcommand
///   dispatcher's first operand are narrowed. A plain `-name`-style flag
///   elsewhere is still a "read" candidate (harmless unless it coincidentally
///   equals the write target, which the review judged not worth chasing).
pub(super) fn same_file_redirect_refusal(cmd: &str, cwd: &str) -> Option<String> {
    let tokens = tokenize(cmd);
    let mut start = 0usize;
    for (idx, tok) in tokens.iter().enumerate() {
        if matches!(tok, RedirectToken::Sep) {
            if let Some(msg) = check_pipeline_redirects(&tokens[start..idx], cmd, cwd) {
                return Some(msg);
            }
            start = idx + 1;
        }
    }
    check_pipeline_redirects(&tokens[start..], cmd, cwd)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedirectOp {
    In,
    Out,
    OutAppend,
    OutClobber,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RedirectToken {
    /// Dequoted text, and whether it touched an expansion/glob character or
    /// an unterminated quote (opaque — never a read or write candidate).
    Word(String, bool),
    Redirect(RedirectOp),
    /// `|` — a new stage of the SAME pipeline scope.
    Pipe,
    /// `&&` / `;` / `||` — starts a fresh pipeline scope.
    Sep,
}

/// Characters that mean "this word may not literally be the path it looks
/// like" — a glob, a variable, a command substitution, or a bare `~`. Also
/// applied to a word's DEQUOTED content, so a single-quoted `'*.rs'` (still
/// meant as a real glob) stays opaque too.
const AMBIGUOUS_CHARS: [char; 6] = ['$', '`', '*', '?', '[', '~'];

fn tokenize(cmd: &str) -> Vec<RedirectToken> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '&' && chars.get(i + 1) == Some(&'&') {
            out.push(RedirectToken::Sep);
            i += 2;
            continue;
        }
        if c == '|' && chars.get(i + 1) == Some(&'|') {
            out.push(RedirectToken::Sep);
            i += 2;
            continue;
        }
        if c == ';' {
            out.push(RedirectToken::Sep);
            i += 1;
            continue;
        }
        if c == '|' {
            out.push(RedirectToken::Pipe);
            i += 1;
            continue;
        }
        if c == '>' && chars.get(i + 1) == Some(&'>') {
            out.push(RedirectToken::Redirect(RedirectOp::OutAppend));
            i += 2;
            continue;
        }
        if c == '>' && chars.get(i + 1) == Some(&'|') {
            out.push(RedirectToken::Redirect(RedirectOp::OutClobber));
            i += 2;
            continue;
        }
        if c == '>' {
            out.push(RedirectToken::Redirect(RedirectOp::Out));
            i += 1;
            continue;
        }
        if c == '<' {
            out.push(RedirectToken::Redirect(RedirectOp::In));
            i += 1;
            continue;
        }
        let (word, opaque, consumed) = read_word(&chars[i..]);
        out.push(RedirectToken::Word(word, opaque));
        i += consumed.max(1);
    }
    out
}

/// Read one word starting at `chars[0]` (guaranteed to be a non-whitespace,
/// non-operator char by [`tokenize`]'s dispatch). Stops at whitespace or an
/// operator-starting char; single/double-quoted spans are dequoted inline
/// (their content still checked for [`AMBIGUOUS_CHARS`]). An unterminated
/// quote consumes the rest of the string and marks the word opaque.
fn read_word(chars: &[char]) -> (String, bool, usize) {
    let mut text = String::new();
    let mut opaque = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() || matches!(c, '>' | '<' | '|' | '&' | ';') {
            break;
        }
        match c {
            '\'' | '"' => {
                let quote = c;
                let mut j = i + 1;
                let mut closed = false;
                while j < chars.len() {
                    if chars[j] == quote {
                        closed = true;
                        break;
                    }
                    j += 1;
                }
                if !closed {
                    opaque = true;
                    text.extend(&chars[i..]);
                    i = chars.len();
                    break;
                }
                text.extend(&chars[i + 1..j]);
                i = j + 1;
            }
            _ if AMBIGUOUS_CHARS.contains(&c) => {
                opaque = true;
                text.push(c);
                i += 1;
            }
            _ => {
                text.push(c);
                i += 1;
            }
        }
    }
    (text, opaque, i)
}

/// One pipeline scope (already split on `&&`/`;`/`||`): collect every read
/// (a non-command-name plain operand, or an `< f` target) and every write
/// (`>`/`>|`/`>>` target), resolve each against `cwd`, and refuse if any
/// write path matches any read path.
/// #2560 round 2 "smallest narrowing": commands that never read a file
/// argument at all — refusing `echo f > f` costs the model's harmless first
/// instinct (#2558 test 1) for zero safety, since `echo` cannot truncate
/// anything it "reads". A stage whose command word is one of these adds NONE
/// of its operands to `reads`.
const NON_READING_COMMANDS: [&str; 5] = ["echo", "printf", "true", "false", ":"];

/// Subcommand DISPATCHERS: their first operand is a verb (`build`, `diff`,
/// `test`), never a path, so `cargo build > build` / `git diff > diff` must
/// not refuse on a coincidental name match. Only the FIRST operand is
/// skipped — a later plain operand (`git diff HEAD~1 file.rs > file.rs`)
/// still counts normally.
const SUBCOMMAND_DISPATCHERS: [&str; 6] = ["cargo", "git", "just", "make", "npm", "go"];

fn check_pipeline_redirects(tokens: &[RedirectToken], cmd: &str, cwd: &str) -> Option<String> {
    let mut reads: Vec<String> = Vec::new();
    let mut writes: Vec<String> = Vec::new();
    let mut at_stage_start = true;
    // The current stage's command word, and how many operands of THIS stage
    // have been seen so far — both reset on `Pipe`, never carried across
    // stages, so the narrowing rules below are per-command, not per-pipeline.
    let mut stage_command: Option<&str> = None;
    let mut operand_index = 0usize;
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            RedirectToken::Pipe => {
                at_stage_start = true;
                stage_command = None;
                operand_index = 0;
                i += 1;
            }
            RedirectToken::Sep => unreachable!("pipelines are pre-split on Sep"),
            RedirectToken::Redirect(op) => {
                at_stage_start = false;
                if let Some(RedirectToken::Word(text, false)) = tokens.get(i + 1) {
                    match op {
                        RedirectOp::In => reads.push(text.clone()),
                        RedirectOp::Out | RedirectOp::OutAppend | RedirectOp::OutClobber => {
                            writes.push(text.clone());
                        }
                    }
                    i += 2;
                } else {
                    // Missing or opaque target: never attributed either way.
                    i += 1;
                }
            }
            RedirectToken::Word(text, opaque) => {
                if at_stage_start {
                    stage_command = Some(text.as_str());
                    at_stage_start = false;
                } else if stage_command == Some("tee") {
                    // #2560 round 2: `tee` opens EVERY non-flag operand with
                    // O_TRUNC at startup — `sort f | tee f` is exactly as
                    // destructive as `sort f > f`, so its operands are
                    // WRITES, not reads. `-a` (append) is a flag, not a
                    // path — skipped like any flag — and still refused,
                    // same as `>>`: append is unsafe for a tool that streams.
                    if !opaque && !text.starts_with('-') {
                        writes.push(text.clone());
                    }
                    operand_index += 1;
                } else {
                    let non_reading =
                        stage_command.is_some_and(|c| NON_READING_COMMANDS.contains(&c));
                    let subcommand_slot = operand_index == 0
                        && stage_command.is_some_and(|c| SUBCOMMAND_DISPATCHERS.contains(&c));
                    if !opaque && !non_reading && !subcommand_slot {
                        reads.push(text.clone());
                    }
                    operand_index += 1;
                }
                i += 1;
            }
        }
    }
    // #2560 round 2: `resolve_exec_cwd` joins but does not normalize, so a
    // lexical spelling difference (`cat ./f > f`) missed the guard entirely
    // — `ws/./f` != `ws/f` by string equality. `lexically_normalize` (the
    // same normalizer `caveats::permits_path` already uses for containment)
    // collapses `.`/`..` components on BOTH sides before comparing.
    let normalized =
        |token: &str| crate::caveats::lexically_normalize(&resolve_exec_cwd(cwd, Some(token)));
    writes.iter().find_map(|write| {
        let write_path = normalized(write);
        reads
            .iter()
            .any(|read| normalized(read) == write_path)
            .then(|| {
                format!(
                    "error: refusing to run this command — it reads '{write}' and \
                     also redirects output to '{write}' in the same pipeline. The \
                     shell truncates '{write}' before the command finishes reading \
                     it, destroying the input. Write to a new name and move it into \
                     place instead (e.g. `awk … f > f.tmp && mv f.tmp f`).\n\
                     Refused command: {cmd}"
                )
            })
    })
}

pub(super) fn confined_dispatch_args(cmd: &str, cwd: &str) -> serde_json::Value {
    serde_json::json!({
        "cmd": cmd,
        "cwd": cwd,
        "env": venv_env_map(),
    })
}

/// The shell engine selected for this dispatch (ADR 0005 D2 seam). An explicit
/// `[shell] engine` / `--shell-engine` choice is published by the CLI through
/// `NEWT_SHELL_ENGINE`, so deep `run_command` dispatch reads it without threading
/// it through every signature. Full-access sessions retain their platform
/// default; otherwise the confined default is resolved below from the current L3
/// fence state rather than cached at startup.
pub(super) fn shell_engine() -> crate::ShellEngine {
    if let Some(engine) = std::env::var("NEWT_SHELL_ENGINE")
        .ok()
        .and_then(|s| s.parse::<crate::ShellEngine>().ok())
    {
        return engine;
    }
    // No engine was published (e.g. a non-CLI entry point that set
    // NEWT_FULL_ACCESS directly). Honor the same auto-upgrade the CLI applies so
    // `NEWT_FULL_ACCESS=1` alone still gets the full-grammar engine (`host` on
    // unix, `brush` on Windows).
    if full_access_requested() {
        return crate::full_access_default_engine();
    }
    // #1243 Leg 1: the CONFINED default is L3-gated and resolved HERE, per
    // dispatch — `dispatch_bridled_shell()` calls this on every run_command, so the
    // fence state is re-checked at exec time and never cached at startup (the
    // agent-bridle #239 TLA+ TOCTOU obligation). Brush when a kernel fence
    // enforces on this host; safe-subset's structural refusal otherwise.
    crate::confined_default_engine(crate::ocap_l3_backend().1)
}

/// agent-bridle's tool registry with the `"shell"` tool bound to the selected
/// engine (the ADR 0005 D2 seam: `safe-subset` / `host` / `brush` all honor the
/// same `Tool` contract under the `"shell"` name). `web_fetch` is added
/// unchanged. Mirrors `agent_bridle::registry()` but swaps the shell engine so
/// `[shell] engine = "host"` (or `--full-access`) routes `run_command` to the
/// full-grammar, kernel-jailed sandbox-host engine instead of the safe subset.
/// The b1 sandbox policy every `run_command` shell engine runs under: the
/// default backend confinement PLUS [`agent_bridle::ChildNetworkPolicy::DenyDirect`]
/// — the seccomp `socket()`-family egress deny (agent-bridle 0.7.15) that closes
/// the UDP/DNS/raw/packet leg the Landlock TCP-only net rule misses.
///
/// Applied unconditionally, but **inert unless the caller's `net` caveat is
/// already deny-all** (`net: none`): a granted net scope leaves it untouched (the
/// caller asked for egress), while a hostile / confined `run_command` under
/// `net: none` gets NO off-box socket of any protocol — the live attacker-exec
/// path finally inheriting the same complete egress floor as the
/// `ConstrainedExecutor` callers. Fail-closed: if the seccomp floor cannot be
/// installed, the spawn is refused rather than run with a weaker floor.
fn b1_run_command_sandbox_policy() -> agent_bridle::SandboxPolicy {
    agent_bridle::SandboxPolicy {
        child_network: agent_bridle::ChildNetworkPolicy::DenyDirect,
        ..crate::confined_exec::runtime_sandbox_policy()
    }
}

fn bridle_registry(
    engine: crate::ShellEngine,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
    wall: std::time::Duration,
) -> agent_bridle::Registry {
    use std::sync::Arc;
    let shell: Arc<dyn agent_bridle::Tool> = match engine {
        crate::ShellEngine::SafeSubset => {
            // F20: the wall rides on the limits — see `shell_limits`.
            let mut tool = agent_bridle::ShellTool::with_config(shell_limits(wall))
                .with_sandbox_policy(b1_run_command_sandbox_policy());
            if let Some(observer) = live.clone() {
                tool = tool.with_output_observer(observer);
            }
            Arc::new(tool)
        }
        crate::ShellEngine::Host => {
            let mut tool = agent_bridle::HostShellTool::new()
                .sandbox_policy(Arc::new(b1_run_command_sandbox_policy()));
            if let Some(observer) = live.clone() {
                tool = tool.with_output_observer(observer);
            }
            Arc::new(tool)
        }
        crate::ShellEngine::Brush => {
            // Cargo's library-test harness is not the `newt` executable and
            // therefore cannot service bridle's worker re-exec handshake
            // (`--agent-bridle-worker brush`). Keep unit tests on the same
            // confined Tool contract without attempting to re-exec the test
            // harness; the real binary path remains Brush unchanged.
            #[cfg(test)]
            let shell = {
                let mut tool = agent_bridle::ShellTool::with_config(shell_limits(wall))
                    .with_sandbox_policy(b1_run_command_sandbox_policy());
                if let Some(observer) = live {
                    tool = tool.with_output_observer(observer);
                }
                Arc::new(tool) as Arc<dyn agent_bridle::Tool>
            };
            #[cfg(not(test))]
            let shell = {
                // The carried brush engine (agent-bridle 0.7): in-process bash + the
                // L2 CommandInterceptor. The cross-platform engine — and on Windows
                // the ONLY full-grammar option, since `host` needs `/bin/sh`.
                #[cfg(windows)]
                {
                    use std::sync::Once;
                    static WARN: Once = Once::new();
                    WARN.call_once(|| {
                        tracing::warn!(
                            "using the 'brush' shell engine on Windows: run_command runs a \
                         bash-in-Rust shell for internal-tooling compatibility. Native \
                         PowerShell/cmd code paths are a FUTURE release — not written yet \
                         (we are opinionated Linux developers who occasionally use a \
                         MacBook). Bash-isms work; Windows-native shell semantics do not."
                        );
                    });
                }
                let mut tool = agent_bridle::BrushShellTool::new()
                    .with_timeout(wall)
                    .with_sandbox_policy(Arc::new(b1_run_command_sandbox_policy()));
                if let Some(observer) = live {
                    tool = tool.with_output_observer(observer);
                }
                Arc::new(tool) as Arc<dyn agent_bridle::Tool>
            };
            shell
        }
    };
    agent_bridle::Registry::builder()
        .tool(shell)
        .tool(Arc::new(agent_bridle::WebFetchTool::new()))
        .build()
}

/// #1176: should an about-to-run command be shadow-recorded? Exactly when it
/// runs UNCONFINED — via either of the two unconfined routes:
/// - `host_bypass` — the `--yolo`/`--disable-ocap` host-shell bypass, or
/// - `full_access` — a `--full-access` session, whose caveats are
///   `Caveats::top()`, so the confined bridle dispatch runs effectively
///   unconfined and would otherwise learn nothing.
///
/// A genuinely confined session (`host_bypass == false` and no full-access) is
/// NOT recorded: its leash is real, so there is no shadow to catch. Pure over
/// the two booleans so the gating is unit-tested without a live shell. Before
/// #1176's full-access parity, only the host-bypass arm recorded — a bare
/// `--full-access` run armed the recorder yet never wrote.
pub(super) fn shadow_records(host_bypass: bool, full_access: bool) -> bool {
    host_bypass || full_access
}

/// Does the caller's effective exec FLOOR permit running `cmd` on the
/// UNCONFINED host shell?
///
/// `None` means the caller found no effective exec floor after composing the
/// session, posture, mode, and persona constraints. The floor therefore
/// imposes nothing and the `--disable-ocap` bypass behaves as it did pre-#307.
///
/// `Some(scope)` ⇒ the bypass may proceed ONLY for a single, simple command
/// whose program (leading token) the scope authorizes. This is deliberately
/// conservative on TWO counts, because the host shell runs `cmd` verbatim with
/// no per-spawn interceptor:
///
/// 1. A **compound** command (containing a shell metacharacter that could chain
///    another program — `&&`, `||`, `;`, `|`, `` ` ``, `$(`, newline, `&`, `>`,
///    `<`) is NOT allowed to bypass. `echo ok && rm -rf /` would otherwise
///    smuggle `rm` past an `echo` grant. It falls through to the confined
///    shell, which gates every spawn.
/// 2. Only the leading token is matched, so a bare allow-listed program runs;
///    anything else is denied.
///
/// The denied command isn't refused outright — it falls to the confined-shell
/// path, which enforces the already-composed effective `caveats`. Every active
/// exec floor therefore keeps its ceiling even under `--yolo`.
pub(super) fn exec_floor_permits(floor: Option<&crate::caveats::Scope<String>>, cmd: &str) -> bool {
    use crate::caveats::ScopeExt as _;
    let Some(scope) = floor else {
        return true; // no effective exec floor ⇒ bypass unchanged
    };
    // Conservative: any shell control/redirection metacharacter that could
    // introduce a second program defeats leading-token matching, so refuse the
    // bypass and let the confined shell gate each spawn.
    const SHELL_META: &[char] = &['&', '|', ';', '`', '$', '\n', '>', '<', '(', ')'];
    if cmd.contains(SHELL_META) {
        return false;
    }
    match cmd.split_ascii_whitespace().next() {
        // An empty command runs nothing; let it through to the normal path.
        None => true,
        Some(prog) => scope.permits(&prog.to_string()),
    }
}

/// INTERIM (#297): run `cmd` on the PLAIN host shell — no leash, no
/// interceptor, no sandbox — and wrap the outcome in an envelope structurally
/// identical to the confined shell's (`{ exit_code, stdout, stderr,
/// sandbox_kind }`, with `denied` / `denials` omitted exactly as the bridle
/// envelope omits them when nothing was denied). [`envelope_denied`] and
/// [`shell_envelope_output`] — and therefore the loop's truncation / denial /
/// exit-code handling — apply to it unchanged.
///
/// A spawn failure surfaces as `Err`, which the caller formats as the same
/// `error: …` string a bridle dispatch failure produces.
/// Run `cmd` through the SAME confined-shell path the `run_command` tool uses —
/// the venv env seam, the `--disable-ocap` host bypass under the #307 exec
/// floor, the agent-bridle confined shell, and the #263 permission-gate re-ask —
/// and render the envelope. Shared by the `run_command` and `lifecycle` (#891)
/// arms so both honor **identical** exec caveats; the central presenter owns
/// the tool-call and completed-result block.
pub(super) async fn dispatch_bridled_shell(
    args: serde_json::Value,
    caveats: &crate::caveats::Caveats,
    sink: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
) -> agent_bridle::ToolResult<serde_json::Value> {
    let mut live = LiveOutputSession::start(sink);
    // NOTE (cross-platform review, `unconfined-fallback-on-missing-backend`):
    // run_command dispatches at the DEFAULT (Advisory) strength floor. On a
    // supported platform whose native backend is present (Linux+Landlock,
    // macOS+Seatbelt, Windows+AppContainer) the fs/net fence is kernel-enforced,
    // so this is confined. But where a RESTRICTED fs/net axis has NO native
    // backend at runtime (`best_available_sandbox` = advisory `NoopSandbox`) this
    // route runs ADVISORY (host) rather than refusing — the ConstrainedExecutor
    // callers fail closed there (Kernel floor). A blanket Kernel floor here is
    // WRONG: run_command legitimately restricts `exec`, which Landlock enforces
    // only as `interceptor` (the exec-behavior-bound BOUNDED residual), so a
    // blanket Kernel floor would refuse every exec-restricted command even on
    // Landlock. The correct fix is a PER-AXIS floor at the bridle boundary
    // (fs/net = Kernel, exec = Interceptor-OK); tracked as an ACTIVE deviation.
    // F20: a command whose leading program is a recognised build tool gets the
    // build lane's wall here too — this IS the lane a compound build command
    // (`cargo test …; echo …`) runs in, because #2533's routing refuses to
    // route anything compound. `dispatch_wall` reads only `args["cmd"]`, so
    // this changes nothing but the wall clock: `caveats` (fs/net/exec
    // authority) passed to `.dispatch()` below is untouched.
    let cmd = args.get("cmd").and_then(serde_json::Value::as_str);
    let wall = cmd.map_or_else(
        || std::time::Duration::from_secs(run_command_wall_secs()),
        dispatch_wall,
    );
    // dec1-build-grant round 2 (Reviewer FIX-FIRST, PR #2579): the toolchain
    // read roots for a build-tool command are added ONLY to the caveats used
    // for THIS dispatch, never folded back into the session's standing
    // authority (`widen_caveats` deliberately does not touch `fs_read` for an
    // exec grant — see its doc comment). Call-scoped, exactly like
    // `build_tool_caveats` for the lifecycle build lane.
    let dispatch_caveats = dispatch_caveats_for_command(cmd.unwrap_or(""), caveats);
    let result = bridle_registry(
        shell_engine(),
        live.as_ref().map(LiveOutputSession::relay),
        wall,
    )
    .dispatch("shell", args, &dispatch_caveats)
    .await;
    if let Some(live) = live.as_mut() {
        let ordinary_completion = result
            .as_ref()
            .ok()
            .and_then(|envelope| envelope.get("timed_out"))
            .and_then(serde_json::Value::as_bool)
            != Some(true);
        if result.is_ok() && ordinary_completion {
            live.finish_after_observer();
        } else {
            live.finish();
        }
    }
    result
}

/// Parse the entire invocation manifest before any approval can be consumed.
pub(super) fn declared_filesystem_requests(
    args: &serde_json::Value,
    cmd: &str,
    cwd: &str,
) -> Result<Vec<PermissionRequest>, String> {
    let mut requests = Vec::new();
    for (field, kind) in [
        ("fs_read", DenialKind::FsRead),
        ("fs_write", DenialKind::FsWrite),
    ] {
        let Some(value) = args.get(field) else {
            continue;
        };
        let invalid = || {
            format!("error: run_command {field} must be an array of nonempty absolute paths without NUL bytes")
        };
        for value in value.as_array().ok_or_else(invalid)? {
            let target = value.as_str().ok_or_else(invalid)?;
            if target.is_empty()
                || target.contains('\0')
                || !std::path::Path::new(target).is_absolute()
            {
                return Err(invalid());
            }
            if !requests
                .iter()
                .any(|request: &PermissionRequest| request.kind == kind && request.target == target)
            {
                requests.push(PermissionRequest {
                    tool: "run_command".into(),
                    kind,
                    target: target.into(),
                    reason: format!("declared {field} for command {cmd:?} in {cwd:?}"),
                });
            }
        }
    }
    Ok(requests)
}

fn permits_filesystem_request(
    caveats: &crate::caveats::Caveats,
    request: &PermissionRequest,
) -> bool {
    let scope = match request.kind {
        DenialKind::FsRead => &caveats.fs_read,
        DenialKind::FsWrite => &caveats.fs_write,
        _ => return false,
    };
    crate::caveats::permits_path(scope, &request.target)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn exec_confined_command(
    cmd: &str,
    // The directory the command runs in (#1159): the workspace root for
    // lifecycle, or a resolved workspace-confined cwd for run_command.
    cwd: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    filesystem_requests: &[PermissionRequest],
    exec_floor: Option<&crate::caveats::Scope<String>>,
    // F19: `&mut Option<&mut dyn PermissionGate>`, not `Option<&mut dyn
    // PermissionGate>` — the double indirection is what lets a caller
    // reborrow (`permission_gate` in the `lifecycle action=run` escalation)
    // for a SECOND sequential confined call in the same turn. The plain
    // `Option<&mut dyn Trait>` shape can't be reborrowed twice: each callee's
    // elided signature ties the trait object's own lifetime bound to the
    // reference's, so a second call can't be typed as "shorter-lived" than
    // the first — see `confirm_unrestricted_fs_mutation`'s param for the
    // same pattern already in this file.
    permission_gate: &mut Option<&mut dyn PermissionGate>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    live_tool_output: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
    presentation: &mut dyn ToolPresentation,
) -> (String, ExecOutcome) {
    // #2558 (HANDOFF item 2): refuse a same-file redirect (`cmd f > f`)
    // BEFORE either lane below runs anything — this is the single choke
    // point both the confined dispatch and the `--yolo` host-bypass share.
    if let Some(refusal) = same_file_redirect_refusal(cmd, cwd) {
        return (refusal, ExecOutcome::Denied);
    }

    // Venv injection (#783): the confined shell carries the venv via
    // agent-bridle's structured `env` seam (see `confined_dispatch_args` /
    // `venv_env_map`), NOT by prepending `export …;` to the command — an
    // `export` builtin is not a program, so the safe-subset engine refuses it on
    // a compound command (the #783 root cause). `cmd_with_venv` (the
    // `export …;`-prefixed form) is built ONLY for the host-bypass path below:
    // that runs on a real `/bin/sh` where `export` is a genuine builtin.
    let cmd_with_venv = match venv_cmd_prefix() {
        Some(prefix) => format!("{prefix}{cmd}"),
        None => cmd.to_string(),
    };

    // INTERIM (#297): --disable-ocap / --yolo / NEWT_DISABLE_OCAP=1 — run the
    // command UNCONFINED on the host shell instead of the bridle's confined
    // shell. Nothing is denied here, so the #263 permission gate below is never
    // consulted. #307 FLOOR: a named-permission-preset clamp WINS over the
    // bypass — the unconfined host path is taken ONLY if the floor permits this
    // command's leading token; else it falls through to the confined shell,
    // which enforces the already-clamped `caveats`. `None` keeps the bypass
    // bit-for-bit.
    let host_bypass = ocap_disabled() && exec_floor_permits(exec_floor, cmd);

    // #1176: shadow-OCAP — record the authority a leash WOULD have gated on
    // whenever this command runs UNCONFINED: the yolo/disable-ocap host bypass
    // above, OR a --full-access session (its caveats are `Caveats::top()`, so
    // the confined bridle dispatch below runs effectively unconfined and would
    // otherwise learn nothing). A genuinely confined session is not recorded —
    // its leash is real. No-op unless recording is armed (NEWT_FLIGHT_RECORDER).
    // `newt ocap propose` folds the capture into reviewable policy candidates.
    if shadow_records(host_bypass, full_access_requested()) {
        crate::flight_recorder::log_unconfined(cmd);
    }

    if host_bypass {
        let mut live = LiveOutputSession::start(live_tool_output);
        let run = host_shell_dispatch(
            &cmd_with_venv,
            cwd,
            live.as_ref().map(LiveOutputSession::relay),
        )
        .await;
        if let Some(live) = live.as_mut() {
            live.finish();
        }
        return match run {
            Ok(envelope) => host_result(&envelope, |envelope| {
                shell_envelope_output(
                    envelope,
                    tool_output_lines,
                    color,
                    tool_offload,
                    spill_store,
                    Some(&mut *presentation),
                )
            }),
            Err(e) => (format!("error: {e}"), ExecOutcome::Unavailable),
        };
    }

    // A proactive session grant can change standing authority during this
    // turn. Refresh before spawning: a child filesystem refusal is not a
    // structured denial that can enter the permission-retry path below.
    let refreshed = match permission_gate.as_deref_mut() {
        Some(gate) => match gate.refresh_caveats(caveats) {
            PermissionDecision::Allow(current) => Some(current),
            PermissionDecision::Deny => {
                return (
                    "capability denied: current permission authority was refused".to_string(),
                    ExecOutcome::Denied,
                );
            }
        },
        None => None,
    };
    let caveats = refreshed.as_ref().unwrap_or(caveats);

    let missing: Vec<_> = filesystem_requests
        .iter()
        .filter(|request| !permits_filesystem_request(caveats, request))
        .cloned()
        .collect();
    let admitted = if missing.is_empty() {
        None
    } else {
        let decision = permission_gate
            .as_deref_mut()
            .map(|gate| gate.ask_with_caveats(caveats, &missing));
        match decision {
            Some(PermissionDecision::Allow(allowed))
                if filesystem_requests
                    .iter()
                    .all(|request| permits_filesystem_request(&allowed, request)) =>
            {
                Some(allowed)
            }
            _ => return (
                "capability denied: declared filesystem authority was not granted for this command"
                    .into(),
                ExecOutcome::Denied,
            ),
        }
    };
    let caveats = admitted.as_ref().unwrap_or(caveats);

    // #783: RAW cmd + venv via the env seam — never the `export …;` prefix,
    // which the confined safe-subset engine refuses.
    let dispatch_args = confined_dispatch_args(cmd, cwd);
    match dispatch_bridled_shell(dispatch_args.clone(), caveats, live_tool_output.clone()).await {
        // The confined shell ran. Its envelope carries
        // `{ exit_code, stdout, stderr, timed_out, ... }` plus — when the leash
        // refused a capability — the STRUCTURED denial fields
        // `{ denied: true, denials: [{ kind, target, reason }] }`. In free-form
        // mode an out-of-scope command is denied *inside* the shell by the brush
        // interceptor (the command genuinely does not run); we lift that to the
        // capability-denied UX by reading the structured `denied` field — NEVER
        // a stderr grep.
        Ok(envelope) => {
            if envelope_denied(&envelope) {
                // Repair evidence is distinct from the prompted-decision log: keep
                // the redacted raw command + structured refusal even when prompting
                // is off or the operator allows it. This is what lets `newt ocap
                // denials` distinguish a policy gap from a parser/implementation
                // defect instead of repeatedly granting a bogus target.
                crate::denial_journal::record_envelope(
                    cmd,
                    cwd,
                    crate::denial_journal::DenialStage::Initial,
                    &envelope,
                );
                // #263: an interactive gate may turn this denial into a human grant.
                // ONE consult + ONE re-execution per call: a second denial (a
                // different target reached on the re-run) surfaces as the standard
                // envelope — the model can retry, which prompts afresh.
                //
                // F19/#2541 round 2: REBORROW (`as_deref_mut`), never `take()`. Taking
                // permanently empties the caller's `&mut Option<&mut dyn PermissionGate>`
                // — the exact double indirection `lifecycle_run_with_escalation` relies on
                // to reuse the SAME gate for its second confined call. `take()` here left
                // that second call (`run_confined_build_lane`) with `permission_gate ==
                // None` whenever THIS run's denial had just been allowed and the re-dispatch
                // then timed out: the escalation refused "requires explicit … authority"
                // without ever asking the operator who just said yes. Nothing below this
                // block touches `permission_gate` again, so reborrowing costs nothing here.
                if let Some(gate) = permission_gate.as_deref_mut() {
                    // #905: promptable exec denials OR net-host denials (agent-bridle
                    // #196). On Allow, the re-mint widens the matching axis (net adds
                    // the host to the allow-list), so the proxy admits it on re-run.
                    if let Some(requests) =
                        exec_denial_requests(&envelope).or_else(|| net_denial_requests(&envelope))
                    {
                        if let PermissionDecision::Allow(widened) =
                            gate.ask_with_caveats(caveats, &requests)
                        {
                            if !filesystem_requests
                                .iter()
                                .all(|request| permits_filesystem_request(&widened, request))
                            {
                                return (
                                    "capability denied: declared filesystem authority was not retained for this command".into(),
                                    ExecOutcome::Denied,
                                );
                            }
                            return match dispatch_bridled_shell(
                                dispatch_args,
                                &widened,
                                live_tool_output,
                            )
                            .await
                            {
                                Ok(env2) if envelope_denied(&env2) => {
                                    crate::denial_journal::record_envelope(
                                        cmd,
                                        cwd,
                                        crate::denial_journal::DenialStage::AfterGrant,
                                        &env2,
                                    );
                                    (denied_run_command_result(&env2, color), ExecOutcome::Denied)
                                }
                                Ok(env2) => (
                                    shell_envelope_output(
                                        &env2,
                                        tool_output_lines,
                                        color,
                                        tool_offload,
                                        spill_store,
                                        Some(&mut *presentation),
                                    ),
                                    envelope_outcome(&env2),
                                ),
                                Err(e) => (format!("error: {e}"), ExecOutcome::Unavailable),
                            };
                        }
                    }
                }
            }
            confined_result(cmd, &envelope, caveats, color, |envelope| {
                shell_envelope_output(
                    envelope,
                    tool_output_lines,
                    color,
                    tool_offload,
                    spill_store,
                    Some(&mut *presentation),
                )
            })
        }
        // An argv-mode leash denial, or an error from inside the tool — surface
        // the reason; the dispatch error Display is safe to show.
        Err(e) => (format!("error: {e}"), ExecOutcome::Unavailable),
    }
}

/// #2315: the class of a completed envelope from its own facts — structured
/// denial, then the `timed_out` flag (NOT exit 124, which a check wrapped in
/// GNU `timeout` returns on its own), then the exit status.
pub(super) fn envelope_outcome(envelope: &serde_json::Value) -> ExecOutcome {
    if envelope_denied(envelope) {
        ExecOutcome::Denied
    } else if envelope
        .get("timed_out")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        ExecOutcome::TimedOut
    } else if envelope
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        == Some(0)
    {
        ExecOutcome::Passed
    } else {
        ExecOutcome::Failed
    }
}

/// The confined lane's result for an envelope no grant widened: each rendering
/// is paired with the class of the branch that chose it, so the class and the
/// text the model reads cannot disagree.
pub(super) fn confined_result(
    cmd: &str,
    envelope: &serde_json::Value,
    caveats: &crate::caveats::Caveats,
    color: bool,
    render: impl FnOnce(&serde_json::Value) -> String,
) -> (String, ExecOutcome) {
    if envelope_denied(envelope) {
        return (
            denied_run_command_result(envelope, color),
            ExecOutcome::Denied,
        );
    }
    // #2274: a 127 with no structured denial is an ABSENCE, not an
    // ordinary failure. Name it before it renders as the ambiguous
    // `error: command exited 127`, which is indistinguishable from a
    // broken machine. Returns None for everything else, so ordinary
    // output and ordinary failures fall through untouched.
    if let Some(refusal) = absent_binary_refusal(envelope, &caveats.exec) {
        return (refusal, ExecOutcome::Unavailable);
    }
    // #2273: a 126 with no structured denial whose program sits
    // outside the fs-read grant is the KERNEL refusing, not a
    // chmod the model forgot. Same structured test, one more state.
    if let Some(refusal) = kernel_refused_binary(cmd, envelope, &caveats.fs_read) {
        return (refusal, ExecOutcome::Denied);
    }
    let outcome = envelope_outcome(envelope);
    let mut text = render(envelope);
    if outcome == ExecOutcome::TimedOut {
        text.push_str(&timed_out_note(dispatch_wall(cmd)));
    }
    (text, outcome)
}

/// How long `lifecycle action=build` may run: the sanctioned lane for work that
/// outlives the `run_command` wall.
pub(super) const LIFECYCLE_BUILD_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30 * 60);

/// The literal call the coaching shows, so a model copies a valid shape instead
/// of inventing `phase="build"`.
const LIFECYCLE_BUILD_CALL: &str = r#"{"action":"build","phase":"test"}"#;

/// The confined shell's per-call wall clock (agent-bridle's default; newt never
/// overrides it, and the model has no argument to raise it).
fn run_command_wall_secs() -> u64 {
    agent_bridle::LimitsPolicy::default().default_timeout_secs
}

/// F20: the wall clock for THIS `cmd` — [`LIFECYCLE_BUILD_TIMEOUT`] when its
/// leading program is a recognised build tool (reusing routing's
/// `is_build_tool_program` table, not a second list), else the ordinary
/// [`run_command_wall_secs`]. This is the same 60s-vs-30min split
/// `lifecycle action=build` already gets — just applied to the shell lane a
/// COMPOUND build command (`cargo test …; echo …`) actually runs in, since
/// #2533's routing refuses to route anything compound. Only the wall clock
/// changes: fs/net/exec authority is unaffected (`dispatch_bridled_shell`
/// passes `caveats` through unchanged).
/// `ShellTool` limits for a call whose wall is `wall`. The model's args carry
/// no `timeout_secs`, so `ShellTool` falls back to `default_timeout_secs`:
/// that field IS the wall. `max_timeout_secs` is raised with it or the clamp
/// would cut a build wall back to 300s.
pub(super) fn shell_limits(wall: std::time::Duration) -> agent_bridle::LimitsPolicy {
    let default = agent_bridle::LimitsPolicy::default();
    agent_bridle::LimitsPolicy {
        default_timeout_secs: wall.as_secs(),
        max_timeout_secs: wall.as_secs().max(default.max_timeout_secs),
        ..default
    }
}

pub(super) fn dispatch_wall(cmd: &str) -> std::time::Duration {
    match leading_program(cmd) {
        Some(program) if crate::agentic::routing::is_build_tool_program(program) => {
            LIFECYCLE_BUILD_TIMEOUT
        }
        _ => std::time::Duration::from_secs(run_command_wall_secs()),
    }
}

/// What the `run_command` description tells the model about its wall, up front.
/// One owner with [`timed_out_note`], so the two cannot disagree (U6).
pub(super) fn run_command_limit_sentence() -> String {
    format!(
        "Each call is killed after {} seconds (wall clock), {} minutes when it starts with \
         `cargo` or `just`; the result carries the exit code. For an offline build use \
         `lifecycle` action=build, i.e. call the tool with {LIFECYCLE_BUILD_CALL} (`phase` is \
         never `build`), or narrow the command (one test filter, one crate).",
        run_command_wall_secs(),
        LIFECYCLE_BUILD_TIMEOUT.as_secs() / 60
    )
}

/// Appended to a timed-out confined result: the limit, that the command was
/// killed, and the lane for long builds. `wall` is the clock that actually
/// applied to this call (F20: a build-tool command gets the build wall, not
/// the default) — named explicitly, so a reader can tell why a call ran for
/// minutes instead of assuming the default 60s.
///
/// `pub(super)` (#2541 follow-up): `tools.rs`'s `escalation_result` strips
/// this exact suffix from a timed-out first run's text when the escalation
/// it recommends was just DECLINED — a result must carry ONE recommendation,
/// never this note's "use lifecycle action=build" immediately followed by
/// "build authority was declined".
pub(super) fn timed_out_note(wall: std::time::Duration) -> String {
    let default = std::time::Duration::from_secs(run_command_wall_secs());
    let wall_note = if wall == default {
        format!("{}s wall", wall.as_secs())
    } else {
        format!(
            "{}s build-lane wall (not the default {}s)",
            wall.as_secs(),
            default.as_secs()
        )
    };
    // A build command already had the build lane's 30 minutes: the only
    // useful advice left is to narrow it.
    if wall == LIFECYCLE_BUILD_TIMEOUT {
        return format!(
            "\n(the command hit the {wall_note} and was killed; the output above is partial. \
             Narrow the command: one crate, one test filter.)"
        );
    }
    format!(
        "\n(the command hit the {wall_note} and was killed; the output above is partial. For a \
         build or full test run use `lifecycle` action=build, i.e. call the tool with \
         {LIFECYCLE_BUILD_CALL} ({}-minute limit, confined, offline; `phase` is the phase to run, not `build`), or \
         narrow the command.)",
        LIFECYCLE_BUILD_TIMEOUT.as_secs() / 60
    )
}

/// The host lane's result (`--unsafe-host-exec`). Same rule as the confined
/// lane: a 127 is `unavailable` only when the shell names the program it could
/// not find AND that program does not resolve on this host. Any other 127 (a
/// bare `exit 127`, a check script's own status) failed and stays repairable.
pub(super) fn host_result(
    envelope: &serde_json::Value,
    render: impl FnOnce(&serde_json::Value) -> String,
) -> (String, ExecOutcome) {
    let outcome = match envelope_outcome(envelope) {
        ExecOutcome::Failed
            if envelope
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                == Some(127)
                && host_named_missing(envelope)
                    .is_some_and(|prog| host_path_lookup(prog).is_none()) =>
        {
            ExecOutcome::Unavailable
        }
        outcome => outcome,
    };
    (render(envelope), outcome)
}

/// How the host shells [`host_shell_output`] runs (bash, else sh) name a
/// program they could not find: `bash: line 1: X: command not found`,
/// `sh: 1: X: not found`. A line must carry the shell's own prefix, so a
/// program printing a look-alike is not read as the shell.
const HOST_NOT_FOUND: [(&str, &str); 2] =
    [("bash: ", ": command not found"), ("sh: ", ": not found")];

/// The program the host shell last reported as not found. Stderr only NAMES
/// it; [`host_result`] decides on the exit code and resolution.
fn host_named_missing(envelope: &serde_json::Value) -> Option<&str> {
    let stderr = envelope.get("stderr")?.as_str()?;
    stderr.lines().rev().find_map(|line| {
        HOST_NOT_FOUND.iter().find_map(|(prefix, suffix)| {
            line.strip_prefix(prefix)?
                .strip_suffix(suffix)?
                .rsplit(": ")
                .next()
        })
    })
}

pub(super) async fn host_shell_dispatch(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
) -> std::io::Result<serde_json::Value> {
    let run = host_shell_output(cmd, cwd, live).await?;
    Ok(serde_json::json!({
        "exit_code": run.exit_code,
        "stdout": decode_shell_stream(&run.stdout),
        "stderr": decode_shell_stream(&run.stderr),
        // Same `timed_out` flag the confined bridle envelope carries, so the
        // host-bypass path can never wedge the session on a hung child (#297).
        "timed_out": run.timed_out,
        // Honest provenance, same field the bridle envelope always carries:
        // nothing sandboxed this run.
        "sandbox_kind": "none",
    }))
}

/// Result of a host-bypass shell run. Unlike a raw [`std::process::Output`] this
/// carries an explicit `timed_out` flag so the dispatch layer can emit the same
/// envelope shape the confined path does when a child is killed for running long.
pub(super) struct HostShellRun {
    pub(super) exit_code: i64,
    pub(super) stdout: Vec<u8>,
    stderr: Vec<u8>,
    pub(super) timed_out: bool,
}

/// Wall-clock ceiling for a single host-bypass shell command. A child that
/// blocks past this (a REPL awaiting input, an accidental `cat` with no args,
/// an interactive prompt) is killed rather than wedging the whole turn — the
/// interrupt keyboard-watcher cannot help once a foreground child owns the tty.
///
/// Convention-driven: override with `NEWT_HOST_EXEC_TIMEOUT_SECS` (a positive
/// integer number of seconds). Absent/blank/invalid/zero falls back to the
/// 120s default, mirroring the confined shell's bound.
fn host_exec_timeout() -> std::time::Duration {
    const DEFAULT_SECS: u64 = 120;
    let secs = std::env::var("NEWT_HOST_EXEC_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

pub(super) fn decode_shell_stream(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => repair_bsd_cat_v_utf8(bytes)
            .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// macOS/BSD `cat -v` is not Unicode-aware: for a UTF-8 glyph such as `─`
/// (`e2 94 80`) it emits the lead byte raw (`e2`) and renders only the
/// continuation bytes as ASCII meta-control notation (`M-^TM-^@`). That byte
/// stream is invalid UTF-8, so a plain lossy decode becomes `�M-^TM-^@`.
///
/// Repair only that precise shape. This keeps ordinary valid UTF-8 untouched and
/// leaves unrelated binary output on the existing lossy fallback.
fn repair_bsd_cat_v_utf8(bytes: &[u8]) -> Option<String> {
    let mut repaired = Vec::with_capacity(bytes.len());
    let mut changed = false;
    let mut i = 0;
    while i < bytes.len() {
        let lead = bytes[i];
        let Some(cont_count) = utf8_continuation_count(lead) else {
            repaired.push(lead);
            i += 1;
            continue;
        };

        let mut seq = Vec::with_capacity(cont_count + 1);
        seq.push(lead);
        let mut j = i + 1;
        let mut ok = true;
        for _ in 0..cont_count {
            match parse_cat_v_meta_byte(bytes, j) {
                Some((cont, next)) if (0x80..=0xbf).contains(&cont) => {
                    seq.push(cont);
                    j = next;
                }
                _ => {
                    ok = false;
                    break;
                }
            }
        }

        if ok && std::str::from_utf8(&seq).is_ok() {
            repaired.extend_from_slice(&seq);
            changed = true;
            i = j;
        } else {
            repaired.push(lead);
            i += 1;
        }
    }

    changed.then(|| String::from_utf8(repaired).ok()).flatten()
}

fn utf8_continuation_count(lead: u8) -> Option<usize> {
    match lead {
        0xc2..=0xdf => Some(1),
        0xe0..=0xef => Some(2),
        0xf0..=0xf4 => Some(3),
        _ => None,
    }
}

fn parse_cat_v_meta_byte(bytes: &[u8], start: usize) -> Option<(u8, usize)> {
    if start + 2 > bytes.len() || &bytes[start..start + 2] != b"M-" {
        return None;
    }
    let pos = start + 2;
    match bytes.get(pos).copied()? {
        b'^' => {
            let c = bytes.get(pos + 1).copied()?;
            let low = if c == b'?' {
                0x7f
            } else if (b'@'..=b'_').contains(&c) {
                c - b'@'
            } else {
                return None;
            };
            Some((low | 0x80, pos + 2))
        }
        c if (0x20..=0x7e).contains(&c) => Some((c | 0x80, pos + 1)),
        _ => None,
    }
}

async fn drain_host_pipe<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
    stream: crate::agentic::ToolOutputStream,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;

    let mut output = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(output);
        }
        if let Some(live) = live.as_ref() {
            live.write(stream, &chunk[..read]);
        }
        output.extend_from_slice(&chunk[..read]);
    }
}

/// INTERIM (#297) host shell selection: `bash -c` with an `sh -c` fallback
/// when bash is absent — the same sh-compatible free-form mode the confined
/// shell ran, so [`venv_cmd_prefix`]'s `export …;` prefix works unchanged.
///
/// Hardened (#297) so a hung child can never wedge the turn:
/// - `kill_on_drop(true)` + `process_group(0)` (own process group) so the whole
///   child *tree* dies when we drop the handle, not just the immediate `bash`.
/// - stdin redirected from `/dev/null` so a child that reads stdin sees EOF
///   instead of blocking forever waiting on a tty the agent can't feed.
/// - a [`host_exec_timeout`] wall-clock ceiling: on expiry the child is killed
///   and we return `timed_out: true` with exit code 124, matching the confined
///   path's envelope shape.
#[cfg(not(windows))]
pub(super) async fn host_shell_output(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
) -> std::io::Result<HostShellRun> {
    host_shell_output_with_timeout(cmd, cwd, live, host_exec_timeout()).await
}

/// newt's control-plane env vars that must NEVER flow into a host-shell child
/// (#8 / invariant 9). Two classes, both newt-internal:
///
/// - **authority switches** — an inherited `NEWT_DISABLE_OCAP` /
///   `NEWT_FULL_ACCESS` / `NEWT_UNSAFE_HOST_EXEC` / … would silently re-assert
///   authority the session did not grant: a `newt` spawned by a Yolo child would
///   re-derive Yolo from the env twin instead of a fresh operator decision (the
///   authority-switch-survives-one-hop hole);
/// - **newt's own secrets** — `NEWT_AGENT_KEY` (the capability envelope),
///   `NEWT_OPERATOR_KEY`, and `NEWT_TOKEN_PASSPHRASE` (the encrypted-token-store
///   unlock) would let the child forge capabilities or decrypt the token store.
///
/// Knowledge in data (three Cs): a new `NEWT_` control switch is added here. The
/// child KEEPS the operator's *general* environment — the `--full-access` /
/// `--disable-ocap` lane is the operator's explicit "run with my ambient
/// authority" opt-out, so provider credentials etc. are their deliberate grant;
/// only newt's OWN control plane is excised.
pub(super) const CHILD_STRIPPED_AUTHORITY_ENV: &[&str] = &[
    "NEWT_DISABLE_OCAP",
    "NEWT_FULL_ACCESS",
    "NEWT_UNSAFE_HOST_EXEC",
    "NEWT_BENCH_OCAP",
    "NEWT_SHELL_ENGINE",
    "NEWT_SHELL_ENV_PASSTHROUGH",
    "NEWT_WRITE_PATHS",
    "NEWT_READ_PATHS",
    "NEWT_EXEC_PATHS",
    "NEWT_VENV",
    "NEWT_NO_ROUTE",
    "NEWT_AGENT_KEY",
    "NEWT_OPERATOR_KEY",
    "NEWT_TOKEN_PASSPHRASE",
];

/// Excise newt's whole control plane ([`CHILD_STRIPPED_AUTHORITY_ENV`]) from a
/// host-shell child. `env_remove` marks each key removed in the child's env plan
/// whether or not it is currently set, so no authority switch or newt secret can
/// reach the child regardless of the ambient environment.
fn strip_child_authority_env(c: &mut tokio::process::Command) {
    for key in CHILD_STRIPPED_AUTHORITY_ENV {
        c.env_remove(key);
    }
}

/// Build the host-shell child command, stripping newt's whole control plane
/// (authority switches + newt's own secrets, [`strip_child_authority_env`]) so
/// none can flow into the child and re-assert authority the session did not
/// grant it or leak newt's credentials (#8 / step-7.1a, invariant 9). The child
/// inherits the rest of the environment (the Yolo lane's explicit ambient-
/// authority grant). Own process group (setsid-equivalent) + `kill_on_drop` so a
/// hung or tty-stealing child is reaped as a whole tree.
#[cfg(not(windows))]
pub(super) fn host_shell_command(program: &str, cmd: &str, cwd: &str) -> tokio::process::Command {
    use std::process::Stdio;
    let mut c = tokio::process::Command::new(program);
    c.arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        // A child that reads stdin gets EOF, never a blocking wait on a tty
        // the agent cannot drive.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    strip_child_authority_env(&mut c);
    c
}

#[cfg(not(windows))]
pub(super) async fn host_shell_output_with_timeout(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
    timeout: std::time::Duration,
) -> std::io::Result<HostShellRun> {
    async fn run_one(
        mut child: tokio::process::Child,
        live: Option<std::sync::Arc<LiveOutputRelay>>,
        timeout: std::time::Duration,
    ) -> std::io::Result<HostShellRun> {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("host shell stdout was not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("host shell stderr was not piped"))?;
        let completed = async {
            let (status, stdout, stderr) = tokio::try_join!(
                child.wait(),
                drain_host_pipe(
                    stdout,
                    live.clone(),
                    crate::agentic::ToolOutputStream::Stdout
                ),
                drain_host_pipe(stderr, live, crate::agentic::ToolOutputStream::Stderr),
            )?;
            Ok::<_, std::io::Error>((status, stdout, stderr))
        };
        match tokio::time::timeout(timeout, completed).await {
            Ok(Ok((status, stdout, stderr))) => Ok(HostShellRun {
                exit_code: status.code().unwrap_or(-1) as i64,
                stdout,
                stderr,
                timed_out: false,
            }),
            Ok(Err(e)) => Err(e),
            Err(_elapsed) => Ok(HostShellRun {
                exit_code: 124,
                stdout: Vec::new(),
                stderr: format!(
                    "command exceeded {}s host-shell timeout and was killed\n",
                    timeout.as_secs()
                )
                .into_bytes(),
                timed_out: true,
            }),
        }
    }

    match host_shell_command("bash", cmd, cwd).spawn() {
        Ok(child) => run_one(child, live, timeout).await,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            run_one(host_shell_command("sh", cmd, cwd).spawn()?, live, timeout).await
        }
        Err(e) => Err(e),
    }
}

/// INTERIM (#297) host shell selection on Windows: `cmd /C`, the same shape
/// as [`build_check_shell`]. Bounded by [`host_exec_timeout`] with
/// `kill_on_drop` so a hung child cannot wedge the turn.
#[cfg(windows)]
pub(super) async fn host_shell_output(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
) -> std::io::Result<HostShellRun> {
    host_shell_output_with_timeout(cmd, cwd, live, host_exec_timeout()).await
}

#[cfg(windows)]
pub(super) async fn host_shell_output_with_timeout(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
    timeout: std::time::Duration,
) -> std::io::Result<HostShellRun> {
    use std::process::Stdio;

    // step-7.1a / #8 / invariant 9: newt's whole control plane (authority
    // switches + newt's own secrets) must not flow into the host-shell child.
    let mut cmd_builder = tokio::process::Command::new("cmd");
    cmd_builder
        .args(["/C", cmd])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    strip_child_authority_env(&mut cmd_builder);
    let mut child = cmd_builder.spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("host shell stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("host shell stderr was not piped"))?;
    let completed = async {
        let (status, stdout, stderr) = tokio::try_join!(
            child.wait(),
            drain_host_pipe(
                stdout,
                live.clone(),
                crate::agentic::ToolOutputStream::Stdout
            ),
            drain_host_pipe(stderr, live, crate::agentic::ToolOutputStream::Stderr),
        )?;
        Ok::<_, std::io::Error>((status, stdout, stderr))
    };
    match tokio::time::timeout(timeout, completed).await {
        Ok(Ok((status, stdout, stderr))) => Ok(HostShellRun {
            exit_code: status.code().unwrap_or(-1) as i64,
            stdout,
            stderr,
            timed_out: false,
        }),
        Ok(Err(e)) => Err(e),
        Err(_elapsed) => Ok(HostShellRun {
            exit_code: 124,
            stdout: Vec::new(),
            stderr: format!(
                "command exceeded {}s host-shell timeout and was killed\r\n",
                timeout.as_secs()
            )
            .into_bytes(),
            timed_out: true,
        }),
    }
}

/// Whether a confined-shell envelope carries the STRUCTURED `denied: true`
/// flag — the leash's machine-readable signal that the brush interceptor
/// refused an exec / open inside the free-form command. Reads the structured
/// field agent-bridle emits; it does NOT parse stdout/stderr (the old stderr
/// string-match was fragile — a command that merely *printed* a denial-like
/// phrase could be misread, and any wording drift would silently break
/// detection).
pub(super) fn envelope_denied(envelope: &serde_json::Value) -> bool {
    envelope
        .get("denied")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Build a human-readable denial message from the envelope's structured
/// `denials: [{ kind, target, reason }]` list, joining each entry's `reason`.
/// Falls back to a generic message when the list is missing or empty.
pub(super) fn envelope_denial_reason(envelope: &serde_json::Value) -> String {
    let reasons: Vec<String> = envelope
        .get("denials")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d.get("reason").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if reasons.is_empty() {
        "denied: the capability leash refused an operation".to_string()
    } else {
        reasons.join("; ")
    }
}

/// The standard `run_command` capability-denial result, composed EXACTLY ONCE:
/// a single `capability denied: <bare reason>. <recovery hint>` for the model.
///
/// #775 (§2.5): the model-facing message is ONE clean level. Two earlier defects
/// are removed:
///
/// 1. The bare denial `reason` (a full sentence from the leash, e.g.
///    `exec of "export" is not within the granted authority`) is NO LONGER
///    stuffed into the old `print_denied` bare `'{target}'` slot. Doing so produced
///    the garbled `capability denied: exec does not permit '<whole reason
///    sentence> - add it via …>'` — a denial sentence nested inside another.
///    That notice path received only the BARE command target, matching its
///    `{axis} does not permit '{target}'` contract (the same shape the fs path
///    uses via [`super::denied_fs_result`]).
/// 2. The stale `extra_exec` config hint is gone from the model-facing message.
///    #721 superseded "edit your `[tui.permissions]` config" with the
///    model-actionable [`denial_recovery_hint`] (`request_permissions`), so the
///    model now sees the bare reason once plus that hint — never a config edit
///    it cannot perform mid-turn.
///
/// The #263 prompt path still falls back here on deny (and on a second denial
/// after a re-execution).
pub(super) fn denied_run_command_result(envelope: &serde_json::Value, _color: bool) -> String {
    let recovery = denial_recovery_hints(envelope)
        .map(|hints| hints.join(" "))
        .unwrap_or_else(|| {
            "No actionable capability grant is identified. Take a different approach within your current authority.".into()
        });
    format!(
        "capability denied: {}. {}",
        envelope_denial_reason(envelope),
        recovery
    )
}

/// Preserve each grantable denial's axis and target. A structural or opaque
/// refusal makes the batch ungrantable; never guess an axis or join targets.
pub(super) fn denial_recovery_hints(envelope: &serde_json::Value) -> Option<Vec<String>> {
    let denials = envelope
        .get("denials")?
        .as_array()
        .filter(|rows| !rows.is_empty())?;
    denials
        .iter()
        .map(|denial| {
            let kind = denial.get("kind")?.as_str()?;
            if !matches!(kind, "exec" | "fs_read" | "fs_write" | "net")
                || denial
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(is_structural_refusal)
            {
                return None;
            }
            let target = denial
                .get("target")?
                .as_str()
                .filter(|target| !target.trim().is_empty())?;
            Some(denial_recovery_hint(kind, target))
        })
        .collect()
}

/// #2274 — absence must never be ambiguous.
///
/// A binary the carried userland does not carry used to reach the model as
/// `error: command exited 127` wrapped around brush's own `command not found:
/// X`, which is indistinguishable from a broken machine. That ambiguity is why
/// the confined profile read as flakiness for six months instead of as policy.
///
/// Three states hide behind that one rendering, and the right next move differs
/// in each, so the model must be able to tell them apart:
///
/// | state | envelope | remedy |
/// |---|---|---|
/// | denied by a grant | `denied:true` + `denials[kind=exec]`, exit 126 | ask for the grant |
/// | not carried, on the host | exit 127, no denials, host PATH resolves | build authority for project validation, or direct `exec:<abs path>` |
/// | not on this host at all | exit 127, no denials, host PATH misses | no grant can help |
///
/// Discrimination is STRUCTURED — the exit code plus the `denials` array — the
/// same rule [`envelope_denied`] follows, and for the same reason: a command
/// that merely *prints* a denial-like phrase must not be misread. Exit 127 is
/// brush's `CommandNotFound`; a refusal the interceptor actually made carries
/// `denials` and is exit 126, so it is excluded here and left to
/// [`denied_run_command_result`].
///
/// An absence also needs brush to NAME the program it could not find (#2315).
/// A 127 it did not attribute (a `make` recipe, a test harness returning 127)
/// is a check failing on its own terms: refusing it as an absence would route
/// a repairable failure to the no-edit-can-help guidance.
///
/// Returns `None` for anything that is not an absence, leaving ordinary output
/// and ordinary failures untouched.
///
/// Only newt can answer the last row. brush runs behind the fence, where a PATH
/// probe would be answering about the fence rather than about the host; newt's
/// own process is never Landlocked, so it can resolve the real host PATH and
/// separate "installed, but not reachable from in here" from "not installed".
pub(crate) fn absent_binary_refusal(
    envelope: &serde_json::Value,
    exec: &crate::caveats::Scope<String>,
) -> Option<String> {
    // 127 = brush's `ExecutionExitCode::NotFound`: the program was never
    // resolved, so the interceptor was never consulted.
    if envelope
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        != Some(127)
    {
        return None;
    }
    // A structured refusal is a DENIAL, not an absence. Relabelling one as the
    // other would send the model to the wrong remedy.
    if envelope_denied(envelope)
        || envelope
            .get("denials")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|d| !d.is_empty())
    {
        return None;
    }

    let prog = named_program(envelope, "command not found: ")?.to_string();
    let granted = granted_host_binaries(exec);

    // The host probe is what makes the two 127 states distinguishable.
    Some(match host_path_lookup(&prog) {
        Some(abs) => format!(
            "error: {prog}: {ABSENT_BINARY_MARKER}.\n  \
             granted host binaries: {granted}\n  \
             For project compiler/test validation, if lifecycle is advertised, prefer lifecycle action=build \
             for the appropriate project phase: it requests explicit offline build authority.\n  \
             For direct execution, ask the operator for exec:{abs}; this does not grant compiler descendants \
             or their filesystem access. Existing permission requirements remain binding; do not retry a declined grant."
        ),
        // Deliberately NO grant coaching here: granting exec for a binary that
        // is not installed is a no-op, and teaching the model to ask for one is
        // exactly the futile loop the denial journal exists to detect.
        None => format!(
            "error: {prog}: {ABSENT_BINARY_MARKER}, and {NOT_ON_HOST_MARKER}.\n  \
             granted host binaries: {granted}\n  \
             no grant can supply it - install it on the host, or use a carried tool."
        ),
    })
}

/// The phrase every absent-binary refusal carries. The loop guidance keys on
/// it (`MISSING_EXECUTABLE_NEEDLES` in `agentic`) to recognise a blocker no
/// edit can clear (#2273); one constant, so renderer and classifier cannot
/// drift — #2277 changed this rendering once and the classifier kept grepping
/// for brush's old `command not found`.
pub(crate) const ABSENT_BINARY_MARKER: &str = "not in this profile's carried userland";

/// The suffix that separates "absent here but installed on the host" (a grant
/// gap) from "not installed at all" (no grant can help). One constant, for the
/// same renderer/classifier drift reason as [`ABSENT_BINARY_MARKER`] (#2304).
pub(crate) const NOT_ON_HOST_MARKER: &str = "not installed on this host";

/// #2273 — the fourth state: the binary exists and no grant refused it, yet
/// the KERNEL did, because the program lives outside the fs-read grant
/// (`~/.cargo/bin` outside the sandbox's read scope is the issue's own
/// transcript). brush reports it as exit 126 with `Permission denied`, which
/// is indistinguishable from a script the model forgot to `chmod +x` — a
/// repairable failure. The gate is STRUCTURED and reads no stderr: exit 126,
/// no `denials`, and the resolved host path is NOT permitted by the read
/// scope. Stderr only chooses WHICH program that path check examines — brush's
/// own error names it first, the leading token is the fallback (see
/// [`failed_program`]). Only then is it rendered in newt's own denial vocabulary
/// so the guidance stops asking for an edit; an ordinary 126 inside the grant
/// falls through untouched.
pub(crate) fn kernel_refused_binary(
    cmd: &str,
    envelope: &serde_json::Value,
    fs_read: &crate::caveats::Scope<String>,
) -> Option<String> {
    if envelope
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        != Some(126)
    {
        return None;
    }
    if envelope_denied(envelope)
        || envelope
            .get("denials")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|d| !d.is_empty())
    {
        return None;
    }
    let prog = named_program(envelope, "failed to execute command '")
        .or_else(|| leading_program(cmd))?
        .to_string();
    let abs = host_path_lookup(&prog)?;
    if crate::caveats::permits_path(fs_read, &abs) {
        return None;
    }
    let dir = std::path::Path::new(&abs)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| abs.clone());
    Some(format!(
        "capability denied: exec of {prog} at {abs} is outside the fs-read \
         grant, so the kernel refused it (exit 126).\n  \
         ask the operator for read:{dir} (and exec:{abs}), or run the host lane."
    ))
}

/// Which program brush failed on, as brush names it in its own error —
/// `command not found: X` for 127, `failed to execute command 'X': ...` for
/// 126. That is the authoritative answer for a compound command, where the
/// leading token (`cd newt-core && cargo test`) is not the one that failed
/// (#2304). The LAST occurrence wins: the exit status belongs to the command
/// that ran last. `None` when brush named nothing: the 126 caller falls back
/// to the leading token for its message; the 127 caller does not, because an
/// unnamed 127 is not an absence (#2315).
///
/// Reading stderr is acceptable HERE and not in [`envelope_denied`] because
/// both callers are already fenced behind the structured exit-code-and-no-
/// denials test, and the model controls `cmd` just as fully: stderr adds no
/// authority the leading token did not already give it. `envelope_denied`
/// decides whether authority was refused; this only picks which program the
/// advisory message is about.
fn named_program<'a>(envelope: &'a serde_json::Value, marker: &str) -> Option<&'a str> {
    envelope
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .and_then(|stderr| stderr.rfind(marker).map(|at| &stderr[at + marker.len()..]))
        .and_then(|rest| rest.split(['\'', '\n']).next())
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// F32/#2537, PR #2577 round 3 (Shawn): when the confined shell is about to
/// run a bare `git …` command in the session's OWN repository on a
/// non-default branch, widen the caveats passed to THIS ONE dispatch with
/// kernel WRITE on the worktree's own gitdir and the common `objects/`
/// directory (`own_gitdir_shell_write_grant` — round 3 narrowed the grant to
/// exactly those two directories; round 4 bound it to the identity resolved
/// at session start rather than a live re-resolve, see that function's doc
/// comment). This unblocks a confined-shell `git add` under the real kernel
/// fence (a file-level Landlock rule cannot carry the
/// `MAKE_REG`/`REFER`/`REMOVE_FILE` rights `index.lock` create+rename and
/// object insertion need — a directory rule can).
///
/// Deliberately scoped to a SIMPLE `git` invocation, not folded into the
/// session `fs_write` scope: `write_file`/`edit_file` on git metadata stay
/// refused (`caveats::apply_cli_fs_grants` never grants this), and a shell
/// `git commit` is refused outright before reaching here
/// (`run_command_creates_shell_git_commit`), so this widening never needs to
/// cover a ref move — `refuse_if_default_branch` in `newt-git` is what
/// prevents that, at the one place a commit can actually land. A compound
/// command (`git add && rm -rf /`) is not "leading program `git`" in the
/// sense that matters here either way — it still runs through the confined
/// engine, which gates each spawn on these SAME (possibly widened) caveats,
/// so a second program in the pipeline gets the same widened `fs_write` too;
/// that is the accepted trade-off Shawn signed off on (a confined-shell
/// `rm -rf <common>/objects` becomes possible on a non-default branch — see
/// `RESULT-dec2-own-gitdir.md`), not an oversight.
///
/// Mirrors the shape a sibling PR's `dispatch_caveats_for_command` (build
/// tools) uses — a separate function, called at the same call site, so the
/// two compose without conflict.
pub(super) fn dispatch_caveats_for_git_shell(
    cmd: &str,
    workspace: &str,
    caveats: &crate::caveats::Caveats,
) -> crate::caveats::Caveats {
    if leading_program(cmd) != Some("git") {
        return caveats.clone();
    }
    // Round 4, Blocker 1: NOT `own_gitdir_grants` — that re-resolves via a
    // live `rev-parse`, which follows model-writable pointers (the workspace
    // `.git` gitlink, `<gitdir>/commondir`). This bounds the write grant to
    // the identity cached at session start.
    let write = crate::git_hardening::own_gitdir_shell_write_grant(std::path::Path::new(workspace));
    if write.is_empty() {
        return caveats.clone();
    }
    let mut widened = caveats.clone();
    widened.fs_write = match &widened.fs_write {
        crate::caveats::Scope::All => crate::caveats::Scope::All,
        crate::caveats::Scope::Only(set) => {
            crate::caveats::Scope::only(set.iter().cloned().chain(write))
        }
    };
    // Real git ALWAYS tries to read the system config (`/etc/gitconfig`),
    // regardless of repo/branch — not just on a host that happens to have
    // one. Landlock's base read allowlist does not include it (CI caught
    // this: a Landlock-confined `git add` on a runner that ships
    // `/etc/gitconfig` failed with "unknown error occurred while reading
    // the configuration files", exit 128 — this environment's sandbox
    // silently worked only because it has no such file). One extra,
    // non-secret, well-known system path, granted only on this ONE widened
    // dispatch, same as the write grant above.
    widened.fs_read = match &widened.fs_read {
        crate::caveats::Scope::All => crate::caveats::Scope::All,
        crate::caveats::Scope::Only(set) => crate::caveats::Scope::only(
            set.iter()
                .cloned()
                .chain(std::iter::once("/etc/gitconfig".to_string())),
        ),
    };
    widened
}

/// The leading program of `cmd`: `FOO=bar prog ...` - an env assignment is not
/// the program.
pub(super) fn leading_program(cmd: &str) -> Option<&str> {
    cmd.split_ascii_whitespace().find(|tok| !tok.contains('='))
}

/// dec1-build-grant round 2 (Reviewer FIX-FIRST, PR #2579): the caveats for
/// ONE confined-shell dispatch — `caveats` as-is, unless `cmd`'s leading
/// program is a build tool (`confined_exec::is_build_tool_exec`), in which
/// case its toolchain read roots (`confined_exec::toolchain_read_roots`) are
/// added to `fs_read`. Call-scoped and NEVER returned to the permission
/// gate: `widen_caveats` deliberately leaves an exec grant's `fs_read`
/// untouched, because its result feeds `recalled_caveats` — the caveats
/// checked for every tool the model calls this session, including
/// `read_file`. Widening the SESSION's `fs_read` from an `exec:cargo` grant
/// would let `read_file` read `$CARGO_HOME/credentials.toml` for the rest of
/// the session; widening only this one dispatch's caveats lets the SPAWNED
/// cargo process resolve its own toolchain and nothing else gains the read.
fn dispatch_caveats_for_command(
    cmd: &str,
    caveats: &crate::caveats::Caveats,
) -> crate::caveats::Caveats {
    let mut widened = caveats.clone();
    if let crate::caveats::Scope::Only(exec) = &mut widened.exec {
        let twins = crate::confined_exec::developer_exec_twins(exec.iter());
        exec.extend(twins);
    }
    if leading_program(cmd).is_some_and(crate::confined_exec::is_build_tool_exec) {
        if let crate::caveats::Scope::Only(reads) = &mut widened.fs_read {
            reads.extend(crate::confined_exec::toolchain_read_roots());
        }
    }
    widened
}

/// The exec grants in force, for the refusal's second line. Naming what IS
/// granted turns "no" into a question the operator can answer in one line.
fn granted_host_binaries(exec: &crate::caveats::Scope<String>) -> String {
    match exec {
        crate::caveats::Scope::All => "(unrestricted)".to_string(),
        crate::caveats::Scope::Only(set) if set.is_empty() => "(none)".to_string(),
        crate::caveats::Scope::Only(set) => set
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// Resolve `prog` against the REAL host PATH.
///
/// Sound because newt's own process is never Landlocked - the fence is applied
/// on the spawning thread immediately before the child starts (agent-bridle's
/// `ConfinedCommand::spawn_authorized`), never to newt itself. This therefore
/// answers "is it installed on this host", which is exactly the question the
/// confined child cannot answer about itself.
fn host_path_lookup(prog: &str) -> Option<String> {
    fn executable(p: &std::path::Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            p.is_file()
        }
    }

    // An explicit path is already the answer; PATH is not consulted for it.
    if prog.contains('/') || prog.contains('\\') {
        return executable(std::path::Path::new(prog)).then(|| prog.to_string());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(prog))
        .find(|cand| executable(cand))
        .map(|p| p.display().to_string())
}

/// The standard `run_command` success path: return stdout/stderr, or `(exit N)`
/// when the command produced no output. Factored verbatim so
/// the #263 re-execution path shares one formatter with the first dispatch.
pub(super) fn shell_envelope_output(
    envelope: &serde_json::Value,
    _tool_output_lines: usize,
    _color: bool,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    presentation: Option<&mut dyn ToolPresentation>,
) -> String {
    let stdout = envelope
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let stderr = envelope
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let out = format!("{stdout}{stderr}");
    // #1969: the exit code decides success, not the shape of the output.
    //
    // This used to be consulted ONLY when the output was empty, so every
    // command that failed LOUDLY — which is every failing compile — returned
    // a non-empty string with no failure marker and was classified by
    // `tool_result_ok`'s prefix test as a success. Three consumers read that
    // one bit: the turn's `ToolEvent` ledger, `RepeatCallGuard` (which
    // memoizes a `Failure` only for `!ok`, so the per-run steer never fired
    // on a repeated failing build), and `loop_watch::repeated_failure`.
    //
    // The marker is a prefix rather than a suffix because that is what
    // `tool_result_ok` reads, and it names the code because "it failed" with
    // no evidence is the claim this repo keeps refusing to accept elsewhere.
    // The diagnostics follow it untouched — the model still needs them.
    let code = envelope
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(-1);
    let mark_failure = |payload: String| {
        if code == 0 {
            payload
        } else {
            format!("error: command exited {code}\n{payload}")
        }
    };
    if out.trim().is_empty() {
        // A failing command that printed nothing needs no second rendering of
        // its own code: the marker already carries it.
        return if code == 0 {
            format!("(exit {code})")
        } else {
            format!("error: command exited {code}")
        };
    }
    {
        // The terminal follows the full output tail even when the model-facing
        // payload below is token-capped or replaced by a spill handle.
        if let Some(presentation) = presentation {
            presentation.override_result(out.clone());
        }
        // #726/#945: the MODEL-facing payload is capped by the shared TOKEN
        // budget using head+tail. When tool_offload is on, spill the FULL
        // redacted output before capping so the true tail and elided middle stay
        // recoverable via memory_fetch("spill:<id>") and grep.
        //
        // The spill decision sizes with the SAME conservative estimator the cap
        // uses (via `should_spill_full_output` → `cap_estimator`, not the 4 c/t
        // context default) — otherwise output between the conservative cap
        // (3 c/t) and the looser default (4 c/t) gets head/tail-truncated by the
        // cap yet judged "under budget" by the spill gate, so its elided middle
        // is never spilled and becomes unrecoverable. One shared owner keeps
        // "will the cap truncate?" and "should we spill?" from ever diverging.
        let max_tokens = max_output_tokens();
        let est = output_budget::cap_estimator();
        let should_spill = output_budget::should_spill_full_output(
            out.len(),
            out.chars().count(),
            max_tokens,
            tool_offload,
        );
        let capped = if should_spill {
            match spill_store {
                Some(store) => {
                    let (id, redacted) = content_spill::store_redacted_full(
                        &out,
                        Some("run_command".to_string()),
                        store,
                    );
                    let teaser_tokens = est
                        .tokens_for_chars(content_spill::TOOL_RESULT_SPILL_CAP.saturating_sub(512));
                    match id {
                        // Committed: cap with the `spill:<id>` retrieval handle.
                        Some(id) => cap_model_output_with_handle(
                            &redacted,
                            max_tokens.min(teaser_tokens),
                            output_head_tokens(),
                            Some(&id),
                        ),
                        // Commit failed: fail closed — cap the redacted output with
                        // NO handle rather than promise a `spill:<id>` that resolves
                        // to nothing (BHV-SPILL-001).
                        None => cap_model_output(&redacted, max_tokens),
                    }
                }
                None => cap_model_output(&out, max_tokens),
            }
        } else {
            cap_model_output(&out, max_tokens)
        };
        // #898: if this command's output carries a forge "open a pull/merge
        // request" URL (git prints it on push of a new branch), append an
        // explicit next-step hint so the model opens the PR instead of stalling.
        // Detected from the UNcapped output so a long push log can't truncate the
        // URL away, and appended AFTER the cap so the hint always survives.
        mark_failure(match pr_creation_url(&out) {
            Some(url) => format!("{capped}{}", pr_next_step_hint(url)),
            None => capped,
        })
    }
}

/// #898: the forge "open a pull/merge request" URL that git prints on `push` of
/// a new branch — GitHub `…/pull/new/<branch>` (or a `…/compare/…` link) and
/// GitLab `…/merge_requests/new…`. Returned so [`shell_envelope_output`] can
/// append a next-step hint: models routinely push and then stall instead of
/// opening the PR (issue #898). Scans whitespace-split tokens because git emits
/// the URL on its own `remote:`-prefixed line.
pub(super) fn pr_creation_url(output: &str) -> Option<&str> {
    output.split_whitespace().find(|tok| {
        tok.starts_with("https://")
            && (tok.contains("/pull/new/")
                || tok.contains("/merge_requests/new")
                || tok.contains("/compare/"))
    })
}

/// The next-step hint appended after a push whose output carries a PR-creation
/// URL (#898). Names the concrete `gh` command AND the tool boundary — the
/// embedded `git` tool cannot push or open PRs — so the model proceeds through
/// run_command + `gh` instead of looping back to the pushless git tool.
fn pr_next_step_hint(url: &str) -> String {
    format!(
        "\n\n[newt] A branch was pushed. To open a pull request now, call \
         run_command with `gh pr create --fill` (the `gh` CLI is available; the \
         `git` tool cannot push or open PRs). Or open this URL: {url}"
    )
}

/// Lift a confined-shell denial envelope into promptable #263 requests.
///
/// Returns `Some` only when EVERY structured denial entry is an `exec` kind
/// with a non-empty target — the case the human can meaningfully grant (the
/// exact executable target). Any other kind
/// (e.g. an `open` refused inside the shell) keeps the standard denial:
/// guessing which fs axis an opaque `open` maps to would over-grant.
/// #1150: a STRUCTURAL refusal is a can't, not a may-not — the confined shell
/// engine cannot interpret the construct (`$(...)`, backgrounding `&`, heredocs,
/// fd duplication), so NO grant unlocks it. Offering "allow once / session /
/// always" for one is a grant→denial contradiction that destroys trust in the
/// whole permission loop (the operator grants, the engine denies anyway). We
/// detect it by the stable markers agent-bridle's `Refusal::Display` emits, so
/// these fall through to the plain denial (which already names the --yolo
/// escape) with no grant menu.
fn is_structural_refusal(reason: &str) -> bool {
    reason.contains("refused by design:")
        || reason.contains("dynamic construct the confined shell")
        || reason.contains("not yet supported by the confined shell")
}

pub(super) fn exec_denial_requests(envelope: &serde_json::Value) -> Option<Vec<PermissionRequest>> {
    let denials = envelope.get("denials")?.as_array()?;
    if denials.is_empty() {
        return None;
    }
    let mut requests = Vec::with_capacity(denials.len());
    for d in denials {
        if d.get("kind")?.as_str()? != "exec" {
            return None;
        }
        // A structural refusal anywhere in the batch: grants are meaningless,
        // so surface the plain denial for the whole call (#1150).
        if d.get("reason")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_structural_refusal)
        {
            return None;
        }
        let target = d.get("target")?.as_str().filter(|t| !t.is_empty())?;
        requests.push(PermissionRequest {
            tool: "run_command".to_string(),
            kind: DenialKind::Exec,
            target: target.to_string(),
            reason: d
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    Some(requests)
}

/// #905: lift a confined-shell NET denial envelope into promptable #263 requests
/// — the `net`-axis sibling of [`exec_denial_requests`]. agent-bridle #196
/// surfaces a refused CONNECT host as `Denial { kind: "net", target: <host> }`
/// (with `denied: true`), so when the operator's `net` allow-list refuses a host
/// the shell reached (e.g. `git push` to `github.com`), this turns each into a
/// `PermissionRequest { kind: Net, target: host }` the gate can prompt per-host.
///
/// Returns `Some` only when EVERY denial is a `net` kind with a non-empty host
/// target — the case a grant is meaningful (add the host to the net allow-list).
/// A mixed or non-net batch returns `None` and keeps the standard denial.
pub(super) fn net_denial_requests(envelope: &serde_json::Value) -> Option<Vec<PermissionRequest>> {
    let denials = envelope.get("denials")?.as_array()?;
    if denials.is_empty() {
        return None;
    }
    let mut requests = Vec::with_capacity(denials.len());
    for d in denials {
        if d.get("kind")?.as_str()? != "net" {
            return None;
        }
        let host = d.get("target")?.as_str().filter(|t| !t.is_empty())?;
        requests.push(PermissionRequest {
            tool: "run_command".to_string(),
            kind: DenialKind::Net,
            target: host.to_string(),
            reason: d
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    Some(requests)
}

#[cfg(test)]
mod dispatch_caveats_tests {
    use super::*;
    use crate::caveats::{Caveats, CaveatsExt as _, CountBound, Scope};

    fn base(ws: &str) -> Caveats {
        Caveats {
            fs_read: Scope::only([ws.to_string()]),
            fs_write: Scope::only([ws.to_string()]),
            exec: Scope::only(["cargo".to_string()]),
            net: Scope::none(),
            max_calls: CountBound::Unlimited,
            valid_for_generation: Scope::All,
        }
    }

    /// dec1-build-grant round 2, red test (b): the shell-lane dispatch
    /// caveats for a build-tool command include its toolchain read root —
    /// call-scoped, so `read_file` (which checks the SESSION's caveats, not
    /// this dispatch's) never sees it. Would fail before the fix:
    /// `dispatch_caveats_for_command` did not exist; `dispatch_bridled_shell`
    /// passed the raw session `caveats` straight through, unable to read
    /// `$RUSTUP_HOME`.
    #[test]
    fn a_build_tool_command_gets_its_toolchain_read_root_for_this_dispatch_only() {
        // Pins the toolchain-home resolution so the
        // assertion does not depend on the machine running the suite.
        // Platform-absolute (a POSIX `/fake-home` is not absolute on Windows),
        // and under the process-env lock like every other env-pinning test.
        let _env = crate::process_env::lock();
        let fake = std::env::temp_dir().join("fake-home").join(".rustup");
        let fake = fake.to_string_lossy().into_owned();
        let saved = std::env::var_os("RUSTUP_HOME");
        crate::process_env::set_var("RUSTUP_HOME", &fake);
        let widened = dispatch_caveats_for_command("cargo --version", &base("/ws"));
        match saved.and_then(|v| v.into_string().ok()) {
            Some(v) => crate::process_env::set_var("RUSTUP_HOME", &v),
            None => crate::process_env::remove_var("RUSTUP_HOME"),
        }
        assert!(
            widened.permits_fs_read(&fake),
            "a build-tool dispatch must be able to read its toolchain home"
        );
    }

    /// A non-build-tool command is passed through unchanged — no toolchain
    /// roots leak into an ordinary `run_command` dispatch.
    #[test]
    fn a_non_build_tool_command_is_unchanged() {
        let base = base("/ws");
        let widened = dispatch_caveats_for_command("ls -la", &base);
        assert_eq!(widened.fs_read, base.fs_read);
    }
}
