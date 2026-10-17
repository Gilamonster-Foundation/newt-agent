//! Pinned inference (#2927): a user, workspace, repo or session can require
//! that inference stay on sovereign backends.
//!
//! This module is the policy *value* and its discovery. It names a class
//! (`sovereign`), never an endpoint: which backends are sovereign is the
//! operator's call alone. Enforcement at the request funnel is a later step.
//!
//! Two properties hold by construction:
//! - **Pins only accumulate.** [`InferencePolicy`] has no API that removes a
//!   pin, and no config value means "unpinned", so a narrower scope can
//!   tighten a pin but never loosen one.
//! - **Discovery fails closed.** A candidate config that cannot be read, does
//!   not parse, or names an unknown class counts as pinned.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::{project_dirs_from, resolve_or_self, Config};

/// The class of inference a pin requires. `Sovereign` is the only value: any
/// other string in `[inference] pin` is a config problem, and fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinClass {
    Sovereign,
}

/// Where a pin came from, for `doctor`, the status line and refusals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinScope {
    User,
    Workspace,
    Repo,
    Session,
}

/// One pin: its class, where it was declared, and (when discovery could not
/// read the source) why it is being enforced anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub class: PinClass,
    pub scope: PinScope,
    pub source: Option<PathBuf>,
    pub problem: Option<String>,
}

/// The pins in force for a session. Private field: built by
/// [`InferencePolicy::discover`] and tightened only by [`pin_session`] or
/// [`meet`](InferencePolicy::meet).
///
/// [`pin_session`]: InferencePolicy::pin_session
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InferencePolicy {
    pins: Vec<Pin>,
}

#[derive(Deserialize)]
struct InferenceTable {
    inference: Option<InferenceSection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InferenceSection {
    pin: Option<PinClass>,
}

impl InferencePolicy {
    #[must_use]
    pub fn unpinned() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_pinned(&self) -> bool {
        !self.pins.is_empty()
    }

    #[must_use]
    pub fn pins(&self) -> &[Pin] {
        &self.pins
    }

    /// `/pin`: require sovereign inference for the rest of this session.
    #[must_use]
    pub fn pin_session(mut self) -> Self {
        self.pins.push(Pin {
            class: PinClass::Sovereign,
            scope: PinScope::Session,
            source: None,
            problem: None,
        });
        self
    }

    /// The union of two policies: everything either one requires.
    #[must_use]
    pub fn meet(mut self, other: Self) -> Self {
        self.pins.extend(other.pins);
        self
    }

    /// The policy for a session started in `cwd`: the user config plus every
    /// `.newt/config.toml` from `cwd` up to (not including) `$HOME`. The
    /// innermost project config is the repo scope; outer ones are workspace.
    /// Reads through `read` and resolves symlinks through `resolve`, so tests
    /// touch no filesystem; `NotFound` means "no file here", any other error
    /// fails closed. A misspelled key inside `[inference]` fails closed too.
    pub fn discover(
        cwd: std::io::Result<&Path>,
        home: Option<&Path>,
        user_config: Option<&Path>,
        read: &dyn Fn(&Path) -> std::io::Result<String>,
        resolve: &dyn Fn(&Path) -> PathBuf,
    ) -> Self {
        let mut policy = Self::unpinned();
        if let Some(path) = user_config {
            policy.read_scope(path, PinScope::User, read);
        }
        // Without a working directory the project pins cannot be found:
        // that is a pin of its own, not an absence.
        let cwd = match cwd {
            Ok(cwd) => cwd,
            Err(e) => {
                policy.pins.push(Pin {
                    class: PinClass::Sovereign,
                    scope: PinScope::Repo,
                    source: None,
                    problem: Some(format!("working directory unknown: {e}")),
                });
                return policy;
            }
        };
        let mut repo_seen = false;
        for dir in project_dirs_from(cwd, home, resolve) {
            let path = dir.join(".newt").join("config.toml");
            let scope = if repo_seen {
                PinScope::Workspace
            } else {
                PinScope::Repo
            };
            if policy.read_scope(&path, scope, read) {
                repo_seen = true;
            }
        }
        policy
    }

    /// [`discover`](Self::discover) against the real filesystem and process
    /// environment.
    #[must_use]
    pub fn discover_here() -> Self {
        let home = crate::config::home_dir();
        let user = Config::user_config_path();
        let cwd = std::env::current_dir();
        Self::discover(
            cwd.as_deref()
                .map_err(|e| std::io::Error::new(e.kind(), e.to_string())),
            home.as_deref(),
            user.as_deref(),
            &|p| std::fs::read_to_string(p),
            &resolve_or_self,
        )
    }

    /// Read one candidate file; returns whether a file existed there.
    fn read_scope(
        &mut self,
        path: &Path,
        scope: PinScope,
        read: &dyn Fn(&Path) -> std::io::Result<String>,
    ) -> bool {
        let problem = match read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(e) => Some(format!("unreadable: {e}")),
            Ok(text) => match toml::from_str::<InferenceTable>(&text) {
                Ok(InferenceTable {
                    inference: Some(InferenceSection { pin: Some(class) }),
                }) => {
                    self.push(class, scope, path, None);
                    return true;
                }
                Ok(_) => return true,
                Err(e) => Some(format!("cannot read [inference]: {}", e.message())),
            },
        };
        self.push(PinClass::Sovereign, scope, path, problem);
        true
    }

    fn push(&mut self, class: PinClass, scope: PinScope, path: &Path, problem: Option<String>) {
        self.pins.push(Pin {
            class,
            scope,
            source: Some(path.to_path_buf()),
            problem,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn reader(files: &[(&str, &str)]) -> impl Fn(&Path) -> std::io::Result<String> {
        let map: HashMap<PathBuf, String> = files
            .iter()
            .map(|(p, t)| (PathBuf::from(p), (*t).to_owned()))
            .collect();
        move |p| {
            map.get(p)
                .cloned()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        }
    }

    fn discover(files: &[(&str, &str)]) -> InferencePolicy {
        InferencePolicy::discover(
            Ok(Path::new("/w/org/repo")),
            Some(Path::new("/home/op")),
            Some(Path::new("/home/op/.newt/config.toml")),
            &reader(files),
            &same,
        )
    }

    /// The resolver for tests: no symlinks, no filesystem.
    fn same(p: &Path) -> PathBuf {
        p.to_path_buf()
    }

    const PINNED: &str = "[inference]\npin = \"sovereign\"\n";

    #[test]
    fn no_pin_anywhere_is_unpinned() {
        let p = discover(&[("/w/org/repo/.newt/config.toml", "[context]\n")]);
        assert!(!p.is_pinned());
    }

    #[test]
    fn a_repo_pin_is_found_with_its_source() {
        let p = discover(&[("/w/org/repo/.newt/config.toml", PINNED)]);
        assert_eq!(p.pins().len(), 1);
        assert_eq!(p.pins()[0].scope, PinScope::Repo);
        assert_eq!(
            p.pins()[0].source.as_deref(),
            Some(Path::new("/w/org/repo/.newt/config.toml"))
        );
    }

    /// A workspace pin covers a repo inside it that says nothing, and an
    /// unpinned repo config cannot loosen it.
    #[test]
    fn an_inner_unpinned_repo_stays_pinned_by_its_workspace() {
        let p = discover(&[
            ("/w/org/repo/.newt/config.toml", "[context]\n"),
            ("/w/org/.newt/config.toml", PINNED),
        ]);
        assert!(p.is_pinned());
        assert_eq!(p.pins()[0].scope, PinScope::Workspace);
    }

    #[test]
    fn pins_from_every_scope_accumulate() {
        let p = discover(&[
            ("/home/op/.newt/config.toml", PINNED),
            ("/w/org/.newt/config.toml", PINNED),
            ("/w/org/repo/.newt/config.toml", PINNED),
        ]);
        let scopes: Vec<_> = p.pins().iter().map(|p| p.scope.clone()).collect();
        assert_eq!(
            scopes,
            [PinScope::User, PinScope::Repo, PinScope::Workspace]
        );
    }

    /// No config value means "unpinned": an unknown class fails closed.
    #[test]
    fn an_unknown_pin_class_fails_closed() {
        for value in ["none", "Sovereign2", "false"] {
            let text = format!("[inference]\npin = \"{value}\"\n");
            let p = discover(&[("/w/org/repo/.newt/config.toml", &text)]);
            assert!(p.is_pinned(), "{value}");
            assert!(p.pins()[0].problem.is_some(), "{value}");
        }
    }

    #[test]
    fn an_unreadable_or_unparseable_config_fails_closed() {
        let p = discover(&[("/w/org/repo/.newt/config.toml", "[inference\n")]);
        assert!(p.is_pinned());
        let denied = InferencePolicy::discover(
            Ok(Path::new("/w/org/repo")),
            Some(Path::new("/home/op")),
            None,
            &|_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            &same,
        );
        assert!(denied.is_pinned());
    }

    /// The walk stops before `$HOME`, so the operator's own `~/.newt` is read
    /// once, as the user scope, never as a workspace.
    #[test]
    fn the_home_dir_is_not_a_workspace() {
        let p = InferencePolicy::discover(
            Ok(Path::new("/home/op/src/repo")),
            Some(Path::new("/home/op")),
            Some(Path::new("/home/op/.newt/config.toml")),
            &reader(&[("/home/op/.newt/config.toml", PINNED)]),
            &same,
        );
        assert_eq!(p.pins().len(), 1);
        assert_eq!(p.pins()[0].scope, PinScope::User);
    }

    #[test]
    fn session_pin_and_meet_only_tighten() {
        let p = InferencePolicy::unpinned().pin_session();
        assert!(p.is_pinned());
        let met = InferencePolicy::unpinned().meet(p.clone());
        assert_eq!(met, p);
    }

    /// A repo may restrict, so `[inference]` must never become a stripped
    /// control-plane key.
    #[test]
    fn inference_is_not_a_control_plane_key() {
        assert!(!crate::config::CONTROL_PLANE_KEYS.contains(&"inference"));
    }

    /// #2931 review: a misspelled key inside `[inference]` decoded as "no pin".
    #[test]
    fn a_misspelled_inference_key_fails_closed() {
        for text in [
            "[inference]\npinn = \"sovereign\"\n",
            "[inference]\nsovereign = true\n",
            "[inference]\nPIN = \"sovereign\"\n",
        ] {
            let p = discover(&[("/w/org/repo/.newt/config.toml", text)]);
            assert!(p.is_pinned(), "{text}");
            assert!(p.pins()[0].problem.is_some(), "{text}");
        }
    }

    /// #2931 review: an unknown working directory used to discover nothing.
    #[test]
    fn an_unknown_working_directory_fails_closed() {
        let p = InferencePolicy::discover(
            Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            Some(Path::new("/home/op")),
            None,
            &reader(&[]),
            &same,
        );
        assert!(p.is_pinned());
        let problem = p.pins()[0].problem.as_deref().unwrap();
        assert!(problem.contains("working directory"), "{problem}");
    }
}
