//! Private Brush-worker and carried-coreutils dispatch **as a library**.
//!
//! [`maybe_dispatch`] recognizes the sandboxed Brush worker entrypoint whenever
//! the `brush` feature is enabled. With `carried-coreutils`, it also recognizes
//! the busybox-style dispatch protocol: a shim builtin re-executes the current
//! process as `current_exe() --invoke-bundled <name> <args…>`, and the dispatcher
//! turns that back into a call of the `uu_*::uumain`.
//!
//! Both private entrypoints must be handled by the embedding binary rather than
//! a standalone `brush-shell` binary because `current_exe()` is the host (for
//! example, `newt`). An embedder becomes dispatch-capable with one line in
//! `main`:
//!
//! ```no_run
//! if let Some(code) = agent_bridle_tool_shell::maybe_dispatch() {
//!     std::process::exit(code);
//! }
//! ```
//!
//! We keep a **fresh-process self re-exec** model on purpose. On Unix, "fork and
//! call `uumain` in-process" is unsafe under a multithreaded runtime because
//! other threads' locks remain held in the child; the following `execve` resets
//! the address space. Linux and macOS additionally authenticate the re-exec over
//! a private kernel socket, binding the spawned PID, same executable image, and
//! exact raw argv. Other targets refuse this private dispatch rather than
//! treating the argv marker as authority.
//!
//! Cribbed from our brush fork's `brush-shell/src/bundled.rs` (same license); to
//! be upstreamed to reubeno/brush as the dispatch-as-a-library follow-up to the
//! static command filters (reubeno/brush#1314).

#[cfg(feature = "carried-coreutils")]
use std::collections::HashMap;
#[cfg(feature = "carried-coreutils")]
use std::ffi::OsString;
#[cfg(all(
    feature = "carried-coreutils",
    any(target_os = "linux", target_os = "macos")
))]
use std::io::Write;
#[cfg(all(
    feature = "carried-coreutils",
    any(target_os = "linux", target_os = "macos")
))]
use std::os::fd::OwnedFd;
#[cfg(all(
    feature = "carried-coreutils",
    any(target_os = "linux", target_os = "macos")
))]
use std::os::unix::net::UnixStream;
#[cfg(all(
    feature = "carried-coreutils",
    any(target_os = "linux", target_os = "macos")
))]
use std::path::PathBuf;
#[cfg(feature = "carried-coreutils")]
use std::sync::OnceLock;

#[cfg(feature = "carried-coreutils")]
use brush_core::builtins::{BoxFuture, ContentOptions, ContentType, Registration};
#[cfg(all(
    feature = "carried-coreutils",
    any(target_os = "linux", target_os = "macos")
))]
use brush_core::commands;
#[cfg(feature = "carried-coreutils")]
use brush_core::commands::{CommandArg, ExecutionContext};
#[cfg(feature = "carried-coreutils")]
use brush_core::extensions::ShellExtensions;
#[cfg(all(
    feature = "carried-coreutils",
    any(target_os = "linux", target_os = "macos")
))]
use brush_core::filter::{
    CmdExecFilter, ExternalCmdOutput, ExternalCmdParams, FileOpenAccess, FileOpenFilter,
    FileOpenParams, PostFilterResult, PreFilterResult,
};
use brush_core::ExecutionExitCode;

/// The leading flag that signals a bundled-command dispatch. Deliberately
/// obscure so it does not collide with real flags or script tokens.
#[cfg(feature = "carried-coreutils")]
pub const DISPATCH_FLAG: &str = "--invoke-bundled";

/// Internal marker selecting the kernel-authenticated control protocol.
///
/// The marker is not authority by itself. The child accepts the dispatch only
/// over a fresh private socket whose actual peer is the same executable and
/// whose challenge response binds the exact utility argv.
#[cfg(feature = "carried-coreutils")]
const LIVE_PARENT_FLAG: &str = "--agent-bridle-live-parent-v1";

/// Signature of a bundled command's entry point — matches `uu_*::uumain`.
#[cfg(feature = "carried-coreutils")]
pub type BundledFn = fn(args: Vec<OsString>) -> i32;

/// Process-wide registry. Set once, read on each shim invocation + dispatch.
#[cfg(feature = "carried-coreutils")]
static REGISTRY: OnceLock<HashMap<String, BundledFn>> = OnceLock::new();

/// Cached path to the running executable.
#[cfg(all(
    feature = "carried-coreutils",
    any(target_os = "linux", target_os = "macos")
))]
static SELF_EXE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Install the bundled-command registry. Idempotent (first call wins).
#[cfg(feature = "carried-coreutils")]
#[allow(
    clippy::implicit_hasher,
    reason = "registry uses the default hasher; callers build with HashMap::new()"
)]
pub fn install(commands: HashMap<String, BundledFn>) {
    let _ = REGISTRY.set(commands);
}

/// Install the registry from all compiled-in providers. Call once, before
/// [`maybe_dispatch`] / [`register_shims`], so both the dispatch fast-path and
/// the shell's shim builtins see a populated registry.
#[cfg(feature = "carried-coreutils")]
pub fn install_default_providers() {
    let mut commands: HashMap<String, BundledFn> = HashMap::new();
    commands.extend(brush_coreutils_builtins::bundled_commands());
    install(commands);
}

/// Run the bundled-command fast path if the process was invoked for it.
///
/// If invoked as the private Brush worker, serves its single request and
/// returns `Some(code)`. With `carried-coreutils`, an invocation shaped as
/// `<self> --invoke-bundled <NAME> [ARGS…]` runs the registered function and
/// likewise returns its code. Returns `None` for a normal invocation, so the
/// embedder's usual startup proceeds.
#[must_use]
pub fn maybe_dispatch() -> Option<i32> {
    let mut raw = std::env::args_os();
    let _argv0 = raw.next();
    let first = raw.next()?;
    if first == crate::brush_worker::WORKER_FLAG {
        return match raw.next().and_then(|value| value.into_string().ok()) {
            Some(kind) if kind == crate::brush_worker::WORKER_KIND && raw.next().is_none() => {
                Some(crate::brush_worker::main())
            }
            _ => {
                eprintln!("agent-bridle: invalid private worker entrypoint");
                Some(u8::from(ExecutionExitCode::InvalidUsage).into())
            }
        };
    }
    #[cfg(feature = "carried-coreutils")]
    {
        if first != DISPATCH_FLAG {
            return None;
        }

        if REGISTRY.get().is_none() {
            install_default_providers();
        }

        let rest: Vec<OsString> = raw.collect();
        let Some((name, args)) = rest.split_first() else {
            eprintln!("carried-coreutils: {DISPATCH_FLAG} requires a command name");
            return Some(u8::from(ExecutionExitCode::InvalidUsage).into());
        };
        let Some((auth_flag, args)) = args.split_first() else {
            eprintln!(
                "carried-coreutils: refusing direct dispatch; private parent control is required"
            );
            return Some(u8::from(ExecutionExitCode::CannotExecute).into());
        };
        if auth_flag != LIVE_PARENT_FLAG {
            eprintln!(
                "carried-coreutils: refusing direct dispatch; private parent control is required"
            );
            return Some(u8::from(ExecutionExitCode::CannotExecute).into());
        }
        if let Err(error) = crate::private_control::authenticate_carried_dispatch(name, args) {
            eprintln!("carried-coreutils: private parent authentication failed: {error}");
            return Some(u8::from(ExecutionExitCode::CannotExecute).into());
        }
        let Some(name_str) = name.to_str() else {
            eprintln!(
                "carried-coreutils: unknown bundled command: {}",
                name.to_string_lossy()
            );
            return Some(u8::from(ExecutionExitCode::NotFound).into());
        };
        let Some(func) = REGISTRY.get().and_then(|r| r.get(name_str)) else {
            eprintln!("carried-coreutils: unknown bundled command: {name_str}");
            return Some(u8::from(ExecutionExitCode::NotFound).into());
        };

        let mut argv: Vec<OsString> = Vec::with_capacity(1 + args.len());
        argv.push(name.clone());
        argv.extend(args.iter().cloned());
        Some(func(argv))
    }
    #[cfg(not(feature = "carried-coreutils"))]
    {
        let _ = first;
        None
    }
}

/// Recover the model-visible identity of a carried command from the private
/// re-exec argv. The executable must be this process and the command must be in
/// the installed, closed registry; otherwise this is an ordinary external exec.
#[cfg(feature = "carried-coreutils")]
pub(crate) fn logical_carried_command(program: &str, args: &[String]) -> Option<String> {
    if REGISTRY.get().is_none() {
        install_default_providers();
    }
    let program = std::path::Path::new(program).canonicalize().ok()?;
    let current = std::env::current_exe().ok()?.canonicalize().ok()?;
    if program != current
        || args.first().map(String::as_str) != Some(DISPATCH_FLAG)
        || args.get(2).map(String::as_str) != Some(LIVE_PARENT_FLAG)
    {
        return None;
    }
    let name = args.get(1)?;
    REGISTRY
        .get()
        .is_some_and(|registry| registry.contains_key(name))
        .then(|| name.clone())
}

/// Path to the running executable (cached).
#[cfg(all(
    feature = "carried-coreutils",
    any(target_os = "linux", target_os = "macos")
))]
fn self_exe() -> Option<&'static PathBuf> {
    SELF_EXE
        .get_or_init(|| std::env::current_exe().ok())
        .as_ref()
}

/// Help/usage content provider for the shim builtin.
#[cfg(feature = "carried-coreutils")]
#[allow(
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    reason = "signature dictated by brush_core::builtins::CommandContentFunc"
)]
fn shim_content(
    name: &str,
    content_type: ContentType,
    _options: &ContentOptions,
) -> Result<String, brush_core::Error> {
    match content_type {
        ContentType::ShortDescription => Ok(format!("{name} - carried bundled command")),
        ContentType::DetailedHelp => Ok(format!(
            "{name} - carried bundled command (runs via `<self> {DISPATCH_FLAG} {name}`)\n"
        )),
        ContentType::ShortUsage | ContentType::ManPage => Ok(String::new()),
    }
}

/// Builtin execute function shared by all shims: re-exec the running executable
/// as `<self> --invoke-bundled <name> <args>`. This private spawn uses the same
/// transform and final authorization filters as normal external commands, then
/// attaches the authenticated channel and inherits the shell's redirections.
#[cfg(feature = "carried-coreutils")]
fn shim_execute<SE: ShellExtensions>(
    context: ExecutionContext<'_, SE>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<brush_core::ExecutionResult, brush_core::Error>> {
    Box::pin(async move {
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (context, args);
            Ok(ExecutionExitCode::CannotExecute.into())
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let exe_path = if let Some(p) = self_exe() {
                p.to_string_lossy().into_owned()
            } else {
                let _ = writeln!(
                    context.stderr(),
                    "carried-coreutils: cannot determine path to running executable"
                );
                return Ok(ExecutionExitCode::CannotExecute.into());
            };

            let bundled_name = context.command_name.clone();
            let user_args: Vec<String> = args
                .into_iter()
                .skip(1)
                .map(|arg| arg.to_string())
                .collect();
            let mut child_args = Vec::with_capacity(user_args.len() + 3);
            child_args.push(DISPATCH_FLAG.to_string());
            child_args.push(bundled_name.clone());
            child_args.push(LIVE_PARENT_FLAG.to_string());
            child_args.extend(user_args.iter().cloned());

            let command = commands::compose_external_command(
                context.shell,
                &exe_path,
                &bundled_name,
                &child_args,
                false,
            );
            let filter = context.shell.cmd_exec_filter().clone();
            let params =
                ExternalCmdParams::with_original_command(context.shell, &bundled_name, command);
            let params = match filter.pre_external_cmd(params).await {
                PreFilterResult::Continue(params) => params,
                PreFilterResult::Return(output) => return wait_filtered_output(output).await,
                _ => {
                    return brush_core::error::unimp(
                        "unrecognized carried-command filter result; refusing private dispatch",
                    );
                }
            };
            filter.authorize_external_cmd(&params).await?;
            // Bind the private authentication frame to the final program/argv.
            // A filter may change utility arguments but cannot repurpose this
            // authenticated transport as a launch path for another executable.
            let protocol_args: Vec<String> = params
                .command
                .args()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            let bundled_name = logical_carried_command(
                &params.command.program().to_string_lossy(), &protocol_args,
            ).ok_or_else(|| brush_core::Error::from(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "transformed carried command no longer identifies the authenticated utility protocol",
            )))?;
            let identity_args = params.command.args()[3..].to_vec();
            let output: ExternalCmdOutput = async {
                for arg in &identity_args {
                    let path = std::path::Path::new(arg);
                    if crate::caveat_interceptor::is_private_descriptor_path(path) {
                        context
                            .shell
                            .file_open_filter()
                            .pre_open_file(FileOpenParams::new(
                                context.shell,
                                path,
                                path,
                                FileOpenAccess::Read,
                            ))?;
                    }
                }
                // Preserve the exact authorized env, cwd, argv0 and argv. Only
                // descriptors are attached after final authorization.
                let mut cmd = commands::compose_filtered_std_command(&context, params.command)?;
                let (parent_control, child_control) =
                    UnixStream::pair().map_err(brush_core::Error::from)?;
                crate::private_control::prepare_carried_parent(&parent_control)
                    .map_err(|error| brush_core::Error::from(std::io::Error::other(error)))?;
                let child_control = OwnedFd::from(child_control);
                cmd.stderr(std::process::Stdio::from(child_control));
                // #319: close ambient descriptors >= 3 in the child. The authenticated
                // control channel rides in as stderr (fd 2) and is preserved; only
                // un-delegated ambient capabilities are closed. Enforced on Linux
                // (close_range) and macOS (planned FD_CLOEXEC sweep, refusing the
                // spawn when the descriptor universe cannot be bounded, #352).
                #[cfg(unix)]
                agent_bridle_fdguard::deny_inherited_fds(&mut cmd);
                let mut child = cmd.spawn().map_err(|error| {
                    brush_core::ErrorKind::FailedToExecuteCommand(bundled_name.clone(), error)
                })?;
                // `Command` retains the configured child endpoint after `spawn` on
                // some standard-library implementations. Drop it before authentication
                // so model-visible code cannot inherit or duplicate that endpoint.
                drop(cmd);

                let identity =
                    crate::private_control::carried_identity(bundled_name.as_ref(), &identity_args);
                let child_pid = child.id();
                let mut child_stderr = match crate::private_control::authorize_carried_child(
                    parent_control,
                    child_pid,
                    &identity,
                ) {
                    Ok(stderr) => stderr,
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(std::io::Error::other(format!(
                            "carried command private authentication failed: {error}"
                        ))
                        .into());
                    }
                };

                // Authentication consumed only binary frames. From here on the socket
                // is a transparent proxy for the utility's actual stderr. Stdin and
                // stdout retain the redirection state composed by Brush.
                let mut command_stderr = context.stderr();
                let stderr_proxy = std::thread::spawn(move || {
                    let _ = std::io::copy(&mut child_stderr, &mut command_stderr);
                });

                let status = tokio::task::spawn_blocking(move || child.wait())
                    .await
                    .map_err(|error| {
                        brush_core::Error::from(std::io::Error::other(format!(
                            "join carried command: {error}"
                        )))
                    })?
                    .map_err(brush_core::Error::from)?;
                let _ = stderr_proxy.join();
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "shell exit status is conventionally the low 8 bits"
                )]
                let exit_code = status.code().map_or(127, |code| (code & 0xff) as u8);
                Ok(brush_core::ExecutionResult::new(exit_code).into())
            }
            .await;
            match filter.post_external_cmd(output).await {
                PostFilterResult::Return(output) => wait_filtered_output(output).await,
                _ => brush_core::error::unimp("unrecognized carried-command post-filter result"),
            }
        }
    })
}

#[cfg(all(
    feature = "carried-coreutils",
    any(target_os = "linux", target_os = "macos")
))]
async fn wait_filtered_output(
    output: ExternalCmdOutput,
) -> Result<brush_core::ExecutionResult, brush_core::Error> {
    match output?.wait().await? {
        brush_core::results::ExecutionWaitResult::Completed(result) => Ok(result),
        brush_core::results::ExecutionWaitResult::Stopped(_) => {
            brush_core::error::unimp("filter returned a stopped process from carried dispatch")
        }
    }
}

/// A [`Registration`] for the shim builtin (reused for every bundled name).
#[cfg(feature = "carried-coreutils")]
fn shim_registration<SE: ShellExtensions>() -> Registration<SE> {
    Registration {
        execute_func: shim_execute::<SE>,
        content_func: shim_content,
        disabled: false,
        special_builtin: false,
        declaration_builtin: false,
    }
}

/// Register a shim builtin for every name in the installed registry, using
/// `register_builtin_if_unset` so brush's own builtins win on conflict. Call
/// [`install_default_providers`] first.
#[cfg(feature = "carried-coreutils")]
pub fn register_shims<SE: ShellExtensions>(shell: &mut brush_core::Shell<SE>) {
    let Some(registry) = REGISTRY.get() else {
        return;
    };
    for name in registry.keys() {
        shell.register_builtin_if_unset(name.clone(), shim_registration::<SE>());
    }
}

#[cfg(all(test, feature = "carried-coreutils"))]
mod tests {
    use super::*;

    #[test]
    fn carried_reexec_is_authorized_by_logical_name() {
        install_default_providers();
        let executable = std::env::current_exe().expect("current test executable");
        let args = vec![
            DISPATCH_FLAG.to_string(),
            "cat".to_string(),
            LIVE_PARENT_FLAG.to_string(),
            "fixture.txt".to_string(),
        ];
        assert_eq!(
            logical_carried_command(&executable.to_string_lossy(), &args).as_deref(),
            Some("cat")
        );
    }

    #[test]
    fn lookalike_dispatch_is_not_a_carried_command() {
        install_default_providers();
        let executable = std::env::current_exe().expect("current test executable");
        assert!(logical_carried_command(
            &executable.to_string_lossy(),
            &[DISPATCH_FLAG.to_string(), "not-carried".to_string()]
        )
        .is_none());
        assert!(logical_carried_command(
            "/bin/echo",
            &[
                DISPATCH_FLAG.to_string(),
                "cat".to_string(),
                LIVE_PARENT_FLAG.to_string()
            ]
        )
        .is_none());
    }

    #[test]
    fn bundled_identity_requires_the_live_parent_protocol_marker() {
        install_default_providers();
        let executable = std::env::current_exe().expect("current test executable");
        assert!(
            logical_carried_command(
                &executable.to_string_lossy(),
                &[DISPATCH_FLAG.to_string(), "cat".to_string()]
            )
            .is_none(),
            "a bare private flag must not synthesize carried-command authority"
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn carried_identity_binds_name_argument_boundaries_and_raw_order() {
        let cat_ab = crate::private_control::carried_identity(
            std::ffi::OsStr::new("cat"),
            &[OsString::from("a"), OsString::from("b")],
        );
        let cat_joined = crate::private_control::carried_identity(
            std::ffi::OsStr::new("cat"),
            &[OsString::from("ab")],
        );
        let head_ab = crate::private_control::carried_identity(
            std::ffi::OsStr::new("head"),
            &[OsString::from("a"), OsString::from("b")],
        );
        assert_ne!(cat_ab, cat_joined);
        assert_ne!(cat_ab, head_ab);
    }
}
