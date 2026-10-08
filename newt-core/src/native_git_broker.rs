//! Native Git retains its CLI, index, author selection, hooks, and streams.
//! Only message policy, signing, and verified publication cross to the host.
//!
//! All repository probes use the original invocation's confined executor. The
//! helper directory is a read-only mechanism resource outside child writes.
//! Ambient sessions attenuate writes for the commit invocation to protect it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_bridle::{ToolContext, ToolError, ToolResult};
use agent_bridle_tool_shell::{
    BrokerCommand, BrokerControl, BrokerPeer, BrokerProcess, BrokerSession, CommandBroker,
    PreparedBrokerCommand,
};
use agent_toolchain::native_git::helper::{decode_request, HookRequest, HookResponse};
use agent_toolchain::native_git::{CommitBroker, CommitPolicy, MAX_COMMIT_BYTES};
use content_addressable::ContentAddressable;

#[path = "native_git_broker_common_view.rs"]
mod common_view;

const BROKER_FD: i32 = 198;
const FD_ENV: &str = "NEWT_NATIVE_GIT_BROKER_FD";
const HOOKS_ENV: &str = "NEWT_NATIVE_GIT_ORIGINAL_HOOKS";
const HOOK_NAMES: &[&str] = &[
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "post-index-change",
    "post-rewrite",
    "reference-transaction",
];

fn denied(error: impl std::fmt::Display) -> ToolError {
    ToolError::denied(format!("native Git commit: {error}"))
}

/// One shell invocation's owned policy and immutable mechanism resources.
/// No repository is opened, and no session authority changes, on construction.
pub(crate) struct NativeGitBroker {
    policy: Arc<dyn CommitPolicy>,
    helper_dir: tempfile::TempDir,
    executable: PathBuf,
    git: PathBuf,
    native_image: PathBuf,
}

impl NativeGitBroker {
    /// #2720: ambient session authority is not the commit child's write grant.
    /// Keep helpers and the worker image immutable during the hook handshake.
    /// Scoped sessions retain their already-bound repository identity; only an
    /// ambient caller may authorize resolving a repository at dispatch time.
    pub(crate) fn invocation_caveats(
        caveats: &crate::Caveats,
        workspace: &Path,
    ) -> Result<crate::Caveats, String> {
        if !matches!(caveats.fs_write, crate::Scope::All) {
            return Ok(caveats.clone());
        }
        let canonical_workspace = workspace
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let write = crate::git_hardening::ambient_gitdir_write_grant(workspace);
        let mut invocation = caveats.clone();
        invocation.fs_write = crate::Scope::only(
            std::iter::once(canonical_workspace.to_string_lossy().into_owned()).chain(write),
        );
        Ok(invocation)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn new(policy: Arc<dyn CommitPolicy>) -> Result<Arc<Self>, String> {
        let executable = std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .map_err(|error| error.to_string())?;
        let git = crate::git_hardening::hardened_git(Path::new("."), &["--version"])
            .map_err(|error| error.to_string())?
            .get_program()
            .to_owned();
        let git = PathBuf::from(git);
        let git = if git.is_absolute() {
            git
        } else {
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .map(|path| path.join(&git))
                .find(|path| path.is_file())
                .ok_or("native Git executable is unavailable")?
        }
        .canonicalize()
        .map_err(|error| error.to_string())?;
        let native_image = native_git_image(&git)?;
        let helper_dir = tempfile::Builder::new()
            .prefix("newt-native-git-")
            .tempdir()
            .map_err(|error| error.to_string())?;
        for name in HOOK_NAMES {
            std::os::unix::fs::symlink(&executable, helper_dir.path().join(name))
                .map_err(|error| error.to_string())?;
        }
        Ok(Arc::new(Self {
            policy,
            helper_dir,
            executable,
            git,
            native_image,
        }))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn new(_policy: Arc<dyn CommitPolicy>) -> Result<Arc<Self>, String> {
        Err("authenticated native Git helper transport is unavailable on this platform".into())
    }
}

/// Apple's system Git is an exec shim. Resolve only that fixed system shim
/// using the existing host-selected developer toolchain, with no model env.
/// The native command still executes its original /usr/bin/git exactly once.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn native_git_image(program: &Path) -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    if program == Path::new("/usr/bin/git") {
        let developer = crate::confined_exec::selected_developer_directory()
            .ok_or("cannot resolve the host's native Git image")?;
        return Path::new(developer)
            .join("usr/bin/git")
            .canonicalize()
            .map_err(|error| error.to_string());
    }
    Ok(program.to_owned())
}

/// Inspect only Git's expanded global-option boundary. Git itself still parses
/// every commit option and pathspec; no shell source is rewritten or replayed.
fn commit_position(args: &[OsString]) -> Option<usize> {
    let mut position = 0;
    while let Some(arg) = args.get(position) {
        let arg = arg.to_str()?;
        match arg {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env" => {
                position += 2;
            }
            arg if arg.starts_with('-') => position += 1,
            "commit" => return Some(position),
            _ => return None,
        }
    }
    None
}

/// Fixed-argv reads run under the SAME authority as the native command, with
/// hooks/filter programs disabled. No ambient host repository traversal.
#[derive(Clone)]
struct RepositoryProbe {
    program: String,
    prefix: Vec<String>,
    cwd: PathBuf,
    caveats: crate::Caveats,
    env: Vec<(String, String)>,
}

impl RepositoryProbe {
    fn check_commit_authority(&self, reference: &str, control: &BrokerControl) -> ToolResult<()> {
        use agent_toolchain::git_caveats::{check_default_branch_move, GitCaveats};
        let caps = GitCaveats::from_session(&self.caveats);
        if !caps.permits_commit() || !caps.permits_ref(reference) {
            return Err(denied(
                "commit or destination ref is outside the effective Git grant",
            ));
        }
        let refs_exist = !self
            .text(
                &[
                    "for-each-ref",
                    "--count=1",
                    "--format=%(refname)",
                    "refs/heads/",
                ],
                control,
            )?
            .is_empty();
        check_default_branch_move(reference, refs_exist, None).map_err(denied)?;
        if refs_exist {
            let origin =
                self.optional_text(&["symbolic-ref", "-q", "refs/remotes/origin/HEAD"], control)?;
            let default = origin
                .as_deref()
                .and_then(|name| name.strip_prefix("refs/remotes/origin/"));
            check_default_branch_move(reference, true, default).map_err(denied)?;
        }
        Ok(())
    }

    fn output(
        &self,
        args: &[&str],
        control: &BrokerControl,
    ) -> ToolResult<crate::confined_exec::ConfinedOutput> {
        control.check()?;
        let mut argv = self.prefix.clone();
        for setting in [
            "core.hooksPath=/dev/null",
            "core.fsmonitor=false",
            "diff.external=",
            "core.untrackedCache=false",
            "core.pager=cat",
            "pager.branch=false",
        ] {
            argv.extend(["-c".into(), setting.into()]);
        }
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        let remaining = control
            .deadline()
            .saturating_duration_since(std::time::Instant::now());
        let mut request = crate::confined_exec::ExecRequest::new(
            // #2693: this broker's own fixed-program, policy-mediated `git`
            // re-dispatch — not a model-chosen command — accepts the
            // per-axis `BrokerMediated` floor (exec: Interceptor; fs/net
            // stay Kernel). See `confined_exec`'s module doc.
            crate::confined_exec::ExecOrigin::BrokerMediated,
            &self.program,
            argv,
            &self.cwd,
            self.caveats.clone(),
        )
        .timeout(remaining.min(std::time::Duration::from_secs(15)));
        for (key, value) in &self.env {
            request = request.env(key, value);
        }
        let output = crate::confined_exec::ConstrainedExecutor::run(&request).map_err(denied)?;
        control.check()?;
        if output.stdout.len() > MAX_COMMIT_BYTES {
            return Err(denied("repository probe exceeded its bounded result"));
        }
        Ok(output)
    }

    fn run(&self, args: &[&str], control: &BrokerControl) -> ToolResult<Vec<u8>> {
        let output = self.output(args, control)?;
        if !output.success {
            return Err(denied(String::from_utf8_lossy(&output.stderr)));
        }
        Ok(output.stdout)
    }

    fn optional_text(&self, args: &[&str], control: &BrokerControl) -> ToolResult<Option<String>> {
        let output = self.output(args, control)?;
        if output.success {
            String::from_utf8(output.stdout)
                .map(|value| Some(value.trim_end().to_owned()))
                .map_err(denied)
        } else if output.code == Some(1) {
            Ok(None)
        } else {
            Err(denied(String::from_utf8_lossy(&output.stderr)))
        }
    }

    fn text(&self, args: &[&str], control: &BrokerControl) -> ToolResult<String> {
        String::from_utf8(self.run(args, control)?)
            .map(|value| value.trim_end().to_owned())
            .map_err(denied)
    }

    fn candidate(&self, oid: &str, control: &BrokerControl) -> ToolResult<Vec<u8>> {
        if !valid_oid(oid) {
            return Err(denied("invalid Git object identifier"));
        }
        let size: usize = self
            .text(&["cat-file", "-s", oid], control)?
            .parse()
            .map_err(denied)?;
        if size > MAX_COMMIT_BYTES {
            return Err(denied("candidate commit exceeds the broker bound"));
        }
        self.run(&["cat-file", "commit", oid], control)
    }
}

impl CommandBroker for NativeGitBroker {
    fn read_only_resources(&self, context: &ToolContext) -> ToolResult<Vec<PathBuf>> {
        let protected = [
            self.helper_dir.path().canonicalize().map_err(denied)?,
            self.executable.clone(),
        ];
        match &context.caveats().fs_write {
            crate::Scope::All => return Err(denied("native commit helpers require directory-scoped filesystem authority; unrestricted writes cannot protect the signing boundary")),
            crate::Scope::Only(paths) => {
                for path in paths {
                    let path = Path::new(path).canonicalize().map_err(denied)?;
                    if protected.iter().any(|protected| protected.starts_with(&path) || path.starts_with(protected)) {
                        return Err(denied("native commit helper mechanism overlaps admitted write authority"));
                    }
                }
            }
        }
        Ok(vec![protected[0].clone()])
    }

    fn prepare(
        &self,
        command: &BrokerCommand,
        context: &ToolContext,
        control: &BrokerControl,
    ) -> ToolResult<Option<PreparedBrokerCommand>> {
        let Some(position) = commit_position(&command.args) else {
            return Ok(None);
        };
        let program = command.program.canonicalize().map_err(denied)?;
        if program != self.git && program != self.native_image {
            if command
                .original_command
                .to_string_lossy()
                .rsplit(['/', '\\'])
                .next()
                .is_some_and(|name| {
                    name.eq_ignore_ascii_case("git") || name.eq_ignore_ascii_case("git.exe")
                })
            {
                return Err(denied(
                    "selected Git executable differs from the admitted host Git image",
                ));
            }
            return Ok(None);
        }
        control.check()?;
        let policy = self
            .policy
            .snapshot_for_commit()
            .unwrap_or_else(|| self.policy.clone());
        if context
            .check_exec(&command.program.to_string_lossy())
            .is_err()
        {
            // No broker preparation or probe may run without exec authority.
            // Leave the command unchanged so Brush's mandatory final exec
            // check refuses it into the structured denial sink. Returning an
            // error here crosses the broker RPC as an opaque terminating error,
            // which cannot reach the operator's permission gate (#2689).
            return Ok(None);
        }
        context.check_path_read(&command.cwd)?;
        let prefix = command.args[..position]
            .iter()
            .map(|arg| {
                arg.to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| denied("non-UTF8 Git repository selector"))
            })
            .collect::<ToolResult<Vec<_>>>()?;
        let env = command
            .env
            .iter()
            .map(|(key, value)| {
                Ok((
                    key.to_str()
                        .ok_or_else(|| denied("non-UTF8 environment name"))?
                        .to_owned(),
                    value
                        .to_str()
                        .ok_or_else(|| denied("non-UTF8 Git environment value"))?
                        .to_owned(),
                ))
            })
            .collect::<ToolResult<Vec<_>>>()?;
        let mut probe = RepositoryProbe {
            program: self.native_image.to_string_lossy().into_owned(),
            prefix,
            cwd: command.cwd.clone(),
            // BrokerMediated supplies the reviewed per-axis enforcement floor;
            // it does not grant any authority beyond this invocation's caveats.
            caveats: context.caveats().clone(),
            env,
        };
        let git_dir = PathBuf::from(probe.text(&["rev-parse", "--absolute-git-dir"], control)?);
        let common = PathBuf::from(probe.text(
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            control,
        )?);
        context.check_path_write(&git_dir)?;
        context.check_path_write(&common.join("objects"))?;
        let reference = probe
            .optional_text(&["symbolic-ref", "-q", "HEAD"], control)?
            .unwrap_or_else(|| "HEAD".into());
        probe.check_commit_authority(&reference, control)?;
        let ref_path = if reference == "HEAD" {
            git_dir.join("HEAD")
        } else {
            common.join(&reference)
        };
        context.check_path_write(&ref_path)?;
        let head = probe.optional_text(&["rev-parse", "--verify", "--quiet", "HEAD"], control)?;
        let previous_parents = match &head {
            Some(head) => commit_parents(&probe.candidate(head, control)?).map_err(denied)?,
            None => Vec::new(),
        };
        let auto_merge =
            probe.optional_text(&["rev-parse", "--verify", "--quiet", "AUTO_MERGE"], control)?;

        // Resolve existing hook behavior before injecting our own final config.
        // The ordinary shell hardening may deliberately disable hooks; preserve
        // that choice, as well as an explicit caller-provided hooksPath.
        let original_hooks = original_hooks(command, position, &git_dir, &probe, control)?;
        let common_view = if reference == "HEAD" && git_dir != common {
            Some(common_view::create(
                &git_dir,
                &common,
                head.as_deref()
                    .ok_or_else(|| denied("detached commit has no HEAD"))?,
                context,
                &probe,
                control,
            )?)
        } else {
            None
        };
        let mut env = vec![
            (FD_ENV.into(), BROKER_FD.to_string().into()),
            (HOOKS_ENV.into(), original_hooks.into_os_string()),
        ];
        if let Some(view) = &common_view {
            env.push((
                "GIT_COMMON_DIR".into(),
                view.directory.path().as_os_str().to_owned(),
            ));
        }
        let mut args = command.args[..position].to_vec();
        if let Some(view) = &common_view {
            let worktree = probe.text(&["rev-parse", "--show-toplevel"], control)?;
            let selectors = [
                format!("--git-dir={}", view.directory.path().display()),
                format!("--work-tree={worktree}"),
            ];
            args.extend(selectors.iter().map(OsString::from));
            probe.prefix.extend(selectors);
            probe.env.retain(|(key, _)| key != "GIT_COMMON_DIR");
            probe.env.push((
                "GIT_COMMON_DIR".into(),
                view.directory.path().to_string_lossy().into_owned(),
            ));
            if !command.env.iter().any(|(key, _)| key == "GIT_INDEX_FILE") {
                env.push((
                    "GIT_INDEX_FILE".into(),
                    git_dir.join("index").into_os_string(),
                ));
            }
        }
        for (key, value) in [
            // Commit-scoped views must not initiate repository-wide maintenance.
            ("gc.auto", "0".into()),
            ("maintenance.auto", "false".into()),
            (
                "core.hooksPath",
                self.helper_dir.path().as_os_str().to_owned(),
            ),
            ("gpg.format", "openpgp".into()),
            ("gpg.program", self.executable.as_os_str().to_owned()),
            (
                "gpg.openpgp.program",
                self.executable.as_os_str().to_owned(),
            ),
            ("user.signingkey", "newt-native-git".into()),
            (
                "commit.gpgsign",
                if policy.signing_required() {
                    "true"
                } else {
                    "false"
                }
                .into(),
            ),
        ] {
            args.push("-c".into());
            let mut assignment = OsString::from(key);
            assignment.push("=");
            assignment.push(value);
            args.push(assignment);
        }
        args.extend(command.args[position..].iter().cloned());
        // Do not scan commit arguments for a pathspec delimiter: `-m --` is
        // a valid message. Config requests signing; an explicit no-sign option
        // retains native parsing and is refused at the protected ref boundary.
        Ok(Some(PreparedBrokerCommand {
            args: Some(args),
            env,
            target_fd: BROKER_FD,
            expected_process_image: (program != self.native_image)
                .then(|| self.native_image.clone()),
            session: Arc::new(NativeCommitSession {
                common_view,
                state: Mutex::new(SessionState {
                    process: None,
                    broker: CommitBroker::new(policy, reference.clone()),
                    candidate: None,
                    published: false,
                }),
                probe,
                reference,
                head,
                previous_parents,
                auto_merge,
                git: program,
                native_image: self.native_image.clone(),
                executable: self.executable.clone(),
            }),
        }))
    }
}

fn original_hooks(
    command: &BrokerCommand,
    position: usize,
    git_dir: &Path,
    probe: &RepositoryProbe,
    control: &BrokerControl,
) -> ToolResult<PathBuf> {
    // `git config --get` is a native read with the original selectors and
    // environment. Unlike other probes it must observe the original hooksPath.
    let mut plain = probe.clone();
    let mut args = command.args[..position]
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    args.extend(["config".into(), "--get".into(), "core.hooksPath".into()]);
    plain.prefix.clear();
    // A config query does not execute hooks. It still runs inside the fence.
    let mut request = crate::confined_exec::ExecRequest::new(
        // #2693: same fixed-program broker re-dispatch as `RepositoryProbe::output`.
        crate::confined_exec::ExecOrigin::BrokerMediated,
        &plain.program,
        args,
        &plain.cwd,
        plain.caveats.clone(),
    )
    .timeout(
        control
            .deadline()
            .saturating_duration_since(std::time::Instant::now())
            .min(std::time::Duration::from_secs(15)),
    );
    for (key, value) in &plain.env {
        request = request.env(key, value);
    }
    let result = crate::confined_exec::ConstrainedExecutor::run(&request).map_err(denied)?;
    control.check()?;
    if result.success {
        // Git resolves relative hooksPath in its hook working directory, not
        // the caller's cwd. The helper inherits that exact native directory.
        Ok(PathBuf::from(
            String::from_utf8(result.stdout).map_err(denied)?.trim_end(),
        ))
    } else if result.code == Some(1) {
        Ok(git_dir.join("hooks"))
    } else {
        Err(denied(String::from_utf8_lossy(&result.stderr)))
    }
}

struct SessionState {
    process: Option<BrokerProcess>,
    broker: CommitBroker,
    candidate: Option<(String, Vec<u8>)>,
    published: bool,
}

struct NativeCommitSession {
    common_view: Option<common_view::CommitView>,
    state: Mutex<SessionState>,
    probe: RepositoryProbe,
    reference: String,
    head: Option<String>,
    previous_parents: Vec<String>,
    auto_merge: Option<String>,
    git: PathBuf,
    native_image: PathBuf,
    executable: PathBuf,
}

impl BrokerSession for NativeCommitSession {
    fn on_spawn(&self, process: &BrokerProcess, control: &BrokerControl) -> ToolResult<()> {
        control.check()?;
        if process.executable != self.git && process.executable != self.native_image {
            return Err(denied("native Git spawn image changed"));
        }
        let mut state = self.state.lock().map_err(denied)?;
        if state.process.replace(process.clone()).is_some() {
            return Err(denied("native Git process already bound"));
        }
        Ok(())
    }

    fn request(
        &self,
        peer: &BrokerPeer,
        payload: &[u8],
        control: &BrokerControl,
    ) -> ToolResult<Vec<u8>> {
        control.check()?;
        let mut state = self.state.lock().map_err(denied)?;
        let process = state
            .process
            .as_ref()
            .ok_or_else(|| denied("native Git process not bound"))?;
        if peer.parent_process_id != process.process_id
            || peer.parent_birth != process.birth
            || peer.parent_executable != self.native_image
            || peer.executable != self.executable
        {
            return Err(denied(
                "helper identity does not belong to the admitted native Git process",
            ));
        }
        let result = match decode_request(payload).map_err(denied)? {
            HookRequest::Message { message } => state
                .broker
                .prepare_message(&message)
                .map(String::into_bytes),
            HookRequest::Sign { commit } => {
                self.check_commit_context(&commit, control)?;
                state.broker.sign(&commit).map(String::into_bytes)
            }
            HookRequest::References { phase, updates } => self
                .reference_event(&mut state, &phase, &updates, control)
                .map(|()| Vec::new()),
        };
        control.check()?;
        match result {
            Ok(bytes) => HookResponse::Success(bytes),
            Err(reason) => HookResponse::Refused(reason),
        }
        .canonical_form()
        .map_err(denied)
    }
}

impl NativeCommitSession {
    fn check_commit_context(&self, commit: &[u8], control: &BrokerControl) -> ToolResult<()> {
        self.probe
            .check_commit_authority(&self.reference, control)?;
        let boundary = commit
            .windows(2)
            .position(|window| window == b"\n\n")
            .ok_or_else(|| denied("commit has no message"))?;
        let headers = std::str::from_utf8(&commit[..boundary]).map_err(denied)?;
        let tree = headers
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("tree "))
            .ok_or_else(|| denied("commit lacks tree"))?;
        if !valid_oid(tree) || self.probe.text(&["cat-file", "-t", tree], control)? != "tree" {
            return Err(denied(
                "commit tree is not present in the admitted repository",
            ));
        }
        // Ordinary commit and --amend both stay rooted in the repository's
        // starting history. Native Git owns author, timestamp and message flags.
        let parents = commit_parents(commit).map_err(denied)?;
        let append_parents: Vec<_> = self.head.iter().cloned().collect();
        if parents != append_parents && parents != self.previous_parents {
            return Err(denied(
                "commit is neither an append nor an amendment of the admitted HEAD",
            ));
        }
        Ok(())
    }

    fn reference_event(
        &self,
        state: &mut SessionState,
        phase: &str,
        updates: &str,
        control: &BrokerControl,
    ) -> Result<(), String> {
        // Git 2.54+ reports preparing before locking or resolving symbolic refs.
        // It is only a notification: do not retain a candidate, approve bytes,
        // or consume attribution here. The locked prepared event still performs
        // every destination, ancestry, message and signing check below; committed
        // cannot succeed without that verified candidate. This also covers the
        // pre-lock AUTO_MERGE cleanup notification after publication (#2813).
        if phase == "preparing" {
            return Ok(());
        }
        // Git removes its temporary AUTO_MERGE tree reference after publishing
        // a commit, even when it is absent (zero -> zero). This is native
        // cleanup under the admitted gitdir write grant, never another commit.
        let auxiliary: Vec<_> = updates.split_whitespace().collect();
        if state.published
            && auxiliary.len() == 3
            && auxiliary[2] == "AUTO_MERGE"
            && valid_oid(auxiliary[0])
            && valid_oid(auxiliary[1])
            && auxiliary[0].len() == auxiliary[1].len()
            && old_ref_matches(self.auto_merge.as_deref(), auxiliary[0])
            && auxiliary[1].bytes().all(|byte| byte == b'0')
            && matches!(phase, "prepared" | "committed" | "aborted")
        {
            return Ok(());
        }
        let mut candidate = None;
        for line in updates.lines() {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() != 3 || !valid_oid(fields[0]) || !valid_oid(fields[1]) {
                return Err("malformed native Git reference event".into());
            }
            if fields[2] != self.reference && fields[2] != "HEAD" {
                return Err("native commit attempted an unrelated reference update".into());
            }
            if fields[0].len() != fields[1].len()
                || !old_ref_matches(self.head.as_deref(), fields[0])
            {
                return Err("native Git reference changed after invocation admission".into());
            }
            if let Some(existing) = &candidate {
                if existing != fields[1] {
                    return Err("native commit reference event has conflicting objects".into());
                }
            }
            candidate = Some(fields[1].to_owned());
        }
        let oid = candidate.ok_or("native commit reference event is empty")?;
        match phase {
            "prepared" => {
                let bytes = self
                    .probe
                    .candidate(&oid, control)
                    .map_err(|error| error.to_string())?;
                self.check_commit_context(&bytes, control)
                    .map_err(|error| error.to_string())?;
                state.broker.prepare_ref_update(&self.reference, &bytes)?;
                state.candidate = Some((oid, bytes));
            }
            "aborted" => {
                state.broker.aborted();
                state.candidate = None;
            }
            "committed" => {
                let (expected, bytes) = state
                    .candidate
                    .as_ref()
                    .ok_or("native commit has no prepared candidate")?;
                if expected != &oid {
                    return Err("committed Git object differs from prepared candidate".into());
                }
                let actual = self
                    .probe
                    .text(&["rev-parse", "--verify", &self.reference], control)
                    .map_err(|error| error.to_string())?;
                if actual != oid {
                    return Err(
                        "native Git success does not match the actual repository ref".into(),
                    );
                }
                let actual = self
                    .probe
                    .candidate(&oid, control)
                    .map_err(|error| error.to_string())?;
                if actual != *bytes {
                    return Err("published Git object differs from the prepared bytes".into());
                }
                state.broker.committed(&self.reference, &actual)?;
                if let Some(view) = &self.common_view {
                    view.publish(
                        self.head
                            .as_deref()
                            .ok_or("private commit lacks original HEAD")?,
                        &oid,
                        &actual,
                    )?;
                }
                state.published = true;
                state.candidate = None;
            }
            _ => return Err("unknown native Git reference phase".into()),
        }
        Ok(())
    }
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn old_ref_matches(head: Option<&str>, old: &str) -> bool {
    match head {
        Some(head) => head == old,
        None => old.bytes().all(|byte| byte == b'0'),
    }
}

/// #2682 round 2 (`git_hardening::advance_own_branch_ref`): also reused
/// host-side to verify the fast-forward/amend shape of a commit landed with
/// HEAD detached, before the bounded `update-ref` that publishes it.
pub(crate) fn commit_parents(commit: &[u8]) -> Result<Vec<String>, String> {
    let boundary = commit
        .windows(2)
        .position(|window| window == b"\n\n")
        .ok_or("commit has no message boundary")?;
    std::str::from_utf8(&commit[..boundary])
        .map_err(|error| error.to_string())?
        .lines()
        .filter_map(|line| line.strip_prefix("parent "))
        .map(|parent| {
            if valid_oid(parent) {
                Ok(parent.to_owned())
            } else {
                Err("invalid Git parent identifier".into())
            }
        })
        .collect()
}

/// Dispatch before CLI parsing, only for the fixed Git hook/signer entrypoints.
/// Environment and argv merely select a role; the private client authenticates
/// each message, and the host independently binds the actual parent Git image.
pub(crate) fn maybe_dispatch() -> Option<i32> {
    use agent_toolchain::native_git::helper::HelperRole;
    let mut args = std::env::args_os();
    let executable = args.next()?;
    let name = Path::new(&executable).file_name()?.to_str()?;
    let args: Vec<_> = args.collect();
    let role = HelperRole::from_name(name);
    let signing = args.first().is_some_and(|arg| arg == "--status-fd=2")
        && std::env::var_os(FD_ENV).is_some();
    if !signing && !HOOK_NAMES.contains(&name) {
        return None;
    }
    let result = run_helper(
        if signing {
            Some(HelperRole::Sign)
        } else {
            role
        },
        name,
        &args,
    );
    if let Err(error) = &result {
        eprintln!("native Git helper refused: {error}");
    }
    Some(if result.is_ok() { 0 } else { 1 })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn forward_original_hook_input(mut stdin: impl std::io::Write, input: &[u8]) -> Result<(), String> {
    // Git treats EPIPE as an early-successful hook exit and uses the hook's
    // eventual status to decide whether the operation is refused.
    if let Err(error) = stdin.write_all(input) {
        if error.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(error.to_string());
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_helper(
    role: Option<agent_toolchain::native_git::helper::HelperRole>,
    name: &str,
    args: &[OsString],
) -> Result<(), String> {
    use agent_toolchain::native_git::helper::{self, HelperRole};
    use std::io::Read as _;
    if role == Some(HelperRole::Sign) && args.get(2).is_none_or(|key| key != "newt-native-git") {
        return Err(
            "native Git signing-key override differs from the operator-configured signer".into(),
        );
    }
    let fd: i32 = std::env::var(FD_ENV)
        .map_err(|_| "native Git helper has no delegated endpoint")?
        .parse()
        .map_err(|_| "invalid delegated endpoint")?;
    if fd != BROKER_FD {
        return Err("unexpected native Git delegated descriptor".into());
    }
    let mut client = agent_bridle_tool_shell::BrokerClient::from_delegated_fd(fd)
        .map_err(|error| error.to_string())?;
    let mut input = Vec::new();
    // Message hooks receive no stdin contract. Do not drain a pipeline's input
    // merely to finalize the on-disk message file passed by native Git.
    let reads_stdin = matches!(
        role,
        Some(HelperRole::ReferenceTransaction | HelperRole::Sign)
    ) || name == "post-rewrite";
    if reads_stdin {
        std::io::stdin()
            .take(MAX_COMMIT_BYTES as u64 + 1)
            .read_to_end(&mut input)
            .map_err(|error| error.to_string())?;
        if input.len() > MAX_COMMIT_BYTES {
            return Err("native Git helper input exceeds its bound".into());
        }
    }
    if !matches!(role, Some(HelperRole::Sign)) {
        if let Some(original) = std::env::var_os(HOOKS_ENV) {
            let hook = PathBuf::from(original).join(name);
            if hook.is_file() {
                let mut command = std::process::Command::new(&hook);
                command.args(args).env_remove(FD_ENV).env_remove(HOOKS_ENV);
                if reads_stdin {
                    command.stdin(std::process::Stdio::piped());
                }
                // This helper already inherits the kernel fence. Preserve the
                // hook's stdio while withholding all private authority FDs.
                agent_bridle_fdguard::deny_inherited_fds(&mut command);
                match command.spawn() {
                    Ok(mut child) => {
                        if reads_stdin {
                            let stdin = child.stdin.take().ok_or("original hook stdin missing")?;
                            forward_original_hook_input(stdin, &input)?;
                        }
                        if !child.wait().map_err(|error| error.to_string())?.success() {
                            return Err(format!("original {name} hook refused"));
                        }
                    }
                    // Git ignores non-executable hook files rather than
                    // changing a previously successful native commit to fail.
                    Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
    }
    if let Some(role) = role {
        helper::run(
            role,
            args,
            &mut std::io::Cursor::new(input),
            &mut std::io::stdout(),
            &mut std::io::stderr(),
            &mut |request| client.request(request).map_err(|error| error.to_string()),
        )?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn run_helper(
    _role: Option<agent_toolchain::native_git::helper::HelperRole>,
    _name: &str,
    _args: &[OsString],
) -> Result<(), String> {
    Err("authenticated native Git helper transport is unavailable on this platform".into())
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::forward_original_hook_input;
    use std::io::Read;
    use std::process::{Command, Stdio};

    /// #2720: attenuation is per invocation; it must not mutate the ambient
    /// session or re-resolve a scoped caller's repository grant.
    #[test]
    fn full_access_commit_writes_are_bounded_without_changing_session() {
        use crate::caveats::permits_path;
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().canonicalize().unwrap();
        let session = crate::Caveats::top();
        let invocation = super::NativeGitBroker::invocation_caveats(&session, &workspace).unwrap();
        assert!(matches!(session.fs_write, crate::Scope::All));
        assert!(permits_path(
            &invocation.fs_write,
            workspace.to_str().unwrap()
        ));
        assert!(!permits_path(
            &invocation.fs_write,
            workspace.parent().unwrap().to_str().unwrap()
        ));
        let repeated = super::NativeGitBroker::invocation_caveats(&invocation, &workspace).unwrap();
        assert_eq!(invocation, repeated);
    }

    #[test]
    fn original_hook_closing_stdin_does_not_refuse_successful_hook() {
        // The hook says "ready" on its stdout once its stdin is closed, and the
        // blocking read of that byte is the event; the former 1 s deadline poll
        // on a marker file measured fork+exec latency on a loaded box instead.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec 0<&-\nprintf ready"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = [0_u8; 5];
        child
            .stdout
            .take()
            .expect("test hook stdout missing")
            .read_exact(&mut ready)
            .expect("the hook reports that its stdin is closed");
        assert_eq!(&ready, b"ready");
        let stdin = child.stdin.take().expect("test hook stdin missing");
        forward_original_hook_input(
            stdin,
            b"0000000000000000000000000000000000000000 \
1111111111111111111111111111111111111111 refs/heads/task\n",
        )
        .unwrap();
        assert!(child.wait().unwrap().success());
    }
}
