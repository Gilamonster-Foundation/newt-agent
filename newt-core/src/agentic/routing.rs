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
use std::path::Path;

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

/// Is `program` a build tool this repo recognises — the single table both
/// [`RouteTable::classify`]'s clean-argv route AND (F20) the confined shell's
/// per-call wall clock read, so a compound build command (`cargo test …; echo
/// …`) that CANNOT route still gets classified as a build reach by the same
/// rule, instead of a second hand-maintained list.
pub(crate) fn is_build_tool_program(program: &str) -> bool {
    program == "cargo" || program == "just"
}

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
    /// newly scoped Git read route. Other routes retain their existing
    /// policy. `workspace` is the session's workspace root; `read_scope` is
    /// the call's fs-read fence (F24/PR1's `cd`/`cwd` fold, below, needs
    /// both — the caller's own values, never read from the process).
    #[must_use]
    pub(crate) fn classify_call(
        &self,
        call: &Value,
        workspace: &Path,
        read_scope: &crate::caveats::Scope<String>,
    ) -> RouteDecision {
        let decision = self.classify(
            call.get("command").and_then(Value::as_str).unwrap_or(""),
            workspace,
            read_scope,
        );
        let RouteDecision::Route { tool, .. } = &decision else {
            return decision;
        };
        // Every route discards the rest of the call object once routed —
        // none has anywhere to put a `timeout` the model also sent. #2551
        // round 2 (the BLOCKER's "same rule" for the `cwd`-field path, and
        // its pre-existing sibling: `{command:"cat f", cwd:"sub"}` used to
        // route and silently drop `cwd`): a `cwd` field is allowed
        // alongside `command` ONLY for `build_exec`/`git` — the two routes
        // that actually read it ([`attach_cwd`]) — every other route
        // refuses a call carrying anything beyond bare `command`, exactly
        // as `build_exec`/the scoped git read always required.
        let extra_keys_allowed: &[&str] = if matches!(*tool, "build_exec" | "git") {
            &["command", "cwd"]
        } else {
            &["command"]
        };
        let keys_ok = call.as_object().is_some_and(|obj| {
            obj.keys()
                .all(|key| extra_keys_allowed.contains(&key.as_str()))
        });
        if !keys_ok {
            return RouteDecision::Exec;
        }
        // A leading `cd` INSIDE `command` already resolved a `cwd` above —
        // that is the question `cd` itself answers, so a `cwd` FIELD
        // alongside it is never silently combined into a second, different
        // directory. #2551 round 3: the leading `cd` was folded and
        // resolved against `workspace`, but a real shell resolves a
        // RELATIVE leading `cd` against the call's `cwd` field, not the
        // workspace root — `{command:"cd sub && cargo test", cwd:"other"}`
        // routes to `<root>/sub` while the shell would run in
        // `<root>/other/sub`. Refuse rather than silently building/reading
        // the wrong directory.
        let already_has_cwd =
            matches!(&decision, RouteDecision::Route { args, .. } if args.get("cwd").is_some());
        if already_has_cwd {
            return if call.get("cwd").is_some() {
                RouteDecision::Exec
            } else {
                decision
            };
        }
        // PR1: a model-supplied `cwd` FIELD on an otherwise-bare call is the
        // SAME question a leading `cd` asks — resolved the SAME way
        // ([`attach_cwd`]'s own build_exec/git-only rule applies here too).
        // A call with no `cwd` field at all (the original bare shape) is
        // unaffected: `decision` returned as-is.
        let Some(cwd_field) = call.get("cwd").and_then(Value::as_str) else {
            return decision;
        };
        match resolve_workspace_relative_dir(cwd_field, workspace, read_scope) {
            Some(cwd) => attach_cwd(decision, cwd),
            None => RouteDecision::Exec,
        }
    }

    /// Classify a `run_command` shell-command string. `workspace`/
    /// `read_scope` feed PR1's `cd`-fold (below) — real filesystem reads
    /// (`std::fs::canonicalize`), no longer the pure table lookup this was
    /// before #2550/PR1; every other branch stays a data lookup.
    #[must_use]
    pub(crate) fn classify(
        &self,
        command: &str,
        workspace: &Path,
        read_scope: &crate::caveats::Scope<String>,
    ) -> RouteDecision {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return RouteDecision::Exec;
        }
        // PR1 (r10-r12 evidence, multi-repo-recon rows 1/2/4/6): almost
        // every `run_command` began with `cd <dir> && …` — not only the
        // workspace root (#2550's F24 case) but a SUBDIRECTORY (`cd
        // repoA && cargo test`), which used to stay compound and never
        // route at all, even though `tools/shell.rs`'s `split_leading_cd`
        // ALREADY folds it into a correct `cwd` for the confined shell's
        // own dispatch. Fold it here too, BEFORE any other check (the
        // tail-pipe attempt, the SHELL_META refusal), so `<rest>` is
        // classified exactly as if it had been sent alone from `<dir>`.
        let (effective, cwd) = fold_leading_cd(trimmed, workspace, read_scope);
        let decision = self.classify_stripped(effective);
        match cwd {
            Some(cwd) => attach_cwd(decision, cwd),
            None => decision,
        }
    }

    /// The classification table lookup itself, over an ALREADY-stripped
    /// command (no leading no-op `cd`). Split out of [`Self::classify`] so
    /// the `cd`-prefix handling has exactly one seam to annotate the
    /// result, rather than every early return needing to remember it.
    fn classify_stripped(&self, command: &str) -> RouteDecision {
        // F23 / #2524 "tail-pipe-routes": recognise EXACTLY `<clean build
        // argv> [2>&1] | tail -N` (or `head -N`) BEFORE the blanket
        // compound-command refusal below — the model pipes ONLY to cut
        // output (see `build_piped_to_trim_route`'s doc), and a masked exit
        // code from `| tail` is the exact hazard `note_verified_pass`'s doc
        // comment (mod.rs) names as why an un-routed `run_command` pass
        // can't be trusted. Only short-circuits on an actual match; any
        // other pipe shape (multiple pipes, `tail -f`, a redirect before the
        // pipe, a grep, …) falls through unchanged to the refusal below.
        if command.contains('|') {
            if let route @ RouteDecision::Route { .. } = build_piped_to_trim_route(command) {
                return route;
            }
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
        if is_build_tool_program(program) {
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

/// Resolve `dir` (a `cd` argument or a `cwd` field — the SAME question
/// either way) against `workspace`: the path `workspace`/`read_scope` grant
/// a real `cd <dir>` would land in. Returns `Some(<relative path>)` (`.`
/// for `workspace` itself) or `None` — never a different directory than a
/// real shell would reach, and never one outside this call's authority.
///
/// `<dir>` must first be a plain, unquoted, non-glob, metacharacter-free,
/// whitespace-free token (checked against the SAME [`SHELL_META`]/[`GLOB`]/
/// [`BUILD_UNSAFE`] tables the rest of this module already refuses on, plus
/// no internal whitespace — `cd /a b` is TWO arguments to a real `cd` and
/// fails there, #2550 round 2) — `cd -`, `cd ~`, a quoted or globbed dir
/// never qualifies. #2551 round 2: a `..` component in `<dir>` ALSO refuses
/// outright, checked on the un-normalized token (never re-added by
/// `#2550`'s original reasoning: `std::fs::canonicalize` resolves
/// PHYSICALLY, following every symlink to its real target, while a real
/// shell's `cd` defaults to LOGICAL (`-L`) — `cd link/..` lands wherever
/// `link` pointed in bash, not one physical level up from where it
/// resolves. Refusing any `..` sidesteps that divergence entirely rather
/// than trying to emulate it.
///
/// Then PR1's real check, replacing #2550's lexical "does `<dir>` normalize
/// to the workspace root" with the actual question: does `<dir>`
/// canonicalize (through any symlink) to a real DIRECTORY inside both the
/// canonical workspace AND this call's fs-read fence
/// ([`crate::caveats::permits_path`], the SAME containment check the
/// interactive tool gate and the headless coder apply path both use)? A
/// missing directory, a file, an out-of-workspace absolute path, and a
/// symlink that resolves outside all fail identically to how a real `cd`
/// (or a real out-of-scope read) would refuse — this is the question `cd`
/// itself answers, not a parallel heuristic.
///
/// #2551 round 2 should-fix: the fence is checked on BOTH the lexical join
/// (`workspace.join(dir)`, matching how `read_file`'s own fs gate checks a
/// path — see `tools.rs`; a symlinked WORKSPACE root cannot itself mismatch
/// a fence granted by its original string) AND the canonical path (a
/// symlink INSIDE a granted root that points to a real directory OUTSIDE
/// the fence — `sub` granted, `sub/link` → `<root>/other` — used to pass
/// the lexical check alone and route with a `cwd` the fence never granted).
fn resolve_workspace_relative_dir(
    dir: &str,
    workspace: &Path,
    read_scope: &crate::caveats::Scope<String>,
) -> Option<String> {
    if dir.is_empty()
        || dir == "-"
        || dir.starts_with('~')
        || dir.contains(SHELL_META)
        || dir.contains(GLOB)
        || dir.contains(BUILD_UNSAFE)
        || dir.contains(char::is_whitespace)
        || Path::new(dir)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    let lexical = workspace.join(dir);
    if !crate::caveats::permits_path(read_scope, &lexical.to_string_lossy()) {
        return None;
    }
    let canonical_workspace = workspace.canonicalize().ok()?;
    let canonical_dir = lexical.canonicalize().ok()?;
    if !canonical_dir.is_dir() || !canonical_dir.starts_with(&canonical_workspace) {
        return None;
    }
    // #2551 round 3 nit: the fence was granted against the (possibly
    // symlinked) WORKSPACE string, e.g. macOS `/tmp/ws` for a canonical
    // `/private/tmp/ws`. Checking `canonical_dir` against the un-canonicalized
    // `read_scope` then fails for every `<dir>`, including `.` — canonicalize
    // the scope's own roots the same way before checking the canonical side,
    // so a symlinked workspace root still folds `cd`s.
    if !crate::caveats::permits_path(
        &canonicalize_scope(read_scope),
        &canonical_dir.to_string_lossy(),
    ) {
        return None;
    }
    let relative = canonical_dir.strip_prefix(&canonical_workspace).ok()?;
    Some(if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        relative.to_string_lossy().into_owned()
    })
}

/// Canonicalize each root in a read-fence scope, for comparing against an
/// already-canonicalized candidate path. A root that fails to canonicalize
/// (does not exist, dangling symlink) is kept as-is — it simply will not
/// match a canonical candidate, which is fail-closed, not a widening.
fn canonicalize_scope(scope: &crate::caveats::Scope<String>) -> crate::caveats::Scope<String> {
    match scope {
        crate::caveats::Scope::All => crate::caveats::Scope::All,
        crate::caveats::Scope::Only(set) => crate::caveats::Scope::only(set.iter().map(|root| {
            std::fs::canonicalize(root)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.clone())
        })),
    }
}

/// Insert a resolved `cwd` into a route's args — the one seam both
/// [`RouteTable::classify`]'s leading-`cd` fold and [`RouteTable::
/// classify_call`]'s `cwd`-field fold use, so a routed call's `cwd` is
/// attached identically regardless of which shape asked for it.
///
/// #2551 round 2 BLOCKER: `cwd` is honoured ONLY by `build_exec`
/// (`run_confined_build_lane`) and `git` (`newt-git` reads it directly) —
/// `read_file`/`list_dir`/`find`/`delete_file` all join `workspace + path`
/// and never look at `args["cwd"]`. Attaching it to any OTHER route would
/// leave the built-in silently acting on `workspace` while the routed note
/// (and the model) believed it ran in the folded directory — measured: `cd
/// sub && rm x` would have deleted `<root>/x`, not `<root>/sub/x`. So a
/// resolved `cwd != "."` on any other route refuses (`Exec`) instead —
/// `cwd == "."` (the workspace root itself) is harmless for any tool, since
/// that IS where they already run.
fn attach_cwd(decision: RouteDecision, cwd: String) -> RouteDecision {
    let RouteDecision::Route { tool, mut args } = decision else {
        return decision;
    };
    if !matches!(tool, "build_exec" | "git") {
        return if cwd == "." {
            RouteDecision::Route { tool, args }
        } else {
            RouteDecision::Exec
        };
    }
    if let Some(obj) = args.as_object_mut() {
        obj.insert("cwd".to_string(), Value::String(cwd));
    }
    RouteDecision::Route { tool, args }
}

/// Split a leading `cd <dir> && <rest>` or `cd <dir>; <rest>` — exactly ONE
/// `cd`, at the FIRST `&&` or `;` (whichever comes first): `cd a && cd b &&
/// x` yields `dir="a"`, `rest="cd b && x"`, and `rest` still contains `&&`
/// so [`RouteTable::classify_stripped`]'s ordinary compound-command refusal
/// catches the second `cd` — no separate "two `cd`s" check needed. `cd
/// <dir>` with no `&&`/`;` at all (nothing to run) and any other leading
/// token return `None` — command untouched.
fn split_leading_cd(command: &str) -> Option<(&str, &str)> {
    let after = command.strip_prefix("cd ")?;
    let amp = after.find("&&");
    let semi = after.find(';');
    let (dir, rest) = match (amp, semi) {
        (Some(a), Some(s)) if s < a => (&after[..s], &after[s + 1..]),
        (Some(a), _) => (&after[..a], &after[a + 2..]),
        (None, Some(s)) => (&after[..s], &after[s + 1..]),
        (None, None) => return None,
    };
    Some((dir.trim(), rest.trim_start()))
}

/// Fold a leading `cd <dir> && `/`cd <dir>; ` off `command` when `<dir>`
/// resolves inside the workspace and fence ([`resolve_workspace_relative_dir`]).
/// Returns `(effective_command, Some(relative_dir))` when folded,
/// `(command, None)` otherwise — subsumes #2550's workspace-root case
/// (`<dir>` resolves to `.`) under the same mechanism, so `cd <root> &&
/// cargo test` still routes, just via this path now.
fn fold_leading_cd<'a>(
    command: &'a str,
    workspace: &Path,
    read_scope: &crate::caveats::Scope<String>,
) -> (&'a str, Option<String>) {
    let Some((dir, rest)) = split_leading_cd(command) else {
        return (command, None);
    };
    match resolve_workspace_relative_dir(dir, workspace, read_scope) {
        Some(cwd) => (rest, Some(cwd)),
        None => (command, None),
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

/// Recognise EXACTLY `<clean build argv> [2>&1] | tail -N` (or `tail -n N` /
/// `head -N` / `head -n N`) and nothing wider (F23 / #2524
/// "tail-pipe-routes"). Measured (2488-r9): the model's own natural call to
/// verify a build was `cargo … 2>&1 | tail -40` — piping ONLY to cut a long
/// build's output, not to chain semantics onto it. Because the pipe made it
/// compound, it refused to route (`SHELL_META`) and ran in the confined
/// shell instead, where `| tail` masks cargo's real exit code — exactly the
/// hazard `note_verified_pass`'s doc comment (`mod.rs`) names as why a
/// piped `run_command` pass can never count as verified. Route the build
/// argv the model actually wrote (`build_lane_route`, unchanged, still
/// refuses an unsafe operand or a non-build-tool program) and apply the
/// trim to the OUTPUT on the harness side instead, so the exit code the
/// harness sees is cargo's, never `tail`'s.
///
/// Returns `RouteDecision::Exec` for anything this exact shape does not
/// cover: more than one pipe, a pipe to anything but bare `tail`/`head`
/// with a positive line count, `tail -f` / `head -c`, or a redirect/other
/// metacharacter surviving in the build half (checked via the existing
/// [`SHELL_META`] table — reused, not duplicated) — those fall through to
/// the ordinary compound-command refusal in [`RouteTable::classify`].
fn build_piped_to_trim_route(command: &str) -> RouteDecision {
    let Some((build_part, trim_part)) = split_single_pipe(command) else {
        return RouteDecision::Exec;
    };
    let build_part = build_part.trim();
    let build_part = build_part
        .strip_suffix("2>&1")
        .map_or(build_part, str::trim);
    // Any OTHER shell metacharacter surviving in the build half (a redirect
    // BEFORE the pipe, a second chain, …) refuses — same table the blanket
    // compound-command check uses, so a redirect stays refused exactly as
    // today.
    if build_part.contains(SHELL_META) {
        return RouteDecision::Exec;
    }
    let Some(trim) = parse_trim_spec(trim_part.trim()) else {
        return RouteDecision::Exec;
    };
    let mut tokens = build_part.split_ascii_whitespace();
    let Some(program) = tokens.next() else {
        return RouteDecision::Exec;
    };
    if !is_build_tool_program(program) {
        return RouteDecision::Exec;
    }
    let rest: Vec<&str> = tokens.collect();
    match build_lane_route(program, &rest) {
        RouteDecision::Route {
            tool: "build_exec",
            mut args,
        } => {
            args["trim"] = trim;
            RouteDecision::Route {
                tool: "build_exec",
                args,
            }
        }
        // `build_lane_route` itself refused (unsafe operand, unrecognised
        // subcommand, …) — the same refusal applies with or without the
        // trailing pipe.
        other => other,
    }
}

/// Split `command` on exactly ONE `|`. `None` for zero pipes or more than
/// one — the brief's own "multiple pipes … do NOT route" refusal, checked
/// here rather than downstream so a `| tail -40 | head` never even reaches
/// [`parse_trim_spec`].
fn split_single_pipe(command: &str) -> Option<(&str, &str)> {
    let mut parts = command.split('|');
    let build_part = parts.next()?;
    let trim_part = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some((build_part, trim_part))
}

/// `tail -N` / `tail -n N` / `head -N` / `head -n N` — a bare positive
/// integer line count and nothing else. `tail -f` (follow), `head -c`
/// (bytes), extra operands, or a missing/zero/non-numeric count are all
/// refused — this is the ONE place that decides the trim shape is safe to
/// apply, so it stays conservative rather than guessing at intent.
fn parse_trim_spec(spec: &str) -> Option<Value> {
    let tokens: Vec<&str> = spec.split_ascii_whitespace().collect();
    let mode = match tokens.first().copied() {
        Some("tail") => "tail",
        Some("head") => "head",
        _ => return None,
    };
    let n_str = match &tokens[1..] {
        [flag] if flag.len() > 1 && flag.starts_with('-') => &flag[1..],
        ["-n", n] => *n,
        _ => return None,
    };
    // PR #2549 round 2: `u32::from_str` tolerates a leading `+` (unsigned
    // parsers reject `-`, not `+`), so `tail -n +40` / `head -n +40` — GNU's
    // "start AT line 40", a real and DIFFERENT flag shape from "last/first
    // 40 lines" — parsed as if it meant `-n 40`, showing the wrong output
    // slice under a false "as `| tail -40` asked" provenance claim.
    // `head -n +N` isn't even documented by GNU head, so treating it as `-n
    // N` had no basis either way. Check the STRING, not `parse`'s leniency:
    // every byte must be an ASCII digit — refuses `+40`, a stray `-` inside
    // the `-n` form, and any other non-digit that `u32::from_str` might
    // someday tolerate.
    if n_str.is_empty() || !n_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = n_str.parse().ok()?;
    if n == 0 {
        return None;
    }
    Some(json!({ "mode": mode, "n": n }))
}

/// A `run_command` reach that maps onto the confined **build lane**
/// (`build_exec` in `tools.rs`, sharing `run_confined_build_lane` with
/// `lifecycle action=build`) rather than a governed read built-in. Unlike
/// every other route in this table, the routed call runs the model's
/// **literal argv verbatim** — never a re-resolved phase command — so no
/// operand (`-p x`, a test filter, …) is ever silently dropped. A `cwd`
/// (from a leading `cd` or a `cwd` field) is resolved and attached by
/// [`RouteTable::classify`]/[`RouteTable::classify_call`] (PR1); `timeout`
/// still has nowhere to go, so a call carrying it stays Exec.
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
    // #2524 follow-up (F23 evidence): 2488-r9's own command was `cargo
    // +stable test …` — cargo's leading `+toolchain` selector, ONE token,
    // before the subcommand. Accept it and KEEP it in the routed argv (the
    // build lane must run the SAME toolchain the model asked for, never a
    // silently different default one). A `+` token that isn't a valid
    // selector (`is_toolchain_selector`) refuses the whole call rather than
    // stripping it and running something else.
    let (toolchain, rest) = match rest.split_first() {
        Some((first, tail)) if first.starts_with('+') => {
            if !is_toolchain_selector(first) {
                return RouteDecision::Exec;
            }
            (Some(*first), tail)
        }
        _ => (None, rest),
    };
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
    let mut argv = vec!["cargo".to_string()];
    argv.extend(toolchain.map(str::to_string));
    argv.push((*sub).to_string());
    argv.extend(rest.iter().map(|t| (*t).to_string()));
    RouteDecision::Route {
        tool: "build_exec",
        args: json!({ "argv": argv }),
    }
}

/// Is `token` a valid cargo/rustup toolchain selector (`+stable`,
/// `+nightly`, `+1.85.0`, `+nightly-2026-09-01`)? The name after `+` is
/// restricted to `[A-Za-z0-9._-]+` — rustup toolchain names are a channel,
/// an optional date, and an optional target triple, dot/dash-separated;
/// nothing in that alphabet is a shell metacharacter, so this can never
/// smuggle one past `BUILD_UNSAFE`/`SHELL_META`. A bare `+`, an empty name,
/// or anything else refuses — see [`cargo_build_route`], which refuses the
/// WHOLE call on a `false` here rather than silently dropping the token.
///
/// `pub(crate)` (#2524 follow-up / #2548 interaction): `mod.rs`'s
/// `is_progress_verification` reads `routed argv[1]` as the gate
/// subcommand — for a routed `cargo +stable test`, that slot is `+stable`,
/// not `test`, so a genuine pass would silently never count without also
/// skipping the selector there. ONE rule, shared, not re-derived.
pub(crate) fn is_toolchain_selector(token: &str) -> bool {
    match token.strip_prefix('+') {
        Some(name) if !name.is_empty() => name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
        _ => false,
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

    /// A workspace no test command's `cd <dir> &&` prefix (if any) could
    /// ever resolve against (it does not exist on disk), so PR1's `cd`-fold
    /// never fires for a test that doesn't ask for it explicitly (via
    /// [`classify_at`]/[`CdFixture`]) — and for a command with no leading
    /// `cd` at all, `fold_leading_cd` never touches the filesystem, so a
    /// nonexistent path costs nothing.
    fn no_cd_match() -> &'static Path {
        Path::new("/never-matches-a-test-cwd")
    }

    fn classify(cmd: &str) -> RouteDecision {
        RouteTable::builtin().classify(cmd, no_cd_match(), &crate::caveats::Scope::All)
    }

    fn classify_at(
        cmd: &str,
        workspace: &Path,
        read_scope: &crate::caveats::Scope<String>,
    ) -> RouteDecision {
        RouteTable::builtin().classify(cmd, workspace, read_scope)
    }

    /// A real workspace tree for PR1's `cd`-fold tests: `root/`, `root/sub/`
    /// (a real subdirectory to `cd` into), and `root/file.txt` (exists, but
    /// is not a directory). The brief's own boundary needs a REAL
    /// filesystem — `std::fs::canonicalize` is the question a real `cd`
    /// answers, and no mock stands in for it.
    struct CdFixture {
        _dir: tempfile::TempDir,
        root: std::path::PathBuf,
        #[allow(dead_code)]
        sub: std::path::PathBuf,
    }

    impl CdFixture {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().expect("tempdir");
            // Canonicalize up front: a platform tempdir may itself be
            // reached through a symlink (macOS: `/tmp` → `/private/tmp`),
            // and every assertion below must compare against the SAME
            // canonical form `resolve_workspace_relative_dir` produces.
            let root = dir.path().canonicalize().expect("canonicalize tempdir");
            let sub = root.join("sub");
            std::fs::create_dir(&sub).expect("mkdir sub");
            std::fs::write(root.join("file.txt"), b"not a directory").expect("write file");
            // #2551 round 2 BLOCKER fixture: `x`/`f` at BOTH the root and
            // `sub/`, distinct content, so a test that reads/deletes the
            // WRONG one is provably wrong rather than accidentally right.
            std::fs::write(root.join("x"), b"root x").expect("write root x");
            std::fs::write(root.join("f"), b"root f").expect("write root f");
            std::fs::write(sub.join("x"), b"sub x").expect("write sub x");
            std::fs::write(sub.join("f"), b"sub f").expect("write sub f");
            Self {
                _dir: dir,
                root,
                sub,
            }
        }

        fn read_scope(&self) -> crate::caveats::Scope<String> {
            crate::caveats::Scope::only([self.root.to_string_lossy().into_owned()])
        }
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
                table.classify_call(
                    &json!({"command": command}),
                    no_cd_match(),
                    &crate::caveats::Scope::All
                ),
                classify(command)
            );
            // `timeout` still refuses; `cwd: "elsewhere"` does NOT resolve
            // against `no_cd_match()` (a nonexistent workspace), so it
            // refuses too — for a different reason than before PR1 (an
            // unresolvable `cwd`, not a bare-call rule), same outcome.
            for extra in [json!({"cwd": "elsewhere"}), json!({"timeout": 5})] {
                let mut call = extra;
                call["command"] = json!(command);
                assert_eq!(
                    table.classify_call(&call, no_cd_match(), &crate::caveats::Scope::All),
                    RouteDecision::Exec,
                    "{call}"
                );
            }
        }
        // #2551 round 2 BLOCKER (flipped from the pre-round-2 assertion,
        // per the review — this WAS "routes and silently drops cwd" for
        // `cat`/`ls`, the exact class of bug the blocker fixes; `git
        // status` now follows the SAME uniform rule as every other route,
        // not just the scoped-read ones): a `cwd` field on any of these —
        // `git status` included, `git` honours `cwd` for every op, not
        // only `branch-list` — is the SAME question a leading `cd` asks.
        // Against `no_cd_match()` (a nonexistent workspace) it never
        // resolves, so all three now refuse rather than silently ignoring
        // the field and running at the wrong (root) directory.
        for command in ["git status", "cat file", "ls"] {
            assert_eq!(
                table.classify_call(
                    &json!({"command": command, "cwd": "elsewhere"}),
                    no_cd_match(),
                    &crate::caveats::Scope::All
                ),
                RouteDecision::Exec,
                "{command}"
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

    /// #2524 follow-up (F23 evidence): a leading `+toolchain` selector
    /// routes, kept verbatim in the argv, for every valid rustup toolchain
    /// name shape.
    #[test]
    fn cargo_toolchain_selector_routes_and_is_kept_in_the_argv() {
        assert_eq!(
            classify("cargo +stable test"),
            RouteDecision::Route {
                tool: "build_exec",
                args: json!({ "argv": ["cargo", "+stable", "test"] }),
            }
        );
        for (cmd, argv) in [
            (
                "cargo +nightly build",
                json!(["cargo", "+nightly", "build"]),
            ),
            ("cargo +1.85.0 check", json!(["cargo", "+1.85.0", "check"])),
            (
                "cargo +nightly-2026-09-01 clippy",
                json!(["cargo", "+nightly-2026-09-01", "clippy"]),
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

    /// An invalid or empty `+` token refuses the WHOLE call — never
    /// silently dropped, never routed with a different toolchain than the
    /// model asked for.
    #[test]
    fn cargo_invalid_toolchain_selector_never_routes() {
        for cmd in [
            "cargo + test",
            // `$` is a SHELL_META character; also caught by the top-level
            // compound-command refusal before this ever reaches
            // `cargo_build_route`, but exercised here for this shape too.
            "cargo +$X test",
            "cargo +stable/../etc test",
        ] {
            assert_eq!(classify(cmd), RouteDecision::Exec, "{cmd}");
        }
    }

    /// A build-lane route drops the rest of the call object (there is
    /// nowhere to put `timeout`), so `classify_call` must refuse to route a
    /// call carrying it — same rule as the scoped git-read route. `cwd` is
    /// NOT one of these any more (PR1) — see
    /// [`a_cwd_field_on_a_bare_call_routes_the_same_way`].
    #[test]
    fn build_route_call_requires_bare_call() {
        let table = RouteTable::builtin();
        assert_eq!(
            table.classify_call(
                &json!({"command": "cargo test"}),
                no_cd_match(),
                &crate::caveats::Scope::All
            ),
            classify("cargo test")
        );
        let call = json!({"command": "cargo test", "timeout": 5});
        assert_eq!(
            table.classify_call(&call, no_cd_match(), &crate::caveats::Scope::All),
            RouteDecision::Exec,
            "{call}"
        );
    }

    /// PR1: a model-supplied `cwd` FIELD on an otherwise-bare call is the
    /// SAME question a leading `cd` asks, resolved the SAME way.
    #[test]
    fn a_cwd_field_on_a_bare_call_routes_the_same_way() {
        let fx = CdFixture::new();
        let scope = fx.read_scope();
        let table = RouteTable::builtin();
        assert_eq!(
            table.classify_call(
                &json!({"command": "cargo test", "cwd": "sub"}),
                &fx.root,
                &scope
            ),
            RouteDecision::Route {
                tool: "build_exec",
                args: json!({"argv": ["cargo", "test"], "cwd": "sub"}),
            }
        );
        // Still refuses a call carrying anything besides `command`/`cwd`.
        assert_eq!(
            table.classify_call(
                &json!({"command": "cargo test", "cwd": "sub", "timeout": 5}),
                &fx.root,
                &scope
            ),
            RouteDecision::Exec
        );
        // An unresolvable `cwd` field refuses rather than silently dropping
        // it and running in the wrong place.
        assert_eq!(
            table.classify_call(
                &json!({"command": "cargo test", "cwd": "missing"}),
                &fx.root,
                &scope
            ),
            RouteDecision::Exec
        );
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

    /// F23 / #2524 "tail-pipe-routes" (red first): the LITERAL shape
    /// measured in 2488-r9 — a clean build argv, with its `+stable`
    /// toolchain selector, piped only to cut output — routes, carrying the
    /// build's own argv (toolchain included) plus the trim the pipe asked
    /// for. The `+toolchain` support itself is `is_toolchain_selector`'s.
    #[test]
    fn build_piped_to_tail_or_head_routes_with_the_trim() {
        assert_eq!(
            classify(
                "cargo +stable test -j 4 -p newt-core --test config_publishing_ratchet 2>&1 | tail -40"
            ),
            RouteDecision::Route {
                tool: "build_exec",
                args: json!({
                    "argv": ["cargo", "+stable", "test", "-j", "4", "-p", "newt-core",
                              "--test", "config_publishing_ratchet"],
                    "trim": {"mode": "tail", "n": 40},
                }),
            }
        );
        // Every recognised variant: with/without `2>&1`, `-N` and `-n N`,
        // `tail` and `head`, and `just`.
        for (cmd, mode, n) in [
            ("cargo test -p newt-core | tail -40", "tail", 40),
            ("cargo test -p newt-core | tail -n 40", "tail", 40),
            ("cargo build | head -20", "head", 20),
            ("cargo build | head -n 20", "head", 20),
            ("just test 2>&1 | tail -5", "tail", 5),
        ] {
            let RouteDecision::Route { tool, args } = classify(cmd) else {
                panic!("{cmd} must route");
            };
            assert_eq!(tool, "build_exec", "{cmd}");
            assert_eq!(args["trim"]["mode"], mode, "{cmd}");
            assert_eq!(args["trim"]["n"], n, "{cmd}");
        }
    }

    /// Anything wider than the exact recognised shape stays refused — the
    /// SAME `SHELL_META` compound-command refusal every other pipe already
    /// gets, never a new leniency.
    #[test]
    fn build_piped_to_anything_else_never_routes() {
        for cmd in [
            // Multiple pipes.
            "cargo test | tail -40 | head",
            // Follows, doesn't cut — a live stream, not a bounded trim.
            "cargo test | tail -f",
            // Bytes, not lines.
            "cargo build | head -c 10",
            // Not tail/head at all.
            "cargo test | grep FAILED",
            // A redirect BEFORE the pipe: still compound, still refused.
            "cargo test > log.txt | tail -40",
            // Missing, zero, or non-numeric N.
            "cargo test | tail",
            "cargo test | tail -0",
            "cargo test | tail -n 0",
            "cargo test | tail -abc",
            // Extra operands after the count.
            "cargo test | tail -40 extra",
            // Not a recognised build tool program.
            "pytest | tail -40",
            // An operand `build_lane_route` itself would already refuse
            // (near-miss subcommand) — still refused with the pipe.
            "cargo run | tail -40",
            // PR #2549 round 2: GNU's `-n +N` means "start AT line N", a
            // real and DIFFERENT shape from `-n N` ("last/first N lines").
            // `u32::from_str` tolerates a leading `+`, so this must be an
            // explicit string check, not left to `parse`'s leniency.
            "cargo test | tail -n +40",
            "cargo test | head -n +40",
            "cargo test | tail -+40",
            // Two pipes with nothing between them (`||`) splits into THREE
            // parts on `split_single_pipe`'s own code path — refused the
            // same way as any other multi-pipe shape, exercised explicitly.
            "cargo test || tail -40",
        ] {
            assert_eq!(classify(cmd), RouteDecision::Exec, "{cmd}");
        }
    }

    /// PR1 (r10-r12 evidence, multi-repo-recon rows 1/2/4/6, red first):
    /// almost every `run_command` began with `cd <dir> && …` — not only the
    /// workspace root (#2550's F24 case) but a real SUBDIRECTORY (`cd
    /// repoA && cargo test`), which used to stay compound and never route
    /// at all even though `tools/shell.rs`'s `split_leading_cd` already
    /// folds it correctly for the confined shell's own dispatch. Folds now,
    /// with `cwd` attached, for BOTH the build lane and the git read route,
    /// and composes with #2549's tail-pipe route.
    // Not on Windows: the cd fold does not resolve there yet (fail-closed to
    // Exec, as before this change). Refusal tests still run on every platform.
    #[cfg(not(windows))]
    #[test]
    fn cd_into_a_real_subdirectory_routes_with_cwd() {
        let fx = CdFixture::new();
        let scope = fx.read_scope();
        assert_eq!(
            classify_at("cd sub && cargo test", &fx.root, &scope),
            RouteDecision::Route {
                tool: "build_exec",
                args: json!({"argv": ["cargo", "test"], "cwd": "sub"}),
            }
        );
        // `;` is safe here — unlike #2550's boundary, which excluded it
        // because the strip was a lexical guess. If `std::fs::canonicalize`
        // succeeds, the `cd` succeeds, and the separator never changed
        // that answer.
        assert_eq!(
            classify_at("cd sub; cargo test", &fx.root, &scope),
            RouteDecision::Route {
                tool: "build_exec",
                args: json!({"argv": ["cargo", "test"], "cwd": "sub"}),
            }
        );
        let RouteDecision::Route { args, .. } =
            classify_at("cd sub && cargo test | tail -20", &fx.root, &scope)
        else {
            panic!("must route");
        };
        assert_eq!(args["cwd"], "sub");
        assert_eq!(args["trim"], json!({"mode": "tail", "n": 20}));
        // The workspace root itself — #2550's original case — now routes
        // via this SAME mechanism; `cwd` resolves to `.`.
        assert_eq!(
            classify_at(
                &format!("cd {} && cargo test", fx.root.display()),
                &fx.root,
                &scope
            ),
            RouteDecision::Route {
                tool: "build_exec",
                args: json!({"argv": ["cargo", "test"], "cwd": "."}),
            }
        );
        // The git read route folds the SAME `cwd`.
        assert_eq!(
            classify_at("cd sub && git status", &fx.root, &scope),
            RouteDecision::Route {
                tool: "git",
                args: json!({"op": "status", "cwd": "sub"}),
            }
        );
    }

    /// Everything the brief named as staying `Exec`: a target that does not
    /// exist, is a file not a directory, is outside the workspace (a `..`
    /// climb), `cd -`/`cd ~`, a quoted/globbed/metacharacter dir, two
    /// `cd`s, unquoted internal whitespace (bash's real `cd` sees TWO
    /// arguments there and fails — `&&` never runs the rest), and a
    /// non-routable `<rest>` even past a genuinely resolvable `cd`.
    #[test]
    fn cd_stays_exec_when_the_target_does_not_resolve_or_is_out_of_authority() {
        let fx = CdFixture::new();
        let scope = fx.read_scope();
        for cmd in [
            "cd missing && cargo test",
            "cd file.txt && cargo test",
            "cd ../outside-the-workspace && cargo test",
            "cd - && cargo test",
            "cd ~ && cargo test",
            "cd \"sub\" && cargo test",
            "cd su* && cargo test",
            "cd sub$X && cargo test",
            // Two `cd`s: `rest` after the first fold is `cd sub && cargo
            // test`, which still contains `&&` — `classify_stripped`'s
            // ordinary compound refusal catches it, no separate check.
            "cd sub && cd sub && cargo test",
            // A resolvable `cd`, but `<rest>` isn't routable — the fold
            // changes what's classified, not whether it routes.
            "cd sub && rm -rf x",
            // Unquoted internal whitespace: the ONLY correctly quoted
            // spelling is already refused by `BUILD_UNSAFE`'s `"`, so
            // without this refusal the broken command would route while
            // the correct one would not.
            "cd su b && cargo test",
        ] {
            assert_eq!(
                classify_at(cmd, &fx.root, &scope),
                RouteDecision::Exec,
                "{cmd}"
            );
        }
    }

    /// A symlink INSIDE the workspace whose target resolves OUTSIDE it
    /// stays `Exec` — `std::fs::canonicalize` follows the link, so the
    /// containment check sees the REAL destination, not the lexical path
    /// the model typed.
    #[cfg(not(windows))]
    #[test]
    fn cd_through_a_symlink_that_escapes_the_workspace_stays_exec() {
        let fx = CdFixture::new();
        let scope = fx.read_scope();
        // `std::env::temp_dir()` is an ANCESTOR of `fx.root` (tempfile
        // creates its dirs under it), so it is guaranteed to exist and to
        // be outside `fx.root` specifically.
        let outside = std::env::temp_dir();
        std::os::unix::fs::symlink(&outside, fx.root.join("escape")).expect("symlink");
        assert_eq!(
            classify_at("cd escape && cargo test", &fx.root, &scope),
            RouteDecision::Exec
        );
    }

    /// A resolvable directory OUTSIDE this call's fs-read fence (even
    /// though it is genuinely inside the workspace on disk) stays `Exec` —
    /// the fence, not just the workspace boundary, decides authority.
    #[test]
    fn cd_into_a_real_subdirectory_outside_the_fence_stays_exec() {
        let fx = CdFixture::new();
        // A fence that grants only a DIFFERENT root — `sub` is a real
        // directory inside the workspace, but outside THIS call's fence.
        let elsewhere = tempfile::TempDir::new().expect("tempdir");
        let scope = crate::caveats::Scope::only([elsewhere
            .path()
            .canonicalize()
            .expect("canonicalize")
            .to_string_lossy()
            .into_owned()]);
        assert_eq!(
            classify_at("cd sub && cargo test", &fx.root, &scope),
            RouteDecision::Exec
        );
    }

    /// #2551 round 2 BLOCKER (red first, real tempdir with `sub/x`/`sub/f`
    /// AND `x`/`f` at the root — distinct content, so reading/deleting the
    /// WRONG one is provably wrong): `cwd` is honoured ONLY by `build_exec`
    /// and `git` (`newt-git` reads it, `run_confined_build_lane` takes it as
    /// a real parameter) — `read_file`/`list_dir`/`delete_file` all join
    /// `workspace + path` and never look at `args["cwd"]`. Attaching `cwd`
    /// to those routes anyway would leave `cd sub && rm x` deleting
    /// `<root>/x` instead of `<root>/sub/x` — a wrong-file DELETE gated only
    /// by `fs_write` on the root, not on `sub`. Every non-build/git route
    /// with a resolved `cwd != "."` now stays `Exec`; `cwd == "."` (the
    /// workspace root itself) is harmless and still routes.
    #[test]
    fn a_resolved_cwd_never_reaches_a_route_that_does_not_honour_it() {
        let fx = CdFixture::new();
        let scope = fx.read_scope();
        for cmd in ["cd sub && rm x", "cd sub && cat f", "cd sub && ls"] {
            assert_eq!(
                classify_at(cmd, &fx.root, &scope),
                RouteDecision::Exec,
                "{cmd}"
            );
        }
        // The pre-existing sibling of the same bug: a `cwd` FIELD (not a
        // leading `cd`) on a non-build/git route used to route and
        // silently drop it.
        assert_eq!(
            RouteTable::builtin().classify_call(
                &json!({"command": "cat f", "cwd": "sub"}),
                &fx.root,
                &scope
            ),
            RouteDecision::Exec
        );
        // `cwd == "."` (the workspace root itself) is harmless — those
        // tools already run there, so nothing is silently dropped.
        // Not on Windows: the cd fold does not resolve there yet and stays
        // Exec (fail-closed); the refusals above still run on every platform.
        #[cfg(not(windows))]
        assert_eq!(
            classify_at(
                &format!("cd {} && cat f", fx.root.display()),
                &fx.root,
                &scope
            ),
            RouteDecision::Route {
                tool: "read_file",
                args: json!({ "path": "f" }),
            }
        );
    }

    /// #2551 round 2 should-fix (red first): the fence must hold on the
    /// CANONICAL path too, not just the lexical join. A fence granting only
    /// `sub` and a symlink `sub/link` → a REAL sibling directory `other`
    /// (never granted): `workspace.join("sub/link")` lexically starts with
    /// the granted `sub` prefix, but the directory it actually reaches is
    /// outside the fence. Checking only the lexical join would route with
    /// `cwd="other"` — a directory the fence never authorized.
    #[cfg(not(windows))]
    #[test]
    fn a_symlink_whose_real_target_is_outside_the_fence_stays_exec() {
        let fx = CdFixture::new();
        let other = fx.root.join("other");
        std::fs::create_dir(&other).expect("mkdir other");
        std::os::unix::fs::symlink(&other, fx.sub.join("link")).expect("symlink");
        // Grant ONLY `sub` — `other` is genuinely inside the WORKSPACE, but
        // never inside THIS call's fence.
        let scope = crate::caveats::Scope::only([fx.sub.to_string_lossy().into_owned()]);
        assert_eq!(
            classify_at("cd sub/link && cargo test", &fx.root, &scope),
            RouteDecision::Exec
        );
    }

    /// #2551 round 2 should-fix (dropped from #2550 round 2, re-added, red
    /// first): ANY `..` component in `<dir>` refuses outright, even one
    /// that would resolve harmlessly. `std::fs::canonicalize` resolves
    /// PHYSICALLY (follows every symlink to its real target); a real
    /// shell's `cd` defaults to LOGICAL (`-L`) and never re-resolves a
    /// symlink component once it has descended through it. With `link →
    /// <root>/a/b`, `cd link/..` lands at `<root>` in bash (logical: pop
    /// the textual `link` component) but at `<root>/a` if routed
    /// (physical: canonicalize resolves `link` first, then climbs one
    /// REAL level) — two different directories from the same command.
    /// Refusing any `..` sidesteps the divergence entirely rather than
    /// trying to emulate bash's logical resolution.
    #[test]
    fn a_parent_dir_component_in_the_cd_target_always_refuses() {
        let fx = CdFixture::new();
        let scope = fx.read_scope();
        // Even a `..` that would resolve harmlessly (back to a sibling
        // that IS granted) still refuses — the component itself is what's
        // refused, not its eventual resolution.
        assert_eq!(
            classify_at("cd sub/../sub && cargo test", &fx.root, &scope),
            RouteDecision::Exec
        );
        assert_eq!(
            classify_at("cd sub/.. && cargo test", &fx.root, &scope),
            RouteDecision::Exec
        );
    }

    /// #2551 round 3 should-fix: a folded leading `cd` resolves `<dir>`
    /// against `workspace`, but when the call ALSO carries a `cwd` field, a
    /// real shell resolves the relative `cd` against THAT directory, not the
    /// workspace root. `{command:"cd sub && cargo test", cwd:"other"}` used
    /// to route to `<root>/sub` while the shell would run in
    /// `<root>/other/sub` (or fail, if that path does not exist) — a
    /// wrong-directory build that still counted as a pass. Refuse instead of
    /// resolving the fold against the field: simpler, and the review's
    /// evidence never combines the two.
    #[test]
    fn a_cwd_field_alongside_a_folded_leading_cd_refuses() {
        let fx = CdFixture::new();
        let other = fx.root.join("other");
        std::fs::create_dir(&other).expect("mkdir other");
        std::fs::create_dir(other.join("sub")).expect("mkdir other/sub");
        let scope = fx.read_scope();
        let call = json!({ "command": "cd sub && cargo test", "cwd": "other" });
        assert_eq!(
            RouteTable::builtin().classify_call(&call, &fx.root, &scope),
            RouteDecision::Exec
        );
    }

    /// #2551 round 3 nit: the round-2 dual fence check compares a canonical
    /// candidate path against the UN-canonicalized `read_scope` roots, so a
    /// workspace reached through a symlinked path (macOS `/tmp` →
    /// `/private/tmp`) fails the canonical check for every `<dir>`,
    /// including `.` — no `cd` ever routes. Canonicalizing the scope's own
    /// roots before that comparison fixes it. Deliberately does NOT
    /// canonicalize the tempdir path up front (unlike `CdFixture::new`),
    /// so the workspace root passed to `classify_call` is itself the
    /// symlinked string this test is about.
    #[test]
    #[cfg(not(windows))]
    fn a_symlinked_workspace_root_still_folds_a_cd() {
        let real = tempfile::TempDir::new().expect("tempdir");
        let real_root = real.path().canonicalize().expect("canonicalize tempdir");
        std::fs::create_dir(real_root.join("sub")).expect("mkdir sub");
        let parent = real_root.parent().expect("tempdir has a parent");
        let link = parent.join(format!(
            "{}-link",
            real_root.file_name().unwrap().to_string_lossy()
        ));
        std::os::unix::fs::symlink(&real_root, &link).expect("symlink workspace root");
        let scope = crate::caveats::Scope::only([link.to_string_lossy().into_owned()]);
        let call = json!({ "command": "cd sub && cargo test" });
        assert_eq!(
            RouteTable::builtin().classify_call(&call, &link, &scope),
            RouteDecision::Route {
                tool: "build_exec",
                args: json!({ "argv": ["cargo", "test"], "cwd": "sub" }),
            }
        );
        std::fs::remove_file(&link).ok();
    }
}
