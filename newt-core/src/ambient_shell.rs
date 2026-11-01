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

/// The login shells `--ambient` runs: POSIX-compatible ones only, so the POSIX guidance the
/// model always gets is true. `(basename, interactive)`; interactive (`-i`) is what reads
/// `~/.zshrc` / `~/.bashrc`, where aliases and functions live. Any other `$SHELL` (fish, nu,
/// xonsh, ...) runs `/bin/sh -c` instead.
const POSIX_LOGIN_SHELLS: &[(&str, bool)] = &[
    ("sh", false),
    ("dash", false),
    ("bash", true),
    ("zsh", true),
    ("ksh", true),
];

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
    },
    /// `$SHELL` cannot be used; run `/bin/sh -c` and say why.
    Fallback { reason: String },
}

impl AmbientShell {
    /// Resolve `$SHELL`. Unset, empty, relative (it would resolve against the workspace the
    /// agent can write) or not a POSIX shell fails closed to [`Self::Fallback`].
    #[must_use]
    pub fn resolve(shell: Option<&OsStr>) -> Self {
        let fallback = |reason: &str| Self::Fallback {
            reason: reason.to_string(),
        };
        let Some(shell) = shell.filter(|s| !s.is_empty()) else {
            return fallback("$SHELL is not set");
        };
        let path = Path::new(shell);
        if !path.is_absolute() {
            return fallback("$SHELL is not an absolute path");
        }
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        match POSIX_LOGIN_SHELLS.iter().find(|(n, _)| *n == name) {
            Some((_, interactive)) => Self::Login {
                program: path.to_path_buf(),
                args: if *interactive {
                    &["-l", "-i", "-c"]
                } else {
                    &["-l", "-c"]
                },
            },
            None => Self::Fallback {
                reason: format!("$SHELL is {name}, not a POSIX shell"),
            },
        }
    }

    /// [`Self::resolve`], then fail closed when the login shell is not a file on this host:
    /// a path that does not exist cannot run, and saying otherwise would mislead.
    #[must_use]
    pub fn resolve_checked(shell: Option<&OsStr>, exists: impl Fn(&Path) -> bool) -> Self {
        match Self::resolve(shell) {
            Self::Login { program, .. } if !exists(&program) => Self::Fallback {
                reason: "$SHELL does not exist".to_string(),
            },
            other => other,
        }
    }

    /// The shell this process would run for ambient `run_command`: `$SHELL`, checked against
    /// the file system. The one resolution the dispatch and the banner share.
    #[must_use]
    pub fn resolve_host() -> Self {
        Self::resolve_checked(std::env::var_os("SHELL").as_deref(), Path::is_file)
    }

    /// What runs, for the session banner.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Login { program, args } => {
                format!("login shell {} {}", program.display(), args.join(" "))
            }
            Self::Fallback { reason } => {
                format!("/bin/sh -c ({reason}; ambient commands run /bin/sh -c with your full environment)")
            }
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

    #[cfg(unix)]
    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    /// Fail closed: nothing the agent could plant in a relative path runs as the shell.
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
                reason: "$SHELL does not exist".to_string()
            }
        );
        let ok = AmbientShell::resolve_checked(Some("/bin/sh".as_ref()), |_| true);
        assert!(matches!(ok, AmbientShell::Login { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn a_posix_login_shell_runs_login_and_interactive_where_it_reads_its_rc() {
        for (shell, args) in [
            ("/usr/bin/zsh", &["-l", "-i", "-c"][..]),
            ("/bin/bash", &["-l", "-i", "-c"][..]),
            ("/usr/bin/ksh", &["-l", "-i", "-c"][..]),
            ("/bin/dash", &["-l", "-c"][..]),
            ("/bin/sh", &["-l", "-c"][..]),
        ] {
            match AmbientShell::resolve(os(shell).as_deref()) {
                AmbientShell::Login { program, args: got } => {
                    assert_eq!(program, PathBuf::from(shell));
                    assert_eq!(got, args, "{shell}");
                }
                other => panic!("{shell}: {other:?}"),
            }
        }
    }

    /// Fail closed: unset, empty, relative or non-POSIX `$SHELL` runs /bin/sh and says why.
    #[cfg(unix)]
    #[test]
    fn an_unusable_shell_falls_back_to_bin_sh_and_says_why() {
        for (shell, why) in [
            (None, "$SHELL is not set"),
            (os(""), "$SHELL is not set"),
            (os("zsh"), "$SHELL is not an absolute path"),
            (os("./sh"), "$SHELL is not an absolute path"),
            (os("/usr/bin/fish"), "$SHELL is fish, not a POSIX shell"),
            (os("/usr/bin/nu"), "$SHELL is nu, not a POSIX shell"),
        ] {
            let got = AmbientShell::resolve(shell.as_deref());
            assert_eq!(
                got,
                AmbientShell::Fallback {
                    reason: why.to_string()
                },
                "{shell:?}"
            );
            assert!(
                got.describe()
                    .contains("ambient commands run /bin/sh -c with your full environment"),
                "{}",
                got.describe()
            );
        }
    }
}
