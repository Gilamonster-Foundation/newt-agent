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

/// The git subcommands that are **read-only** and route to the embedded `git`
/// tool's read path — pure DATA.
///
/// These map one-to-one onto the embedded git tool's read ops
/// (`status`/`log`/`diff`). State-modifying subcommands (`add`, `stash`,
/// `checkout`, `reset`, `commit`, `push`, `amend`, `rebase`, `branch-delete`,
/// …) are **NOT** here: they GATE as exec (owner decision 2). `show` and a
/// bare `branch` (list) are read-only too, but the embedded git tool has no
/// read-only op for them yet, so routing them would surface a misleading op
/// error — they gate for now and join this set as a one-line data edit once
/// the git tool grows the matching read ops (follow-up).
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
            return match tokens.next() {
                Some(sub) if self.git_read_only.contains(&sub) => RouteDecision::Route {
                    tool: "git",
                    args: json!({ "op": sub }),
                },
                _ => RouteDecision::Exec,
            };
        }

        // Whole-command reaches that map onto governed built-ins.
        let Some(route) = self.shell_routes.iter().find(|r| r.program == program) else {
            return RouteDecision::Exec;
        };
        let rest: Vec<&str> = tokens.collect();
        build_shell_route(route.tool, &rest)
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
            ("git status -s", "status"),
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
            "git branch",
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
