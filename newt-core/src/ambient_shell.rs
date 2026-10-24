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
/// redirects), which no posture hands a child. Sorted by name.
#[must_use]
pub fn child_env(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    strip: &[&str],
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vars
        .into_iter()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| !strip.contains(&k.as_str()))
        .collect();
    env.sort();
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

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
}
