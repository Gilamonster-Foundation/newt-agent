//! The `--ambient` posture's shell and environment (#978).
//!
//! Under [`crate::launch_authority::LaunchAuthority::ambient`], `run_command` runs the
//! operator's login shell (`$SHELL`) instead of `bash -c`, so zsh functions, aliases and
//! rc-set `PATH` exist, and the child (and stdio MCP servers) keep the full process
//! environment, so `DISPLAY`, `WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS` and
//! `XDG_RUNTIME_DIR` reach a browser launch.
//!
//! Both are pure here: the caller supplies `$SHELL` and the environment, so the unit tier
//! needs no process env. Every other posture is untouched; nothing here runs unless the
//! caller already saw `ambient()`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Shells that take a combined login + interactive `-l -i -c <cmd>`: interactive is what
/// reads `~/.zshrc` / `~/.bashrc`, where aliases and functions live.
const LOGIN_INTERACTIVE: &[&str] = &["bash", "zsh", "fish", "ksh"];

// A file can exist and still not start (a missing interpreter in its shebang). Once a spawn
// showed that, the rest of the process runs `/bin/sh` and says so. A process-wide flag in
// production; thread-local under test so parallel tests do not share it.
#[cfg(not(test))]
static LOGIN_SHELL_FAILED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
thread_local! {
    static LOGIN_SHELL_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Record that the login shell could not be started, so dispatch, banner and guidance all
/// treat it as `/bin/sh` from now on.
pub fn mark_login_shell_failed() {
    #[cfg(not(test))]
    LOGIN_SHELL_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    LOGIN_SHELL_FAILED.with(|f| f.set(true));
}

/// Has a spawn shown the login shell cannot start?
#[must_use]
pub fn login_shell_failed() -> bool {
    #[cfg(not(test))]
    return LOGIN_SHELL_FAILED.load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    LOGIN_SHELL_FAILED.with(std::cell::Cell::get)
}

/// What the model is told when `run_command` runs fish: not POSIX.
pub const FISH_DIALECT: &str = "Commands run in the operator's fish shell, not a POSIX shell: use fish syntax (`set name value` and `$name`, `and`/`or` or `; and`, `(cmd)` for substitution), with no heredocs and no `${var:-default}`; run_command already returns the exit status.";

/// Do two environment variable names name the same variable? Windows names are
/// case-insensitive (and case-preserving); elsewhere they are exact.
#[must_use]
pub fn same_env_key(a: &str, b: &str, case_insensitive: bool) -> bool {
    if case_insensitive {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// What `$SHELL` resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmbientShell {
    /// Run this program with `args` then the command (`-l -i -c` / `-l -c`).
    Login {
        program: PathBuf,
        args: &'static [&'static str],
        /// Whether the command text is POSIX (fish's is not, so it is passed untouched).
        posix: bool,
    },
    /// `$SHELL` cannot be trusted; run `/bin/sh -c` and say why.
    Fallback { reason: &'static str },
}

impl AmbientShell {
    /// Resolve `$SHELL`. Unset, empty or not absolute fails closed to [`Self::Fallback`]:
    /// a relative `$SHELL` would be resolved against the workspace the agent can write.
    #[must_use]
    pub fn resolve(shell: Option<&OsStr>) -> Self {
        let Some(shell) = shell.filter(|s| !s.is_empty()) else {
            return Self::Fallback {
                reason: "$SHELL is not set",
            };
        };
        let path = Path::new(shell);
        if !path.is_absolute() {
            return Self::Fallback {
                reason: "$SHELL is not an absolute path",
            };
        }
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        let interactive = LOGIN_INTERACTIVE.contains(&name);
        Self::Login {
            program: path.to_path_buf(),
            args: if interactive {
                &["-l", "-i", "-c"]
            } else {
                &["-l", "-c"]
            },
            posix: name != "fish",
        }
    }

    /// [`Self::resolve`], then fail closed when the login shell is not a file on this host:
    /// a path that does not exist cannot run, and saying otherwise would mislead.
    #[must_use]
    pub fn resolve_checked(shell: Option<&OsStr>, exists: impl Fn(&Path) -> bool) -> Self {
        match Self::resolve(shell) {
            Self::Login { program, .. } if !exists(&program) => Self::Fallback {
                reason: "$SHELL does not exist",
            },
            other => other,
        }
    }

    /// The shell this process would run for ambient `run_command`: `$SHELL`, checked
    /// against the file system, and demoted to `/bin/sh` for the rest of the process once a
    /// spawn showed it cannot start (see [`mark_login_shell_failed`]). The one resolution
    /// the dispatch, the banner and the prompt guidance all use, so they cannot disagree.
    #[must_use]
    pub fn resolve_host() -> Self {
        if login_shell_failed() {
            return Self::Fallback {
                reason: "the login shell could not be started",
            };
        }
        Self::resolve_checked(std::env::var_os("SHELL").as_deref(), Path::is_file)
    }

    /// The model-facing dialect sentence when this shell is not POSIX (fish); `None` when
    /// the default POSIX guidance is right.
    #[must_use]
    pub fn dialect_sentence(&self) -> Option<&'static str> {
        match self {
            Self::Login { posix: false, .. } => Some(FISH_DIALECT),
            _ => None,
        }
    }

    /// One line for the session banner: which shell runs, or why `/bin/sh` does.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Login { program, args, .. } => {
                format!("login shell {} {}", program.display(), args.join(" "))
            }
            Self::Fallback { reason } => format!("{reason}; using /bin/sh"),
        }
    }
}

/// The full process environment for an ambient child: every variable that is valid text,
/// minus newt's own control plane (`strip`: authority switches, newt's secrets, config
/// redirects), which no posture hands a child. Sorted by name. Names compare the way the
/// host does (case-insensitively on Windows).
#[must_use]
pub fn child_env(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    strip: &[&str],
) -> Vec<(String, String)> {
    child_env_with(vars, strip, cfg!(windows))
}

/// [`child_env`] with the name comparison chosen by the caller, so both can be tested anywhere.
#[must_use]
pub fn child_env_with(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    strip: &[&str],
    case_insensitive: bool,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vars
        .into_iter()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| !strip.iter().any(|s| same_env_key(s, k, case_insensitive)))
        .collect();
    env.sort();
    env
}

/// Drop the two lines an interactive bash prints when it has no controlling terminal
/// (`cannot set terminal process group`, `no job control`): they come from the `-i` this
/// posture asks for, not from the command, and would otherwise lead every result.
/// Only those exact diagnostics, wherever they appear; every other byte is kept.
#[must_use]
pub fn strip_interactive_noise(stderr: &[u8]) -> Vec<u8> {
    let noise = |line: &[u8]| {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end();
        line == "bash: no job control in this shell"
            || (line.starts_with("bash: cannot set terminal process group (")
                && line.ends_with("): Inappropriate ioctl for device"))
    };
    let mut out = Vec::with_capacity(stderr.len());
    for line in stderr.split_inclusive(|b| *b == b'\n') {
        if !noise(line) {
            out.extend_from_slice(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[cfg(unix)]
    #[test]
    fn an_absolute_login_shell_runs_login_and_interactive() {
        for (shell, args, posix) in [
            ("/usr/bin/zsh", &["-l", "-i", "-c"][..], true),
            ("/bin/bash", &["-l", "-i", "-c"][..], true),
            ("/usr/bin/fish", &["-l", "-i", "-c"][..], false),
            ("/bin/dash", &["-l", "-c"][..], true),
        ] {
            let got = AmbientShell::resolve(os(shell).as_deref());
            assert_eq!(
                got,
                AmbientShell::Login {
                    program: PathBuf::from(shell),
                    args: match args.len() {
                        3 => &["-l", "-i", "-c"],
                        _ => &["-l", "-c"],
                    },
                    posix,
                },
                "{shell}"
            );
        }
    }

    /// Fail closed: nothing the agent could plant in a relative path runs as the shell.
    #[test]
    fn an_unset_empty_or_relative_shell_falls_back_and_says_why() {
        for (shell, why) in [
            (None, "$SHELL is not set"),
            (os(""), "$SHELL is not set"),
            (os("zsh"), "$SHELL is not an absolute path"),
            (os("./sh"), "$SHELL is not an absolute path"),
            (os("bin/zsh"), "$SHELL is not an absolute path"),
        ] {
            let got = AmbientShell::resolve(shell.as_deref());
            assert_eq!(got, AmbientShell::Fallback { reason: why }, "{shell:?}");
            assert!(
                got.describe().ends_with("using /bin/sh"),
                "{}",
                got.describe()
            );
        }
    }

    #[test]
    fn the_full_env_keeps_the_display_variables_and_drops_the_control_plane() {
        let vars = [
            ("DISPLAY", ":0"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("PATH", "/usr/bin"),
            ("NEWT_AGENT_KEY", "secret"),
            ("NEWT_FULL_ACCESS", "1"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = child_env(vars, &["NEWT_AGENT_KEY", "NEWT_FULL_ACCESS"]);
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "DBUS_SESSION_BUS_ADDRESS",
                "DISPLAY",
                "PATH",
                "WAYLAND_DISPLAY",
                "XDG_RUNTIME_DIR"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_variable_that_is_not_text_is_skipped_not_mangled() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0xff, 0xfe]);
        let env = child_env(
            [
                (OsString::from("GOOD"), OsString::from("1")),
                (OsString::from("BAD"), bad),
            ],
            &[],
        );
        assert_eq!(env, [("GOOD".to_string(), "1".to_string())]);
    }

    #[test]
    fn only_the_bash_job_control_diagnostics_are_stripped() {
        let noisy = b"bash: cannot set terminal process group (1544202): Inappropriate ioctl for device\nbash: no job control in this shell\nreal error\nbash: no job control in this shell extra\n";
        assert_eq!(
            String::from_utf8(strip_interactive_noise(noisy)).unwrap(),
            "real error\nbash: no job control in this shell extra\n"
        );
        assert_eq!(strip_interactive_noise(b""), b"");
        assert_eq!(strip_interactive_noise(b"no newline"), b"no newline");
    }

    #[test]
    fn windows_names_compare_case_insensitively_and_unix_names_exactly() {
        let vars = || {
            ["Newt_Agent_Key", "newt_operator_key", "Path", "KEEP"]
                .map(|k| (OsString::from(k), OsString::from("v")))
        };
        let strip = ["NEWT_AGENT_KEY", "NEWT_OPERATOR_KEY"];
        let windows: Vec<String> = child_env_with(vars(), &strip, true)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            windows,
            ["KEEP", "Path"],
            "mixed-case secrets are stripped on Windows"
        );
        let unix: Vec<String> = child_env_with(vars(), &strip, false)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(unix.len(), 4, "exact names elsewhere");
    }

    #[cfg(unix)]
    #[test]
    fn a_login_shell_that_is_not_a_file_falls_back() {
        let got = AmbientShell::resolve_checked(Some("/no/such/zsh".as_ref()), |_| false);
        assert_eq!(
            got,
            AmbientShell::Fallback {
                reason: "$SHELL does not exist"
            }
        );
        let ok = AmbientShell::resolve_checked(Some("/bin/sh".as_ref()), |_| true);
        assert!(matches!(ok, AmbientShell::Login { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn only_a_non_posix_login_shell_changes_the_dialect_guidance() {
        let fish = AmbientShell::resolve_checked(Some("/usr/bin/fish".as_ref()), |_| true);
        assert_eq!(fish.dialect_sentence(), Some(FISH_DIALECT));
        assert!(FISH_DIALECT.contains("not a POSIX shell") && FISH_DIALECT.contains("fish"));
        for shell in ["/bin/bash", "/usr/bin/zsh"] {
            assert_eq!(
                AmbientShell::resolve_checked(Some(shell.as_ref()), |_| true).dialect_sentence(),
                None
            );
        }
        assert_eq!(AmbientShell::resolve(None).dialect_sentence(), None);
    }
}
