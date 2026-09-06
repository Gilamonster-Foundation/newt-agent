//! `/roadmap` — author and view a Roadmap→Phase→Plan→Task tree (#1030).
//!
//! Parsing, rendering, evaluation fact sources (git / `gh` / verify) and the
//! command handler. Lifted verbatim out of `lib.rs`; the session loop and the
//! slash dispatch that reaches this handler stay in `super`.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RoadmapCommand {
    /// `/roadmap` or `/roadmap show [id]` — render a roadmap's tree.
    Show(Option<String>),
    /// `/roadmap list` — list this workspace's roadmaps.
    List,
    /// `/roadmap new <title>` — create an empty roadmap and make it active.
    New(String),
    /// `/roadmap use <id>` — set the active roadmap.
    Use(String),
    /// `/roadmap add <kind> <title> [under <node-id>]` — append a node.
    Add {
        kind: newt_core::plan::NodeKind,
        title: String,
        under: Option<String>,
    },
    /// `/roadmap next` — the DFS cursor: report (and, if it is a bound Plan node,
    /// resume) the next-ready node.
    Next,
    /// `/roadmap bind [node-id]` — bind THIS conversation to a Plan node (default:
    /// the next-ready node) and mark it Running.
    Bind(Option<String>),
    /// `/roadmap done [node-id]` — mark a node Done (default: the node bound to
    /// this conversation) and advance the cursor.
    Done(Option<String>),
    /// `/roadmap eval [node-id]` — evaluate a node against OBJECTIVE git state
    /// (Task = commit+verify, Plan = children+verify); mark it Done if it passes.
    Eval(Option<String>),
    /// `/roadmap drive` — headless traversal: evaluate the cursor node and, while
    /// it closes, ripple completion up the tree, halting at the first node that
    /// still needs work. Closes only nodes whose OBJECTIVE evaluator passes.
    Drive,
    /// `/roadmap task <node-id> commit [<sha>]` (#1062) — bind a Task node to the
    /// commit that realizes it (default: current `HEAD`), setting
    /// `artifact_ref.commit`/`branch` so `/roadmap eval` closes the Task from git
    /// truth instead of a manual `/roadmap done`.
    TaskCommit { node: String, sha: Option<String> },
    /// `/roadmap issue <node-id> <number>` (#1083) — bind any node to the forge
    /// issue it realizes; `/roadmap eval` then additionally requires the issue
    /// CLOSED before the node may be Done (a verdict input, never a direct Done).
    IssueSet { node: String, number: u64 },
    /// `/roadmap export [path]` (#1082) — write the active roadmap to the
    /// on-repo TOML file (default `.newt/roadmap.toml`); the repo copy is the
    /// authority, the store row a working copy.
    Export(Option<String>),
    /// `/roadmap import [path]` (#1082) — load a roadmap file and upsert it by
    /// id into this workspace's store, then set it active. Fresh checkouts
    /// bootstrap their roadmap with this.
    Import(Option<String>),
}

fn parse_node_kind(s: &str) -> Option<newt_core::plan::NodeKind> {
    use newt_core::plan::NodeKind;
    match s.to_ascii_lowercase().as_str() {
        "roadmap" => Some(NodeKind::Roadmap),
        "phase" => Some(NodeKind::Phase),
        "plan" => Some(NodeKind::Plan),
        "task" => Some(NodeKind::Task),
        _ => None,
    }
}

pub(crate) fn parse_roadmap_command(input: &str) -> anyhow::Result<RoadmapCommand> {
    let body = input.trim().trim_start_matches('/').trim();
    let rest = body.strip_prefix("roadmap").map(str::trim).unwrap_or("");
    let mut parts = rest.split_whitespace();
    match parts.next() {
        // `tree` is `show`'s name for what it renders, and the word the
        // retired `/tree` verb used (#2009 PR12). Folding a verb should not
        // cost the operator its vocabulary — the pointer says `/roadmap tree`
        // because that is a thing you can type.
        None | Some("show" | "tree") => Ok(RoadmapCommand::Show(parts.next().map(str::to_string))),
        Some("list") => Ok(RoadmapCommand::List),
        Some("new") => {
            let title = parts.collect::<Vec<_>>().join(" ");
            if title.trim().is_empty() {
                anyhow::bail!("usage: /roadmap new <title>");
            }
            Ok(RoadmapCommand::New(title.trim().to_string()))
        }
        Some("use") => match parts.next() {
            Some(id) => Ok(RoadmapCommand::Use(id.to_string())),
            None => anyhow::bail!("usage: /roadmap use <id>"),
        },
        Some("export") => Ok(RoadmapCommand::Export(parts.next().map(str::to_string))),
        Some("import") => Ok(RoadmapCommand::Import(parts.next().map(str::to_string))),
        Some("add") => {
            let kind = parts.next().and_then(parse_node_kind).ok_or_else(|| {
                anyhow::anyhow!(
                    "usage: /roadmap add <roadmap|phase|plan|task> <title> [under <node-id>]"
                )
            })?;
            let joined = parts.collect::<Vec<_>>().join(" ");
            let (title, under) = match joined.rsplit_once(" under ") {
                Some((t, u)) => (t.trim().to_string(), Some(u.trim().to_string())),
                None => (joined.trim().to_string(), None),
            };
            if title.is_empty() {
                anyhow::bail!("usage: /roadmap add <kind> <title> [under <node-id>]");
            }
            Ok(RoadmapCommand::Add { kind, title, under })
        }
        Some("next") | Some("work") => Ok(RoadmapCommand::Next),
        Some("bind") => Ok(RoadmapCommand::Bind(parts.next().map(str::to_string))),
        Some("done") => Ok(RoadmapCommand::Done(parts.next().map(str::to_string))),
        Some("eval") => Ok(RoadmapCommand::Eval(parts.next().map(str::to_string))),
        Some("drive") => Ok(RoadmapCommand::Drive),
        Some("task") => {
            let node = parts
                .next()
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("usage: /roadmap task <node-id> commit [<sha>]"))?;
            match parts.next() {
                Some("commit") => Ok(RoadmapCommand::TaskCommit {
                    node,
                    sha: parts.next().map(str::to_string),
                }),
                _ => anyhow::bail!("usage: /roadmap task <node-id> commit [<sha>]"),
            }
        }
        Some("issue") => {
            let usage = || anyhow::anyhow!("usage: /roadmap issue <node-id> <number>");
            let node = parts.next().map(str::to_string).ok_or_else(usage)?;
            let number = parts
                .next()
                .and_then(|s| s.trim_start_matches('#').parse::<u64>().ok())
                .ok_or_else(usage)?;
            Ok(RoadmapCommand::IssueSet { node, number })
        }
        Some(other) => {
            anyhow::bail!(
                "unknown /roadmap subcommand `{other}` \
                 (try: list, new, show, use, add, next, bind, done, eval, drive, task, \
                 issue, export, import)"
            )
        }
    }
}

fn node_kind_label(kind: newt_core::plan::NodeKind) -> &'static str {
    use newt_core::plan::NodeKind;
    match kind {
        NodeKind::Roadmap => "roadmap",
        NodeKind::Phase => "phase",
        NodeKind::Plan => "plan",
        NodeKind::Task => "task",
    }
}

fn node_status_glyph(status: newt_core::plan::SubtaskStatus) -> &'static str {
    use newt_core::plan::SubtaskStatus;
    match status {
        SubtaskStatus::Pending => "○",
        SubtaskStatus::Running => "◐",
        SubtaskStatus::Done => "✓",
        SubtaskStatus::Failed => "✗",
    }
}

/// The next auto node id for a roadmap (`node-N`), skipping any already taken.
pub(crate) fn next_roadmap_node_id(tree: &newt_core::plan::Plan) -> String {
    let mut n = tree.subtasks.len() + 1;
    loop {
        let id = format!("node-{n}");
        if tree.subtask(&id).is_none() {
            return id;
        }
        n += 1;
    }
}

/// The #1030 DFS cursor: the node to act on now — the first Running node (work
/// in progress) if any, else the next-ready (Pending) node. `/roadmap next` and
/// the `/tree` `▶` marker both use this, so an in-progress node stays the cursor
/// until it is marked done rather than the cursor jumping past it.
fn roadmap_cursor(tree: &newt_core::plan::Plan) -> Option<&newt_core::plan::Subtask> {
    tree.subtasks
        .iter()
        .find(|s| s.status == newt_core::plan::SubtaskStatus::Running)
        .or_else(|| tree.next_ready_node())
}

/// Render a roadmap's tree as a depth-first plain-scroller outline (#1030): one
/// line per node — status glyph, kind, id, instruction — indented by depth.
pub(crate) fn render_roadmap_tree(roadmap: &newt_core::Roadmap) -> String {
    let mut out = format!(
        "Roadmap: {}  [{}]",
        roadmap.title,
        short_conversation_id(&roadmap.id)
    );
    if roadmap.tree.subtasks.is_empty() {
        out.push_str("\n  (no nodes yet — add one with /roadmap add <phase|plan|task> <title>)");
        return out;
    }
    // #1030 DFS cursor: the next node to act on (see /roadmap next).
    let cursor = roadmap_cursor(&roadmap.tree).map(|n| n.id.clone());
    fn walk(
        plan: &newt_core::plan::Plan,
        node: &newt_core::plan::Subtask,
        depth: usize,
        cursor: Option<&str>,
        out: &mut String,
    ) {
        // Depth cap: a real roadmap is shallow; this bounds a hand-corrupted
        // tree whose soft parent pointers form a cycle so render can't overflow
        // the stack (authoring via /roadmap add can't create one — a new node is
        // always a leaf — but a hand-edited tree blob could).
        if depth > 64 {
            out.push_str("\n  … (tree too deep — possible cycle in parent pointers)");
            return;
        }
        let mark = if Some(node.id.as_str()) == cursor {
            "▶"
        } else {
            " "
        };
        out.push_str(&format!(
            "\n{}{} {} {} [{}]  {}",
            "  ".repeat(depth + 1),
            mark,
            node_status_glyph(node.status),
            node_kind_label(node.kind),
            node.id,
            node.instruction,
        ));
        for child in plan.children(&node.id) {
            walk(plan, child, depth + 1, cursor, out);
        }
    }
    for root in roadmap.tree.roots() {
        walk(&roadmap.tree, root, 0, cursor.as_deref(), &mut out);
    }
    out.push_str("\n  ▶ next · ○ pending · ◐ running · ✓ done · ✗ failed");
    out
}

/// Resolve a roadmap id or unique short-prefix against this workspace's
/// roadmaps (roadmaps have no FTS `resolve_id`; scan the small list).
fn resolve_roadmap_id(
    store: &newt_core::ConversationStore,
    id_or_prefix: &str,
) -> anyhow::Result<String> {
    let matches: Vec<String> = store
        .list_roadmaps()?
        .into_iter()
        .map(|r| r.id)
        .filter(|id| id == id_or_prefix || id.starts_with(id_or_prefix))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => anyhow::bail!("no roadmap matches `{id_or_prefix}`"),
        many => anyhow::bail!(
            "`{id_or_prefix}` is ambiguous ({} roadmaps match)",
            many.len()
        ),
    }
}

/// The result of a /roadmap subcommand: a message to print, plus an optional
/// conversation to make active — #1030 resume-to-cursor: `/roadmap next` resumes
/// a bound Plan node's conversation.
#[derive(Debug)]
pub(crate) struct RoadmapOutcome {
    pub(crate) message: String,
    pub(crate) switch_to: Option<String>,
}

impl RoadmapOutcome {
    fn msg(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            switch_to: None,
        }
    }
}

/// One-line summary of the next-ready node, for `/roadmap done` / `next` feedback.
fn roadmap_next_hint(tree: &newt_core::plan::Plan) -> String {
    match tree.next_ready_node() {
        Some(n) => format!(
            "Next ready: {} node [{}] — {}",
            node_kind_label(n.kind),
            n.id,
            n.instruction
        ),
        None => "Roadmap complete (or all remaining nodes are blocked).".to_string(),
    }
}

/// Production [`GitFacts`](newt_core::roadmap_eval::GitFacts): a node's commit is
/// "present" iff it appears in the repo's HEAD history (checked via newt-git).
/// A non-repo workspace has no engine, so every commit reads absent.
struct LocalGitFacts {
    engine: Option<newt_git::GitEngine>,
}

impl LocalGitFacts {
    fn open(workspace: &str) -> Self {
        Self {
            engine: newt_git::GitEngine::open(std::path::Path::new(workspace)).ok(),
        }
    }
}

impl newt_core::roadmap_eval::GitFacts for LocalGitFacts {
    fn commit_present(&self, commit: &str, _branch: Option<&str>) -> bool {
        let Some(engine) = &self.engine else {
            return false;
        };
        engine
            .log(&newt_core::git_caveats::GitCaveats::read_only(), 1000)
            .map(|commits| {
                commits
                    .iter()
                    .any(|c| c.id.starts_with(commit) || c.short_id.starts_with(commit))
            })
            .unwrap_or(false)
    }
}

/// Production [`VerifyRunner`](newt_core::roadmap_eval::VerifyRunner): run a
/// node's verify command as a subprocess in the workspace, success = pass.
struct CommandVerifyRunner {
    workspace: std::path::PathBuf,
}

impl newt_core::roadmap_eval::VerifyRunner for CommandVerifyRunner {
    fn run(&self, cmd: &str) -> bool {
        // A roadmap node's `verify` string is loaded from the on-repo
        // `.newt/roadmap.toml`, so it is attacker-influenced and runs CONFINED
        // through `ConstrainedExecutor` (P4): an env-empty child (only PATH/HOME/
        // TMPDIR granted — no credentials, #8), fs fenced to the workspace + the
        // operator's Cargo cache (a verify may `cargo test`) with reads calibrated
        // to the toolchain/cache set, network denied (#9), and fail-closed off the
        // kernel fence (#10). No longer a raw `sh -c` on the host.
        use newt_core::confined_exec::{
            build_tool_caveats_with_writes, ConstrainedExecutor, ExecOrigin, ExecRequest,
        };
        let mut extra_writes = Vec::new();
        if let Some(cargo_home) = std::env::var_os("CARGO_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cargo"))
            })
        {
            extra_writes.push(cargo_home.to_string_lossy().into_owned());
        }
        #[cfg(windows)]
        let (program, args): (&str, [String; 2]) = ("cmd", ["/C".into(), cmd.into()]);
        #[cfg(not(windows))]
        let (program, args): (&str, [String; 2]) = ("sh", ["-c".into(), cmd.into()]);

        let mut req = ExecRequest::new(
            ExecOrigin::AgentInfluenced,
            program,
            args,
            &self.workspace,
            build_tool_caveats_with_writes(&self.workspace, &extra_writes),
        )
        .env("TMPDIR", self.workspace.to_string_lossy());
        // Real HOME so a `cargo` verify finds ~/.cargo (read via the calibrated
        // set, written via the grant above); nothing credential-bearing crosses.
        if let Some(home) = std::env::var_os("HOME") {
            req = req.env("HOME", home.to_string_lossy());
        }
        if let Ok(path) = std::env::var("PATH") {
            req = req.env("PATH", path);
        }
        ConstrainedExecutor::run(&req)
            .map(|o| o.success)
            .unwrap_or(false)
    }
}

/// Production [`ForgeFacts`](newt_core::roadmap_eval::ForgeFacts): a Phase's PR
/// merge state via `gh pr view`. `None` (Unsupported) when `gh` is missing, the
/// workspace has no GitHub remote, or the call fails — never a false "merged".
struct GhForgeFacts {
    workspace: std::path::PathBuf,
}

impl newt_core::roadmap_eval::ForgeFacts for GhForgeFacts {
    fn pr_merged(&self, pr: u64) -> Option<bool> {
        let out = std::process::Command::new("gh")
            .args([
                "pr",
                "view",
                &pr.to_string(),
                "--json",
                "state",
                "-q",
                ".state",
            ])
            .current_dir(&self.workspace)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let state = String::from_utf8_lossy(&out.stdout);
        Some(state.trim() == "MERGED")
    }

    fn issue_closed(&self, issue: u64) -> Option<bool> {
        // #1083: CLOSED regardless of stateReason — a not-planned close also
        // releases the gate; the node's other facts still decide Done.
        let out = std::process::Command::new("gh")
            .args([
                "issue",
                "view",
                &issue.to_string(),
                "--json",
                "state",
                "-q",
                ".state",
            ])
            .current_dir(&self.workspace)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let state = String::from_utf8_lossy(&out.stdout);
        Some(state.trim() == "CLOSED")
    }
}

/// Production [`CiFacts`](newt_core::roadmap_eval::CiFacts): the latest pipeline
/// run's conclusion via `gh run list`. `None` (Unsupported) when `gh`/CI is
/// unavailable or no run has concluded yet — never a false "green".
struct GhCiFacts {
    workspace: std::path::PathBuf,
}

impl newt_core::roadmap_eval::CiFacts for GhCiFacts {
    fn pipelines_green(&self) -> Option<bool> {
        let out = std::process::Command::new("gh")
            .args([
                "run",
                "list",
                "--limit",
                "1",
                "--json",
                "conclusion",
                "-q",
                ".[0].conclusion",
            ])
            .current_dir(&self.workspace)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let concl = String::from_utf8_lossy(&out.stdout);
        let c = concl.trim();
        if c.is_empty() {
            return None; // no runs, or the latest is still in progress
        }
        Some(c == "success")
    }
}

/// The production objective-fact sources for `workspace`, owned so the caller
/// can borrow them into a `roadmap_eval::Facts` bundle. Git reads the repo, the
/// verify runner shells a subprocess, and the forge/CI sources shell `gh`; any
/// unreachable source degrades to Unsupported (never a false Done). Shared by
/// `/roadmap eval` (one node) and `/roadmap drive` (the whole cursor cascade).
fn production_fact_sources(
    workspace: &str,
) -> (LocalGitFacts, CommandVerifyRunner, GhForgeFacts, GhCiFacts) {
    (
        LocalGitFacts::open(workspace),
        CommandVerifyRunner {
            workspace: std::path::PathBuf::from(workspace),
        },
        GhForgeFacts {
            workspace: std::path::PathBuf::from(workspace),
        },
        GhCiFacts {
            workspace: std::path::PathBuf::from(workspace),
        },
    )
}

/// #1062 auto-capture — the current git HEAD (short oid), or `None` when the
/// workspace isn't a repo or HEAD is unborn. Read-only via newt-git. Snapshotted
/// before a turn so the after-turn hook can tell whether a commit landed.
pub(crate) fn git_head_short(workspace: &str) -> Option<String> {
    newt_git::GitEngine::open(std::path::Path::new(workspace))
        .ok()?
        .status(&newt_core::git_caveats::GitCaveats::read_only())
        .ok()?
        .head
}

/// #1062 auto-capture, PURE core: which Task should absorb the turn's commit? If
/// HEAD advanced (`head_now != head_before`) AND this conversation is bound to a
/// Plan node with a next-uncaptured Task, return that Task's id. No git/store —
/// the caller reads HEAD and persists; this is the unit-testable decision.
pub(crate) fn autocapture_target(
    tree: &newt_core::plan::Plan,
    conversation_id: &str,
    head_before: Option<&str>,
    head_now: &str,
) -> Option<String> {
    if Some(head_now) == head_before {
        return None; // no new commit this turn
    }
    let plan_node = tree.subtasks.iter().find(|s| {
        s.conversation_id.as_deref() == Some(conversation_id)
            && s.kind == newt_core::plan::NodeKind::Plan
    })?;
    tree.next_uncaptured_task_under(&plan_node.id)
        .map(|t| t.id.clone())
}

/// #1062 auto-capture: after a bound conversation's turn, if a commit landed,
/// attribute it to the bound Plan's next-uncaptured Task and persist. Returns a
/// one-line notice, or `None` (no active roadmap / no new commit / not bound to a
/// Plan / no ready Task). Orchestration around [`autocapture_target`]; the git
/// read + [`ConversationStore::update_roadmap`] live here.
pub(crate) fn autocapture_commit_after_turn(
    store: &newt_core::ConversationStore,
    active_roadmap_id: &Option<String>,
    active_conversation_id: &str,
    workspace: &str,
    head_before: Option<&str>,
) -> Option<String> {
    let roadmap_id = active_roadmap_id.as_deref()?;
    let status = newt_git::GitEngine::open(std::path::Path::new(workspace))
        .ok()?
        .status(&newt_core::git_caveats::GitCaveats::read_only())
        .ok()?;
    let head_now = status.head?;
    let mut rm = store.load_roadmap(roadmap_id).ok().flatten()?;
    let task_id = autocapture_target(&rm.tree, active_conversation_id, head_before, &head_now)?;
    rm.tree
        .set_artifact_commit(&task_id, &head_now, status.branch.as_deref());
    store.update_roadmap(roadmap_id, &rm.tree).ok()?;
    let short = &head_now[..head_now.len().min(8)];
    Some(format!(
        "⟲ auto-captured commit {short} → task [{task_id}] — /roadmap eval closes it from git."
    ))
}

/// #1082: resolve where the roadmap file lives — an explicit `path` argument
/// (workspace-relative unless absolute) or the checked-in default
/// [`newt_core::roadmap_file::DEFAULT_ROADMAP_FILE`].
pub(crate) fn roadmap_file_path(workspace: &str, arg: Option<&str>) -> std::path::PathBuf {
    let base = std::path::Path::new(workspace);
    match arg {
        Some(p) if std::path::Path::new(p).is_absolute() => std::path::PathBuf::from(p),
        Some(p) => base.join(p),
        None => base.join(newt_core::roadmap_file::DEFAULT_ROADMAP_FILE),
    }
}

/// #1082 `/roadmap export` body. `write` is the injected file edge (real
/// `std::fs` in the command arm, in-memory in the unit tier) so the logic
/// stays in the fully-mocked tier.
pub(crate) fn export_roadmap_to(
    store: &newt_core::ConversationStore,
    id: &str,
    path: &std::path::Path,
    write: &dyn Fn(&std::path::Path, &str) -> std::io::Result<()>,
) -> anyhow::Result<RoadmapOutcome> {
    let rm = store
        .load_roadmap(id)?
        .ok_or_else(|| anyhow::anyhow!("active roadmap `{id}` not found in this workspace"))?;
    let nodes = rm.tree.subtasks.len();
    let file = newt_core::roadmap_file::RoadmapFile::new(rm.id, rm.title.clone(), rm.tree);
    let text = file.to_toml_string()?;
    write(path, &text)
        .map_err(|e| anyhow::anyhow!("cannot write roadmap file {}: {e}", path.display()))?;
    Ok(RoadmapOutcome::msg(format!(
        "Exported roadmap \"{}\" [{}] → {} ({nodes} nodes). Check it in — the repo \
         copy is the authority; /roadmap import loads it on any checkout.",
        rm.title,
        short_conversation_id(id),
        path.display()
    )))
}

/// #1082 `/roadmap import` body. Parses BEFORE touching the store — a corrupt
/// or future-versioned file is a hard error that leaves the working copy
/// untouched. Upserts by the file's roadmap id (same id → update in place)
/// and sets the roadmap active. `read` is the injected file edge.
pub(crate) fn import_roadmap_from(
    store: &newt_core::ConversationStore,
    active_roadmap_id: &mut Option<String>,
    path: &std::path::Path,
    read: &dyn Fn(&std::path::Path) -> std::io::Result<String>,
) -> anyhow::Result<RoadmapOutcome> {
    let text = read(path).map_err(|e| {
        anyhow::anyhow!(
            "cannot read roadmap file {}: {e} — /roadmap export writes one, or pass a \
             path: /roadmap import <path>",
            path.display()
        )
    })?;
    let file = newt_core::roadmap_file::RoadmapFile::from_toml_str(&text)?;
    let existed = store.load_roadmap(&file.id)?.is_some();
    store.create_roadmap(&file.id, &file.title, &file.tree)?;
    *active_roadmap_id = Some(file.id.clone());
    Ok(RoadmapOutcome::msg(format!(
        "Imported roadmap \"{}\" [{}] from {} ({} nodes, {}) and set it active.",
        file.title,
        short_conversation_id(&file.id),
        path.display(),
        file.tree.subtasks.len(),
        if existed {
            "updated existing"
        } else {
            "created new"
        }
    )))
}

/// The roadmap subcommands that only READ.
///
/// Used by the retired `/tree` and `/plan` doors: **a retired READ may still
/// read, a retired MUTATOR must redirect** — the rule `/thinking` set and
/// `/conversation` applied per subcommand (#2009 PR6b). `/roadmap` is both, so
/// the door has to be decided per subcommand rather than per verb, or retiring
/// it either breaks `/tree` on a pipe or lets `/plan done` mutate through a
/// shim that is supposed to be dying.
pub(crate) fn roadmap_subcommand_reads(rest: &str) -> bool {
    matches!(
        rest.split_whitespace().next(),
        None | Some("show" | "tree" | "list")
    )
}

pub(crate) fn handle_roadmap_command(
    input: &str,
    store: &newt_core::ConversationStore,
    active_roadmap_id: &mut Option<String>,
    active_conversation_id: &str,
    workspace: &str,
) -> anyhow::Result<RoadmapOutcome> {
    // The active roadmap id, or a friendly error naming how to get one.
    let require_active = |active: &Option<String>| -> anyhow::Result<String> {
        active.clone().ok_or_else(|| {
            anyhow::anyhow!("no active roadmap — /roadmap new <title> or /roadmap use <id>")
        })
    };
    match parse_roadmap_command(input)? {
        RoadmapCommand::List => {
            let roadmaps = store.list_roadmaps()?;
            if roadmaps.is_empty() {
                return Ok(RoadmapOutcome::msg(
                    "No roadmaps yet — create one with /roadmap new <title>.",
                ));
            }
            let mut out = String::from("Roadmaps (most recently updated first):");
            for r in &roadmaps {
                let marker = if active_roadmap_id.as_deref() == Some(r.id.as_str()) {
                    "▶"
                } else {
                    " "
                };
                out.push_str(&format!(
                    "\n  {} {}  {}  ({} nodes)",
                    marker,
                    short_conversation_id(&r.id),
                    r.title,
                    r.node_count,
                ));
            }
            out.push_str(
                "\nView with /roadmap show <id> or /tree; set active with /roadmap use <id>.",
            );
            Ok(RoadmapOutcome::msg(out))
        }
        RoadmapCommand::New(title) => {
            let id = newt_core::new_conversation_id();
            store.create_roadmap(&id, &title, &newt_core::plan::Plan::default())?;
            *active_roadmap_id = Some(id.clone());
            Ok(RoadmapOutcome::msg(format!(
                "Created roadmap \"{title}\" [{}] and set it active. Add nodes with \
                 /roadmap add <kind> <title>; view with /tree.",
                short_conversation_id(&id)
            )))
        }
        RoadmapCommand::Use(id_or_prefix) => {
            let id = resolve_roadmap_id(store, &id_or_prefix)?;
            *active_roadmap_id = Some(id.clone());
            Ok(RoadmapOutcome::msg(format!(
                "Active roadmap set to [{}].",
                short_conversation_id(&id)
            )))
        }
        RoadmapCommand::Export(arg) => {
            let id = require_active(active_roadmap_id)?;
            let path = roadmap_file_path(workspace, arg.as_deref());
            export_roadmap_to(store, &id, &path, &|p, s| {
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(p, s)
            })
        }
        RoadmapCommand::Import(arg) => {
            let path = roadmap_file_path(workspace, arg.as_deref());
            import_roadmap_from(store, active_roadmap_id, &path, &|p| {
                std::fs::read_to_string(p)
            })
        }
        RoadmapCommand::Show(maybe) => {
            let id = match maybe {
                Some(p) => resolve_roadmap_id(store, &p)?,
                None => require_active(active_roadmap_id)?,
            };
            let rm = store.load_roadmap(&id)?.ok_or_else(|| {
                anyhow::anyhow!("roadmap [{}] not found", short_conversation_id(&id))
            })?;
            Ok(RoadmapOutcome::msg(render_roadmap_tree(&rm)))
        }
        RoadmapCommand::Add { kind, title, under } => {
            let id = require_active(active_roadmap_id)?;
            let mut rm = store.load_roadmap(&id)?.ok_or_else(|| {
                anyhow::anyhow!("active roadmap [{}] not found", short_conversation_id(&id))
            })?;
            let parent = match under {
                Some(p) => {
                    if rm.tree.subtask(&p).is_none() {
                        anyhow::bail!("no node `{p}` in this roadmap (see /tree)");
                    }
                    Some(p)
                }
                None => None,
            };
            let node_id = next_roadmap_node_id(&rm.tree);
            rm.tree.subtasks.push(newt_core::plan::Subtask::node(
                &node_id, title, kind, parent,
            ));
            store.update_roadmap(&id, &rm.tree)?;
            Ok(RoadmapOutcome::msg(format!(
                "Added {} node [{}]. /tree to view.",
                node_kind_label(kind),
                node_id
            )))
        }
        RoadmapCommand::Next => {
            let id = require_active(active_roadmap_id)?;
            let rm = store.load_roadmap(&id)?.ok_or_else(|| {
                anyhow::anyhow!("active roadmap [{}] not found", short_conversation_id(&id))
            })?;
            match roadmap_cursor(&rm.tree) {
                None => Ok(RoadmapOutcome::msg(
                    "Roadmap complete (or all remaining nodes are blocked).",
                )),
                Some(node) if node.kind == newt_core::plan::NodeKind::Plan => {
                    match &node.conversation_id {
                        // Bound Plan node → resume-to-cursor (switch to its conversation).
                        Some(cid) => Ok(RoadmapOutcome {
                            message: format!(
                                "Resuming plan node [{}] — {}",
                                node.id, node.instruction
                            ),
                            switch_to: Some(cid.clone()),
                        }),
                        None => Ok(RoadmapOutcome::msg(format!(
                            "Next: plan node [{}] — {}. Bind this conversation to it with \
                             /roadmap bind.",
                            node.id, node.instruction
                        ))),
                    }
                }
                Some(node) => Ok(RoadmapOutcome::msg(format!(
                    "Next ready: {} node [{}] — {}. Mark it done with /roadmap done [{}].",
                    node_kind_label(node.kind),
                    node.id,
                    node.instruction,
                    node.id
                ))),
            }
        }
        RoadmapCommand::Bind(maybe_node) => {
            let id = require_active(active_roadmap_id)?;
            let mut rm = store.load_roadmap(&id)?.ok_or_else(|| {
                anyhow::anyhow!("active roadmap [{}] not found", short_conversation_id(&id))
            })?;
            let node_id = match maybe_node {
                Some(n) => {
                    if rm.tree.subtask(&n).is_none() {
                        anyhow::bail!("no node `{n}` in this roadmap (see /tree)");
                    }
                    n
                }
                None => rm
                    .tree
                    .next_ready_node()
                    .map(|n| n.id.clone())
                    .ok_or_else(|| {
                        anyhow::anyhow!("no ready node to bind — the roadmap may be complete")
                    })?,
            };
            if let Some(node) = rm.tree.subtasks.iter_mut().find(|s| s.id == node_id) {
                node.conversation_id = Some(active_conversation_id.to_string());
                node.status = newt_core::plan::SubtaskStatus::Running;
            }
            store.update_roadmap(&id, &rm.tree)?;
            store.link_conversation_to_node(
                active_conversation_id,
                Some(id.as_str()),
                Some(node_id.as_str()),
            )?;
            Ok(RoadmapOutcome::msg(format!(
                "Bound this conversation to node [{node_id}] (now running). /end or /roadmap done \
                 [{node_id}] when the node is complete."
            )))
        }
        RoadmapCommand::Done(maybe_node) => {
            let id = require_active(active_roadmap_id)?;
            let mut rm = store.load_roadmap(&id)?.ok_or_else(|| {
                anyhow::anyhow!("active roadmap [{}] not found", short_conversation_id(&id))
            })?;
            let node_id = match maybe_node {
                Some(n) => {
                    if rm.tree.subtask(&n).is_none() {
                        anyhow::bail!("no node `{n}` in this roadmap (see /tree)");
                    }
                    n
                }
                None => rm
                    .tree
                    .subtasks
                    .iter()
                    .find(|s| s.conversation_id.as_deref() == Some(active_conversation_id))
                    .map(|s| s.id.clone())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no node is bound to this conversation — name one: /roadmap done <node-id>"
                        )
                    })?,
            };
            rm.tree
                .mark(&node_id, newt_core::plan::SubtaskStatus::Done, None);
            store.update_roadmap(&id, &rm.tree)?;
            Ok(RoadmapOutcome::msg(format!(
                "Marked node [{node_id}] done. {}",
                roadmap_next_hint(&rm.tree)
            )))
        }
        RoadmapCommand::Eval(maybe_node) => {
            let id = require_active(active_roadmap_id)?;
            let mut rm = store.load_roadmap(&id)?.ok_or_else(|| {
                anyhow::anyhow!("active roadmap [{}] not found", short_conversation_id(&id))
            })?;
            let node_id = match maybe_node {
                Some(n) => {
                    if rm.tree.subtask(&n).is_none() {
                        anyhow::bail!("no node `{n}` in this roadmap (see /tree)");
                    }
                    n
                }
                None => roadmap_cursor(&rm.tree)
                    .map(|n| n.id.clone())
                    .ok_or_else(|| {
                        anyhow::anyhow!("no node to evaluate — the roadmap may be complete")
                    })?,
            };
            let node = rm.tree.subtask(&node_id).cloned().expect("resolved above");
            // Objective evaluation: git for the commit, a subprocess for verify,
            // `gh` for the phase PR and the roadmap's CI. Any missing source
            // yields Unsupported, never a false Done.
            let (git, verify, forge, ci) = production_fact_sources(workspace);
            let facts = newt_core::roadmap_eval::Facts {
                git: &git,
                verify: &verify,
                forge: &forge,
                ci: &ci,
            };
            match newt_core::roadmap_eval::evaluate(&node, &rm.tree, &facts) {
                newt_core::roadmap_eval::NodeVerdict::Done => {
                    rm.tree
                        .mark(&node_id, newt_core::plan::SubtaskStatus::Done, None);
                    store.update_roadmap(&id, &rm.tree)?;
                    Ok(RoadmapOutcome::msg(format!(
                        "✓ node [{node_id}] evaluates DONE — marked done. {}",
                        roadmap_next_hint(&rm.tree)
                    )))
                }
                newt_core::roadmap_eval::NodeVerdict::NotYet(reason) => Ok(RoadmapOutcome::msg(
                    format!("node [{node_id}] not done yet: {reason}"),
                )),
                newt_core::roadmap_eval::NodeVerdict::Unsupported(reason) => {
                    Ok(RoadmapOutcome::msg(format!("node [{node_id}]: {reason}")))
                }
            }
        }
        RoadmapCommand::Drive => {
            use newt_core::roadmap_eval::DriveStep;
            let id = require_active(active_roadmap_id)?;
            let mut rm = store.load_roadmap(&id)?.ok_or_else(|| {
                anyhow::anyhow!("active roadmap [{}] not found", short_conversation_id(&id))
            })?;
            let (git, verify, forge, ci) = production_fact_sources(workspace);
            let facts = newt_core::roadmap_eval::Facts {
                git: &git,
                verify: &verify,
                forge: &forge,
                ci: &ci,
            };
            let steps = newt_core::roadmap_eval::drive_to_fixpoint(&mut rm.tree, &facts);
            // Persist whatever the cascade closed even if it later halted — the
            // Advanced marks are real, objective completions.
            store.update_roadmap(&id, &rm.tree)?;
            let advanced = steps
                .iter()
                .filter(|s| matches!(s, DriveStep::Advanced { .. }))
                .count();
            let mut out = String::new();
            for step in &steps {
                match step {
                    DriveStep::Advanced { node } => {
                        out.push_str(&format!("✓ advanced [{node}]\n"));
                    }
                    DriveStep::Blocked { node, reason } => {
                        out.push_str(&format!("⏸ blocked at [{node}]: {reason}\n"));
                    }
                    DriveStep::Complete => out.push_str("✓ roadmap complete\n"),
                }
            }
            out.push_str(&format!(
                "\nDrove {advanced} node(s) to done. {}",
                roadmap_next_hint(&rm.tree)
            ));
            Ok(RoadmapOutcome::msg(out))
        }
        RoadmapCommand::TaskCommit { node, sha } => {
            let id = require_active(active_roadmap_id)?;
            let mut rm = store.load_roadmap(&id)?.ok_or_else(|| {
                anyhow::anyhow!("active roadmap [{}] not found", short_conversation_id(&id))
            })?;
            let target = rm
                .tree
                .subtask(&node)
                .ok_or_else(|| anyhow::anyhow!("no node `{node}` in this roadmap (see /tree)"))?;
            if target.kind != newt_core::plan::NodeKind::Task {
                anyhow::bail!(
                    "node [{node}] is a {} — only a Task binds a commit",
                    node_kind_label(target.kind)
                );
            }
            // Resolve the commit: the given sha, or the workspace's current HEAD.
            let engine = newt_git::GitEngine::open(std::path::Path::new(workspace)).ok();
            let status = engine.as_ref().and_then(|e| {
                e.status(&newt_core::git_caveats::GitCaveats::read_only())
                    .ok()
            });
            let branch = status.as_ref().and_then(|s| s.branch.clone());
            let commit = match sha {
                Some(s) => s,
                None => status
                    .as_ref()
                    .and_then(|s| s.head.clone())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no HEAD to bind (not a git repo, or an unborn HEAD) — \
                             make a commit first, or pass one: /roadmap task {node} commit <sha>"
                        )
                    })?,
            };
            rm.tree
                .set_artifact_commit(&node, &commit, branch.as_deref());
            store.update_roadmap(&id, &rm.tree)?;
            let short = commit.get(..8).unwrap_or(&commit);
            let on = branch.map(|b| format!(" on {b}")).unwrap_or_default();
            Ok(RoadmapOutcome::msg(format!(
                "Bound task [{node}] to commit {short}{on}. \
                 /roadmap eval [{node}] now checks it against git."
            )))
        }
        RoadmapCommand::IssueSet { node, number } => {
            let id = require_active(active_roadmap_id)?;
            let mut rm = store.load_roadmap(&id)?.ok_or_else(|| {
                anyhow::anyhow!("active roadmap [{}] not found", short_conversation_id(&id))
            })?;
            if rm.tree.subtask(&node).is_none() {
                anyhow::bail!("no node `{node}` in this roadmap (see /tree)");
            }
            rm.tree.set_artifact_issue(&node, number);
            store.update_roadmap(&id, &rm.tree)?;
            Ok(RoadmapOutcome::msg(format!(
                "Bound [{node}] to issue #{number}. \
                 /roadmap eval [{node}] now also requires it CLOSED before Done."
            )))
        }
    }
}
