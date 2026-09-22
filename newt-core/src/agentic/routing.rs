//! Hidden tool-call routing — facade **P4** (§4 of
//! `docs/ocap/permissions-facade-design.md`).
//!
//! Capable models emit tool calls learned from *other* harnesses —
//! `run_command("cat X")`, `run_command("ls")`, `run_command("find … ")`,
//! `run_command("rm X")`, `run_command("git status")` — instead of newt's
//! governed built-ins (`read_file` / `list_dir` / `find` / `delete_file` / the
//! embedded `git` tool). Each such reach lands as a wasted round, and worse, an
//! operation the model could have done *within authority* can trip an **exec**
//! denial when it arrives as a shell command (§4.1).
//!
//! P4 **promotes faithful one-tool reaches to a silent rewrite**: the call is
//! transparently re-dispatched to the OCAP-governed built-in (no model
//! retraining), and **everything else is gated** as ordinary exec.
//!
//! ## Routing is NOT a bypass (§4.4)
//!
//! A routed call goes through the **same** fs / git caveat checks the built-in
//! always runs — [`RouteDecision::Route`] only changes *which built-in serves
//! the call*, never *whether it is within authority*. An out-of-scope
//! `cat /etc/shadow` routes to `read_file{path:"/etc/shadow"}` and is denied by
//! the fs floor exactly as a direct `read_file` would be. Routing is the L2
//! convenience engine; the L3 boundary (the confined shell, the fs fence) is
//! untouched, and is **never** disabled by the routing escape (§7-F5 — see
//! `tools::routing_disabled`, a switch distinct from `--disable-ocap`).
//!
//! ## The route/gate split is DATA, not `match` arms (three-Cs)
//!
//! Per the repo's language-pack / lexicon convention (`CLAUDE.md` → "the three
//! Cs"), the knowledge of *which* shell reaches route, and *which git
//! subcommands are read-only*, lives in pure data — the [`SHELL_ROUTES`] slice
//! and the [`GIT_READ_ONLY_SUBCOMMANDS`] set — read by [`RouteTable::classify`],
//! a pure function (no fs, no env, no I/O). A new read reach, or a newly
//! read-only git subcommand, is a **data edit** (and a future drop-in
//! `[tui.permissions]` override), never a logic change.

use serde_json::{json, Value};

/// What to do with a shell command the model passed to `run_command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RouteDecision {
    /// Silently route to this governed built-in with these translated args. The
    /// built-in applies the SAME fs / git caveat checks — routing is not a
    /// bypass.
    Route { tool: &'static str, args: Value },
    /// Leave the command on the normal exec path (the confined shell / #263
    /// permission gate). Read-only reaches we cannot faithfully represent, and
    /// every state-modifying or unknown command, land here.
    Exec,
}

/// A whole-command shell reach that maps cleanly onto a governed built-in.
/// Pure DATA (three-Cs): a new reach is one slice entry.
#[derive(Debug)]
struct ShellRoute {
    /// The shell program the model typed (the leading token).
    program: &'static str,
    /// The governed built-in it routes to.
    tool: &'static str,
}

/// The shell reaches that route to a governed built-in — pure DATA.
///
/// `cat`→`read_file`, `ls`→`list_dir`, `find`→the embedded `find` tool,
/// `rm`/`unlink`→`delete_file`. The per-tool argument translation (how the
/// command's operands become the built-in's `{path}` / `{name}` / `{type}`) is
/// the pure rule in [`build_shell_route`]; *which* programs route is this slice.
const SHELL_ROUTES: &[ShellRoute] = &[
    ShellRoute {
        program: "cat",
        tool: "read_file",
    },
    ShellRoute {
        program: "ls",
        tool: "list_dir",
    },
    ShellRoute {
        program: "find",
        tool: "find",
    },
    ShellRoute {
        program: "rm",
        tool: "delete_file",
    },
    ShellRoute {
        program: "unlink",
        tool: "delete_file",
    },
];

/// `cargo` subcommands that route to the confined build lane (`build_exec`,
/// `tools.rs`) — pure DATA. Deliberately excludes `run`/`install`/`publish`
/// and anything else that changes what's installed or reaches the network;
/// those stay on the exec path.
const CARGO_BUILD_SUBCOMMANDS: &[&str] = &["build", "check", "test", "clippy"];

/// The git subcommands that are **read-only** and route to the embedded `git`
/// tool's read path — pure DATA.
///
/// These map one-to-one onto the embedded git tool's read ops
/// (`status`/`log`/`diff`). State-modifying subcommands (`add`, `stash`,
/// `checkout`, `reset`, `commit`, `push`, `amend`, `rebase`, `branch-delete`,
/// …) are **NOT** here: they GATE as exec (owner decision 2). `show` has no
/// embedded read op yet. `branch` mixes reads and mutations, so it uses the
/// exact argument translation in [`branch_list_route`] instead of this set.
const GIT_READ_ONLY_SUBCOMMANDS: &[&str] = &["status", "log", "diff"];

/// Shell control / redirection / substitution metacharacters. A command
/// containing any of these is **compound** (`cat f | grep x`, `cat a && cat b`,
/// `cat $(…)`, a redirect) — its semantics cannot be reproduced by a single
/// built-in call, so it is never routed; the confined shell gates each spawn.
/// Mirrors `tools::exec_floor_permits`'s conservative refusal. Globbing
/// (`* ? [ ]`) is handled per-program in [`build_read_route`], not here, because
/// `find -name '*.rs'` globs *inside* the tool.
const SHELL_META: &[char] = &['&', '|', ';', '`', '$', '\n', '>', '<', '(', ')'];

/// Glob metacharacters in a bare path operand. A `cat *.txt` / `ls a?` needs
/// the *shell* to expand the glob into filenames — a built-in receiving the
/// literal glob would misbehave — so such a command gates instead of routing.
const GLOB: &[char] = &['*', '?', '[', ']'];

/// Tokens the SHELL would transform before a program ever sees them — a
/// quote, an escape, a `~` expansion, or a glob (`GLOB` above). The build
/// route runs the model's argv **literally** (no shell in between), so any
/// operand carrying one of these routes with the WRONG argv: `cargo test
/// "a b"` would see two literal tokens `"a` and `b"` instead of one quoted
/// string, `--manifest-path ~/x` would see a literal `~` instead of $HOME,
/// `cargo build --bin *` would see a literal `*` instead of the shell's
/// expansion. #2533 round 2: refuse to route rather than run a silently
/// different command with `ok: true`.
const BUILD_UNSAFE: &[char] = &['"', '\'', '\\', '~', '*', '?', '[', ']'];

/// The route/gate table — pure DATA, read by [`RouteTable::classify`].
#[derive(Debug, Clone)]
pub(crate) struct RouteTable {
    shell_routes: &'static [ShellRoute],
    git_read_only: &'static [&'static str],
}

impl RouteTable {
    /// The built-in table: the [`SHELL_ROUTES`] reaches plus the
    /// [`GIT_READ_ONLY_SUBCOMMANDS`] set. Composed from data (three-Cs) so a
    /// future `[tui.permissions]` override layers on the same shape.
    #[must_use]
    pub(crate) fn builtin() -> Self {
        Self {
            shell_routes: SHELL_ROUTES,
            git_read_only: GIT_READ_ONLY_SUBCOMMANDS,
        }
    }

    /// Classify a complete call without discarding argument semantics on the
    /// newly scoped Git read route. Other routes retain their existing policy.
    #[must_use]
    pub(crate) fn classify_call(&self, call: &Value) -> RouteDecision {
        let decision = self.classify(call.get("command").and_then(Value::as_str).unwrap_or(""));
        // A scoped git read and a build-lane route both discard the rest of
        // the call object once routed — neither has anywhere to put a `cwd`
        // or `timeout` the model also sent — so both require the call be
        // *bare* (`command` only). A call carrying anything else stays Exec
        // rather than silently dropping that field.
        let requires_bare_call = matches!(
            &decision,
            RouteDecision::Route { tool: "git", args }
                if args.get("op").and_then(Value::as_str)
                    .is_some_and(super::git_tool::is_scoped_read_op)
        ) || matches!(
            &decision,
            RouteDecision::Route {
                tool: "build_exec",
                ..
            }
        );
        if requires_bare_call
            && !call
                .as_object()
                .is_some_and(|args| args.keys().all(|key| key == "command"))
        {
            RouteDecision::Exec
        } else {
            decision
        }
    }

    /// Classify a `run_command` shell-command string. **Pure** — no fs, no env,
    /// no I/O — so it is a direct table lookup (TDD: data-driven decision).
    #[must_use]
    pub(crate) fn classify(&self, command: &str) -> RouteDecision {
        let command = command.trim();
        if command.is_empty() {
            return RouteDecision::Exec;
        }
        // Compound / redirected / substituted commands never route (see
        // SHELL_META): a single built-in cannot reproduce a pipe/chain/redirect.
        if command.contains(SHELL_META) {
            return RouteDecision::Exec;
        }
        let mut tokens = command.split_ascii_whitespace();
        let Some(program) = tokens.next() else {
            return RouteDecision::Exec;
        };

        // Read-only VCS reaches route BY SUBCOMMAND (the route/gate split is
        // DATA). A read-only subcommand routes to the embedded git tool's read
        // path; a state-modifying / unknown / absent subcommand GATES as exec.
        if program == "git" {
            let sub = tokens.next();
            if sub == Some("branch") {
                return branch_list_route(&tokens.collect::<Vec<_>>());
            }
            return match sub {
                Some(sub) if self.git_read_only.contains(&sub) => {
                    git_read_route(sub, &tokens.collect::<Vec<_>>())
                }
                _ => RouteDecision::Exec,
            };
        }

        // A recognised build/test invocation routes to the confined build
        // lane — the model's literal argv runs verbatim (see
        // `build_lane_route`'s doc comment): F11's fix for the model that
        // never calls `lifecycle` and instead times out under
        // `run_command`'s 60s wall.
        if program == "cargo" || program == "just" {
            let rest: Vec<&str> = tokens.collect();
            return build_lane_route(program, &rest);
        }

        // Whole-command reaches that map onto governed built-ins.
        let Some(route) = self.shell_routes.iter().find(|r| r.program == program) else {
            return RouteDecision::Exec;
        };
        let rest: Vec<&str> = tokens.collect();
        build_shell_route(route.tool, &rest)
    }
}

/// Translate `status` / `log` / `diff` operands into the embedded git tool's
/// args — same rule as [`branch_list_route`]: only list shapes whose
/// namespace the embedded op can preserve; an unmodeled flag GATES as exec
/// rather than silently running the bare op on a request that asked for
/// something narrower (item 1 of the routing-honesty job: `git log A..B`
/// used to answer with the last 20 commits from HEAD, and `git diff --cached`
/// used to answer with the unstaged diff instead).
///
/// A revision/pathspec/`--stat` shape now routes too (the engine serves
/// them); a near-miss that changes the *presentation* the engine cannot
/// reproduce (`--word-diff`, `-p` with a range, `--oneline`, …) still gates.
fn git_read_route(sub: &str, rest: &[&str]) -> RouteDecision {
    match sub {
        "status" if rest.is_empty() => RouteDecision::Route {
            tool: "git",
            args: json!({ "op": "status" }),
        },
        "log" => log_route(rest),
        "diff" => diff_route(rest),
        _ => RouteDecision::Exec,
    }
}

/// `git log`'s routable shapes: bare, `-N`/`-n N` limit, a single revision or
/// `A..B` range, and an optional `-- <paths>` pathspec tail, any of which may
/// combine. Any other flag (`--oneline`, `-p`, `--author=…`, …) gates.
fn log_route(rest: &[&str]) -> RouteDecision {
    let mut limit = None;
    let mut i = 0;
    if let Some(&"-n") = rest.first() {
        let Some(n) = rest.get(1).and_then(|n| n.parse::<u64>().ok()) else {
            return RouteDecision::Exec;
        };
        limit = Some(n);
        i = 2;
    } else if let Some(flag) = rest.first() {
        if let Some(n) = parse_log_limit(flag) {
            limit = Some(n);
            i = 1;
        }
    }
    let revision = match rest.get(i) {
        Some(tok) if *tok != "--" && !tok.starts_with('-') => {
            i += 1;
            Some(*tok)
        }
        _ => None,
    };
    let paths = match rest.get(i) {
        Some(&"--") => &rest[i + 1..],
        None => &[][..],
        Some(_) => return RouteDecision::Exec,
    };
    if paths.iter().any(|p| p.is_empty()) {
        return RouteDecision::Exec;
    }
    let mut args = serde_json::Map::new();
    args.insert("op".into(), json!("log"));
    if let Some(limit) = limit {
        args.insert("limit".into(), json!(limit));
    }
    if let Some(rev) = revision {
        args.insert("revision".into(), json!(rev));
    }
    if !paths.is_empty() {
        args.insert("paths".into(), json!(paths));
    }
    RouteDecision::Route {
        tool: "git",
        args: Value::Object(args),
    }
}

/// `git diff`'s routable shapes: bare, `--cached`/`--staged`, zero/one/two
/// revisions, an optional `--stat`, and an optional `-- <paths>` pathspec
/// tail, any of which may combine. Any other flag (`--word-diff`, `-p`,
/// `--name-only`, …) gates.
fn diff_route(rest: &[&str]) -> RouteDecision {
    let stat = rest.contains(&"--stat");
    let rest: Vec<&str> = rest.iter().copied().filter(|t| *t != "--stat").collect();
    let rest = rest.as_slice();
    if let [flag] = rest {
        if *flag == "--cached" || *flag == "--staged" {
            let mut args = serde_json::Map::new();
            args.insert("op".into(), json!("diff"));
            args.insert("spec".into(), json!("staged"));
            if stat {
                args.insert("stat".into(), json!(true));
            }
            return RouteDecision::Route {
                tool: "git",
                args: Value::Object(args),
            };
        }
    }
    let mut i = 0;
    let mut revs: Vec<&str> = Vec::new();
    while revs.len() < 2 {
        match rest.get(i) {
            Some(tok) if *tok != "--" && !tok.starts_with('-') => {
                revs.push(tok);
                i += 1;
            }
            _ => break,
        }
    }
    let paths = match rest.get(i) {
        Some(&"--") => &rest[i + 1..],
        None => &[][..],
        Some(_) => return RouteDecision::Exec,
    };
    if paths.iter().any(|p| p.is_empty()) {
        return RouteDecision::Exec;
    }
    let mut args = serde_json::Map::new();
    args.insert("op".into(), json!("diff"));
    match revs.as_slice() {
        [] => {}
        [a] => {
            args.insert("rev".into(), json!(a));
        }
        [a, b] => {
            args.insert("rev".into(), json!(a));
            args.insert("rev2".into(), json!(b));
        }
        _ => unreachable!("capped at 2 by the while loop"),
    }
    if stat {
        args.insert("stat".into(), json!(true));
    }
    if !paths.is_empty() {
        args.insert("paths".into(), json!(paths));
    }
    RouteDecision::Route {
        tool: "git",
        args: Value::Object(args),
    }
}

/// Parse `git log`'s count shorthand: `-n N` is two tokens, `-N` is one. Only
/// a bare non-negative integer count is modeled; anything else (a range, a
/// path, `--oneline`, …) is not representable by `limit` alone, so it gates.
fn parse_log_limit(flag: &str) -> Option<u64> {
    flag.strip_prefix("-n")
        .or_else(|| flag.strip_prefix('-'))
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
}

/// Only list shapes whose namespace the embedded operation can preserve.
/// Never drop a branch operand, mutation flag, or unsupported result filter.
fn branch_list_route(rest: &[&str]) -> RouteDecision {
    let scope = match rest {
        [] | ["--list"] => "local",
        [flag] | ["--list", flag] | [flag, "--list"] => match *flag {
            "-a" | "--all" => "all",
            "-r" | "--remotes" => "remote",
            _ => return RouteDecision::Exec,
        },
        _ => return RouteDecision::Exec,
    };
    RouteDecision::Route {
        tool: "git",
        args: json!({ "op": "branch-list", "scope": scope }),
    }
}

/// A `run_command` reach that maps onto the confined **build lane**
/// (`build_exec` in `tools.rs`, sharing `run_confined_build_lane` with
/// `lifecycle action=build`) rather than a governed read built-in. Unlike
/// every other route in this table, the routed call runs the model's
/// **literal argv verbatim** — never a re-resolved phase command — so no
/// operand (`-p x`, a test filter, …) is ever silently dropped. `cwd`/
/// `timeout` on the call are handled the same way the scoped git-read route
/// handles them: [`RouteTable::classify_call`] refuses to route a call
/// carrying either, rather than dropping them.
fn build_lane_route(program: &str, rest: &[&str]) -> RouteDecision {
    // See `BUILD_UNSAFE`: a quoted, escaped, `~`-relative or globbed operand
    // cannot be routed faithfully (there is no shell downstream to expand
    // it), so gate the whole call to exec rather than run a mangled argv.
    if rest.iter().any(|tok| tok.contains(BUILD_UNSAFE)) {
        return RouteDecision::Exec;
    }
    match program {
        "cargo" => cargo_build_route(rest),
        "just" => just_build_route(rest),
        _ => RouteDecision::Exec,
    }
}

/// `cargo build|check|test|clippy [args…]`. `--config`/`-Z…` change what
/// cargo does (config override / unstable flags outside the calibrated
/// fence) and never route — same "never widen, never drop" posture as the
/// git read routes.
fn cargo_build_route(rest: &[&str]) -> RouteDecision {
    let Some((sub, rest)) = rest.split_first() else {
        return RouteDecision::Exec;
    };
    if !CARGO_BUILD_SUBCOMMANDS.contains(sub) {
        return RouteDecision::Exec;
    }
    if rest
        .iter()
        .any(|tok| *tok == "--config" || tok.starts_with("--config=") || tok.starts_with("-Z"))
    {
        return RouteDecision::Exec;
    }
    let mut argv = vec!["cargo".to_string(), (*sub).to_string()];
    argv.extend(rest.iter().map(|t| (*t).to_string()));
    RouteDecision::Route {
        tool: "build_exec",
        args: json!({ "argv": argv }),
    }
}

/// `just <recipe>` — a single non-flag recipe name, no other operands.
/// Whether a justfile actually exists is a filesystem fact this pure
/// classifier cannot see; the dispatch site (`tools.rs`'s `build_exec` arm)
/// checks it before running and refuses if there is none.
fn just_build_route(rest: &[&str]) -> RouteDecision {
    match rest {
        [recipe] if !recipe.starts_with('-') => RouteDecision::Route {
            tool: "build_exec",
            args: json!({ "argv": ["just", recipe] }),
        },
        _ => RouteDecision::Exec,
    }
}

/// Translate a whole-command shell reach's operands into the governed built-in's
/// argument shape — the pure per-tool rule. Returns [`RouteDecision::Exec`]
/// whenever the shape is ambiguous (so a routed call is always faithful to the
/// command the model typed).
fn build_shell_route(tool: &'static str, rest: &[&str]) -> RouteDecision {
    match tool {
        // read_file reads exactly ONE file. Zero or many operands, or a glob
        // (which the shell would expand to a set), are ambiguous → gate.
        "read_file" => match operands(rest).as_slice() {
            [path] if !path.contains(GLOB) => RouteDecision::Route {
                tool,
                args: json!({ "path": path }),
            },
            _ => RouteDecision::Exec,
        },
        // list_dir lists ONE directory; a bare `ls` is the workspace (".").
        "list_dir" => match operands(rest).as_slice() {
            [] => RouteDecision::Route {
                tool,
                args: json!({ "path": "." }),
            },
            [path] if !path.contains(GLOB) => RouteDecision::Route {
                tool,
                args: json!({ "path": path }),
            },
            _ => RouteDecision::Exec,
        },
        "find" => build_find_route(rest),
        "delete_file" => build_delete_route(rest),
        // Unreachable for SHELL_ROUTES, but keep the rule total.
        _ => RouteDecision::Exec,
    }
}

/// The non-flag operands of a command's argument list (tokens not starting with
/// `-`). Flags like `cat -n` / `ls -la` are dropped — the built-in renders the
/// content/listing regardless.
fn operands<'a>(rest: &[&'a str]) -> Vec<&'a str> {
    rest.iter()
        .copied()
        .filter(|t| !t.starts_with('-'))
        .collect()
}

/// Translate `rm [-f] <path>` / `unlink <path>` into `delete_file`.
/// Recursive/tree deletes, multiple operands, globs, and unknown flags stay on
/// the exec path, where the absolute deny-list refuses them before the shell.
fn build_delete_route(rest: &[&str]) -> RouteDecision {
    let mut operands = Vec::new();
    let mut end_of_flags = false;
    for token in rest {
        if !end_of_flags && *token == "--" {
            end_of_flags = true;
            continue;
        }
        if !end_of_flags && token.starts_with('-') {
            if token.chars().skip(1).all(|c| c == 'f') {
                continue;
            }
            return RouteDecision::Exec;
        }
        operands.push(*token);
    }
    match operands.as_slice() {
        [path] if !path.contains(GLOB) => RouteDecision::Route {
            tool: "delete_file",
            args: json!({ "path": path }),
        },
        _ => RouteDecision::Exec,
    }
}

/// Translate a `find [path] [-name PAT] [-type f|d]` reach into the embedded
/// `find` tool's args. Any predicate the embedded tool does not model (e.g.
/// `-newer`, `-exec`) gates, so a routed `find` never silently drops a filter
/// that would change the result set.
fn build_find_route(rest: &[&str]) -> RouteDecision {
    let mut path = ".";
    let mut name: Option<&str> = None;
    let mut type_filter: Option<&str> = None;

    // A leading non-flag token is the search root.
    let mut i = 0;
    if let Some(first) = rest.first() {
        if !first.starts_with('-') {
            path = first;
            i = 1;
        }
    }
    while i < rest.len() {
        match rest[i] {
            "-name" | "-iname" => match rest.get(i + 1).copied() {
                Some(v) => {
                    name = Some(strip_quotes(v));
                    i += 2;
                }
                None => return RouteDecision::Exec,
            },
            "-type" => match rest.get(i + 1).copied() {
                Some(v @ ("f" | "d")) => {
                    type_filter = Some(v);
                    i += 2;
                }
                _ => return RouteDecision::Exec,
            },
            // An unmodeled predicate → let the shell's `find` handle it.
            _ => return RouteDecision::Exec,
        }
    }
    let mut args = serde_json::Map::new();
    args.insert("path".into(), json!(path));
    if let Some(n) = name {
        args.insert("name".into(), json!(n));
    }
    if let Some(t) = type_filter {
        args.insert("type".into(), json!(t));
    }
    RouteDecision::Route {
        tool: "find",
        args: Value::Object(args),
    }
}

/// Strip a single pair of matching surrounding quotes (`'…'` or `"…"`) from a
/// token, so a shell-quoted `-name '*.rs'` yields the bare `*.rs` glob the
/// embedded `find` tool expects.
fn strip_quotes(token: &str) -> &str {
    let bytes = token.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'\'' || bytes[0] == b'"')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        &token[1..token.len() - 1]
    } else {
        token
    }
}

/// The audit line for a routing decision — **pure** and testable, the single
/// source of truth for the §4.4 audit log (`tools::execute_tool` emits this
/// via `tracing::debug!` on every silent rewrite). Returns `None` for an
/// [`RouteDecision::Exec`] (nothing was rewritten, nothing to log).
#[must_use]
pub(crate) fn audit_line(original: &str, decision: &RouteDecision) -> Option<String> {
    match decision {
        RouteDecision::Route { tool, args } => Some(format!(
            "facade P4 routing: rewrote `{original}` → {tool} {args}"
        )),
        RouteDecision::Exec => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(cmd: &str) -> RouteDecision {
        RouteTable::builtin().classify(cmd)
    }

    /// TDD: `cat <path>` is a silent Rewrite to the governed `read_file`
    /// built-in (the routing promotion). Revert the promotion and this is red.
    #[test]
    fn cat_routes_to_read_file() {
        assert_eq!(
            classify("cat src/main.rs"),
            RouteDecision::Route {
                tool: "read_file",
                args: json!({ "path": "src/main.rs" }),
            }
        );
        // A leading flag is dropped; the single operand still routes.
        assert_eq!(
            classify("cat -n src/main.rs"),
            RouteDecision::Route {
                tool: "read_file",
                args: json!({ "path": "src/main.rs" }),
            }
        );
    }

    /// TDD: read-only `git status` is a silent Rewrite to the governed `git`
    /// built-in read path.
    #[test]
    fn read_only_git_routes_to_the_git_builtin() {
        for (cmd, op) in [
            ("git status", "status"),
            ("git log", "log"),
            ("git diff", "diff"),
        ] {
            assert_eq!(
                classify(cmd),
                RouteDecision::Route {
                    tool: "git",
                    args: json!({ "op": op }),
                },
                "{cmd}"
            );
        }
    }

    /// Item 1 of the routing-honesty job: a routed call must never drop an
    /// operand. `git status -s` gates (no embedded way to honor `-s`'s short
    /// format). `git log`/`git diff` get an EXACT translation table for the
    /// shapes the embedded tool can honour; every other operand gates to
    /// exec instead of silently answering a different question with
    /// `ok: true`.
    #[test]
    fn git_read_routes_never_drop_an_operand() {
        assert_eq!(classify("git status -s"), RouteDecision::Exec);
        assert_eq!(classify("git status --porcelain"), RouteDecision::Exec);
        assert_eq!(
            classify("git diff --cached"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "diff", "spec": "staged" }),
            }
        );
        assert_eq!(
            classify("git diff --cached --stat"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "diff", "spec": "staged", "stat": true }),
            }
        );
        assert_eq!(
            classify("git diff --staged"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "diff", "spec": "staged" }),
            }
        );
        assert_eq!(
            classify("git log -5"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "log", "limit": 5 }),
            }
        );
        assert_eq!(
            classify("git log -n 5"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "log", "limit": 5 }),
            }
        );
    }

    /// The engine now serves a revision/range, an optional pathspec tail, and
    /// (for diff) `--stat` — this job's whole point (#2482). Each shape
    /// routes to the exact args the engine needs; nothing is dropped.
    #[test]
    fn git_read_routes_now_serve_revisions_pathspecs_and_stat() {
        assert_eq!(
            classify("git log A..B"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "log", "revision": "A..B" }),
            }
        );
        assert_eq!(
            classify("git log HEAD~3"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "log", "revision": "HEAD~3" }),
            }
        );
        assert_eq!(
            classify("git log -5 A..B -- src/lib.rs"),
            RouteDecision::Route {
                tool: "git",
                args: json!({
                    "op": "log",
                    "limit": 5,
                    "revision": "A..B",
                    "paths": ["src/lib.rs"],
                }),
            }
        );
        assert_eq!(
            classify("git log -- src/lib.rs docs/"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "log", "paths": ["src/lib.rs", "docs/"] }),
            }
        );
        assert_eq!(
            classify("git diff HEAD~1"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "diff", "rev": "HEAD~1" }),
            }
        );
        assert_eq!(
            classify("git diff A B"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "diff", "rev": "A", "rev2": "B" }),
            }
        );
        assert_eq!(
            classify("git diff A..B"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "diff", "rev": "A..B" }),
            }
        );
        assert_eq!(
            classify("git diff --stat"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "diff", "stat": true }),
            }
        );
        assert_eq!(
            classify("git diff --stat HEAD~1 -- src/lib.rs"),
            RouteDecision::Route {
                tool: "git",
                args: json!({
                    "op": "diff",
                    "rev": "HEAD~1",
                    "stat": true,
                    "paths": ["src/lib.rs"],
                }),
            }
        );
        // `A...B` (symmetric diff) and a bare token that also names a
        // worktree path are both still positionally routed as a `revision` /
        // `rev` — the router stays pure (no fs access) and unchanged; it is
        // the embedded git engine that now tells these two apart from an
        // ordinary revision and refuses them honestly instead of silently
        // answering the wrong question or failing with "could not resolve
        // commit".
        assert_eq!(
            classify("git log HEAD~1...HEAD"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "log", "revision": "HEAD~1...HEAD" }),
            }
        );
        assert_eq!(
            classify("git log src/lib.rs"),
            RouteDecision::Route {
                tool: "git",
                args: json!({ "op": "log", "revision": "src/lib.rs" }),
            }
        );
    }

    /// Near-misses that must STAY refused: the engine has no way to honor a
    /// presentation flag, more than two revisions, or an empty pathspec, so
    /// answering with a routed call would silently answer a different
    /// question.
    #[test]
    fn git_read_routes_still_gate_unservable_shapes() {
        assert_eq!(classify("git log --oneline"), RouteDecision::Exec);
        assert_eq!(classify("git log -p"), RouteDecision::Exec);
        assert_eq!(classify("git log -p A..B"), RouteDecision::Exec);
        assert_eq!(classify("git diff --word-diff"), RouteDecision::Exec);
        assert_eq!(classify("git diff --name-only"), RouteDecision::Exec);
        assert_eq!(classify("git diff A B C"), RouteDecision::Exec);
        assert_eq!(classify("git log --all"), RouteDecision::Exec);
        assert_eq!(classify("git diff --numstat"), RouteDecision::Exec);
    }

    #[test]
    fn branch_listing_routes_preserve_the_requested_namespace() {
        for (cmd, scope) in [
            ("git branch", "local"),
            ("git branch --list", "local"),
            ("git branch -a", "all"),
            ("git branch --all", "all"),
            ("git branch --list --all", "all"),
            ("git branch -a --list", "all"),
            ("git branch -r", "remote"),
            ("git branch --remotes", "remote"),
            ("git branch --list -r", "remote"),
            ("git branch --remotes --list", "remote"),
        ] {
            assert_eq!(
                classify(cmd),
                RouteDecision::Route {
                    tool: "git",
                    args: json!({ "op": "branch-list", "scope": scope }),
                },
                "{cmd}"
            );
        }
    }

    #[test]
    fn branch_listing_call_routes_preserve_argument_semantics() {
        let table = RouteTable::builtin();
        for command in ["git branch", "git branch --all", "git branch --remotes"] {
            assert_eq!(
                table.classify_call(&json!({"command": command})),
                classify(command)
            );
            for extra in [json!({"cwd": "elsewhere"}), json!({"timeout": 5})] {
                let mut call = extra;
                call["command"] = json!(command);
                assert_eq!(table.classify_call(&call), RouteDecision::Exec, "{call}");
            }
        }
        // This repair does not alter the existing routes' argument policy.
        for command in ["git status", "cat file", "ls"] {
            assert_eq!(
                table.classify_call(&json!({"command": command, "cwd": "elsewhere"})),
                classify(command)
            );
        }
    }

    #[test]
    fn branch_mutations_and_unrepresented_filters_never_become_listing() {
        for cmd in [
            "git branch topic",
            "git branch topic HEAD",
            "git branch -d topic",
            "git branch -D topic",
            "git branch -m old new",
            "git branch -M topic",
            "git branch -c old new",
            "git branch -C topic",
            "git branch --set-upstream-to=origin/main",
            "git branch --unset-upstream",
            "git branch --track topic origin/main",
            "git branch --edit-description",
            "git branch --list topic",
            "git branch --list 'feat/*'",
            "git branch --merged",
            "git branch --no-merged main",
            "git branch --contains HEAD",
            "git branch --format=%(refname)",
            "git branch --sort=-committerdate",
            "git branch --show-current",
            "git branch --unknown",
            "git branch -a -r",
            "git branch -ar",
            "git -C other branch",
            "git --git-dir=other branch",
            "git branch -a | wc -l",
            "git branch -a > branches.txt",
            "git branch --all && git branch topic",
        ] {
            assert_eq!(classify(cmd), RouteDecision::Exec, "{cmd}");
        }
    }

    /// TDD: state-modifying git is GATED as exec — NOT silently routed (owner
    /// decision 2). Revert the gate (route every git) and this is red.
    #[test]
    fn state_modifying_git_gates_as_exec() {
        for cmd in [
            "git add a.txt",
            "git add .",
            "git commit -m x",
            "git push",
            "git checkout -b feat",
            "git reset --hard",
            "git stash",
            // read-only but not yet built-in-served (follow-up) → gate, not a
            // misleading routed op error.
            "git show HEAD",
            "git branch topic",
            // a bare / unknown git reach gates.
            "git",
            "git frobnicate",
        ] {
            assert_eq!(classify(cmd), RouteDecision::Exec, "{cmd}");
        }
    }

    /// `ls` routes to `list_dir`; bare `ls` is the workspace root.
    #[test]
    fn ls_routes_to_list_dir() {
        assert_eq!(
            classify("ls"),
            RouteDecision::Route {
                tool: "list_dir",
                args: json!({ "path": "." }),
            }
        );
        assert_eq!(
            classify("ls -la src"),
            RouteDecision::Route {
                tool: "list_dir",
                args: json!({ "path": "src" }),
            }
        );
    }

    /// `find` routes to the embedded `find` tool, translating `-name`/`-type`.
    #[test]
    fn find_routes_with_translated_predicates() {
        assert_eq!(
            classify("find . -name '*.rs' -type f"),
            RouteDecision::Route {
                tool: "find",
                args: json!({ "path": ".", "name": "*.rs", "type": "f" }),
            }
        );
        // Bare `find` → workspace root, no filters.
        assert_eq!(
            classify("find"),
            RouteDecision::Route {
                tool: "find",
                args: json!({ "path": "." }),
            }
        );
        // An unmodeled predicate gates rather than silently dropping a filter.
        assert_eq!(classify("find . -newer ref.txt"), RouteDecision::Exec);
    }

    /// #1022: a simple shell-delete instinct routes to the governed fs_write
    /// tool instead of dead-ending on exec/absolute-deny. Recursive or
    /// multi-target deletes stay gated/denied.
    #[test]
    fn simple_delete_routes_to_delete_file() {
        for cmd in [
            "rm src/cockpit.rs",
            "rm -f src/cockpit.rs",
            "unlink src/cockpit.rs",
        ] {
            assert_eq!(
                classify(cmd),
                RouteDecision::Route {
                    tool: "delete_file",
                    args: json!({ "path": "src/cockpit.rs" }),
                },
                "{cmd}"
            );
        }
        for cmd in [
            "rm -rf src",
            "rm -r src",
            "rm a.txt b.txt",
            "rm *.tmp",
            "unlink a.txt b.txt",
        ] {
            assert_eq!(classify(cmd), RouteDecision::Exec, "{cmd}");
        }
    }

    /// Compound / redirected / substituted commands NEVER route — a single
    /// built-in cannot reproduce a pipe/chain/redirect, so they gate and the
    /// confined shell mediates each spawn. This is the routing-is-not-a-bypass
    /// guard at the decision layer.
    #[test]
    fn compound_commands_gate_not_route() {
        for cmd in [
            "cat secret | grep token",
            "cat a && rm -rf b",
            "cat $(echo /etc/passwd)",
            "ls > out.txt",
            "cat a; cat b",
            "cat `whoami`",
        ] {
            assert_eq!(classify(cmd), RouteDecision::Exec, "{cmd}");
        }
    }

    /// A glob operand needs shell expansion → gate (the built-in cannot expand
    /// `*.txt` into a set).
    #[test]
    fn glob_operands_gate() {
        assert_eq!(classify("cat *.txt"), RouteDecision::Exec);
        assert_eq!(classify("ls sub/*"), RouteDecision::Exec);
        // `cat a b` (two files) is ambiguous for the one-file read_file → gate.
        assert_eq!(classify("cat a.txt b.txt"), RouteDecision::Exec);
    }

    /// Non-routable programs (echo, grep, an interpreter) gate — only the
    /// data-table reaches route.
    #[test]
    fn unknown_programs_gate() {
        for cmd in ["echo hi", "grep foo bar", "bash script.sh", ""] {
            assert_eq!(classify(cmd), RouteDecision::Exec, "{cmd:?}");
        }
    }

    /// F11: `cargo build|check|test|clippy` routes to `build_exec` carrying
    /// the model's LITERAL argv — never a resolved phase command, so no
    /// operand (`-p newt-core`, a test filter, …) is dropped.
    #[test]
    fn cargo_build_commands_route_with_literal_argv() {
        assert_eq!(
            classify("cargo test -p newt-core"),
            RouteDecision::Route {
                tool: "build_exec",
                args: json!({ "argv": ["cargo", "test", "-p", "newt-core"] }),
            }
        );
        for (cmd, argv) in [
            ("cargo build", json!(["cargo", "build"])),
            ("cargo check", json!(["cargo", "check"])),
            (
                "cargo clippy --workspace",
                json!(["cargo", "clippy", "--workspace"]),
            ),
            (
                "cargo test -p x --lib -- --test-threads=1",
                json!([
                    "cargo",
                    "test",
                    "-p",
                    "x",
                    "--lib",
                    "--",
                    "--test-threads=1"
                ]),
            ),
        ] {
            assert_eq!(
                classify(cmd),
                RouteDecision::Route {
                    tool: "build_exec",
                    args: json!({ "argv": argv }),
                },
                "{cmd}"
            );
        }
    }

    /// `just <recipe>` routes the same way; anything more than a bare recipe
    /// name gates rather than guessing.
    #[test]
    fn just_recipe_routes_with_literal_argv() {
        assert_eq!(
            classify("just test"),
            RouteDecision::Route {
                tool: "build_exec",
                args: json!({ "argv": ["just", "test"] }),
            }
        );
        for cmd in ["just", "just test extra", "just -n test", "just --list"] {
            assert_eq!(classify(cmd), RouteDecision::Exec, "{cmd}");
        }
    }

    /// Near-misses that change what runs, or reach outside the calibrated
    /// fence, must NEVER route: `cargo run`/`install`/`publish` execute or
    /// publish something; `--config`/`-Z…` are config overrides / unstable
    /// flags. `cargo test | tail` is compound and already gated by
    /// `SHELL_META`, exercised here for this route specifically.
    #[test]
    fn cargo_near_misses_never_route() {
        for cmd in [
            "cargo run",
            "cargo install ripgrep",
            "cargo publish",
            "cargo test --config net.offline=false",
            "cargo build -Zunstable-options",
            "cargo test | tail",
            "cargo",
            "cargo frobnicate",
        ] {
            assert_eq!(classify(cmd), RouteDecision::Exec, "{cmd}");
        }
    }

    /// A build-lane route drops the rest of the call object (there is
    /// nowhere to put `cwd`/`timeout`), so `classify_call` must refuse to
    /// route a non-bare call — same rule as the scoped git-read route.
    #[test]
    fn build_route_call_requires_bare_call() {
        let table = RouteTable::builtin();
        assert_eq!(
            table.classify_call(&json!({"command": "cargo test"})),
            classify("cargo test")
        );
        for extra in [json!({"cwd": "sub"}), json!({"timeout": 5})] {
            let mut call = extra;
            call["command"] = json!("cargo test");
            assert_eq!(table.classify_call(&call), RouteDecision::Exec, "{call}");
        }
    }

    /// TDD: every silent rewrite is logged — `audit_line` is `Some` for a Route
    /// (carrying the original command + the governed built-in) and `None` for
    /// an Exec (nothing rewritten).
    #[test]
    fn audit_line_logs_every_rewrite_and_only_rewrites() {
        let routed = classify("cat foo.txt");
        let line = audit_line("cat foo.txt", &routed).expect("a route is logged");
        assert!(line.contains("cat foo.txt"), "{line}");
        assert!(line.contains("read_file"), "{line}");

        // An Exec decision produces no audit line — nothing was rewritten.
        assert_eq!(audit_line("git add .", &classify("git add .")), None);
    }
}
