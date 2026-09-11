//! Smart-harness configuration and the host-owned session authority boundary.

use std::{
    io::Read,
    path::{Path, PathBuf},
};

use agent_harness::{Session, SessionConfig};
use content_addressable::{canonical, ContentId, RawContentId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::BackendRef;
use crate::agentic::smart_harness::AdjudicationSettings;
use crate::caveats::{Caveats, Scope};

/// Opt-in accounted navigation and independent narration adjudication.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SmartHarnessConfig {
    /// Preserve installed behavior unless the operator enables this harness.
    pub enabled: bool,
    /// Host-private durable directory, outside every model file grant.
    /// Relative overrides are resolved against the workspace and must pass isolation.
    pub frame_dir: Option<PathBuf>,
    /// A fully pinned external auxiliary. It may name the primary backend's own origin;
    /// the run manifest records `shares_primary_origin` so runs stay comparable (D10).
    pub backend: Option<BackendRef>,
    /// Placement declaration (`cpu`, `cuda`, ...). The embedded auxiliary is cpu-only,
    /// so a non-cpu value requires `backend`. Required for an external
    /// auxiliary, recorded in the manifest verbatim, and NOT verified — the client cannot
    /// inspect a remote server's hardware.
    pub device: Option<String>,
    /// Embedded palette alias; absence uses the installed default palette model.
    pub model: Option<String>,
    /// Explicit embedded GGUF; absence resolves the installed palette model.
    pub model_path: Option<PathBuf>,
    /// Separate auxiliary timeout, call budget, and overridable protocol text.
    pub adjudication: AdjudicationSettings,
    /// Maximum entries in any relevance catalog.
    pub max_catalog_entries: usize,
    /// Total bytes fetched during one primary turn.
    pub max_fetched_bytes: usize,
    /// Maximum pointer dereferences during one primary turn.
    pub max_dereferences: usize,
    /// Maximum navigation calls during one primary turn.
    pub max_navigation_calls: usize,
    /// Rejected-proposal retry budget.
    pub max_retries: usize,
    /// Cumulative navigation work time, separate from the primary inference deadline.
    pub max_elapsed_ms: u64,
    /// Maximum records accepted in a session journal.
    pub max_history_nodes: usize,
    /// Maximum bytes in each persisted record or source, checked on reads and writes.
    pub max_record_bytes: usize,
    /// Maximum returned bytes in one reread slice.
    pub max_slice_bytes: usize,
}

impl Default for SmartHarnessConfig {
    fn default() -> Self {
        let limits = SessionConfig::default();
        Self {
            enabled: false,
            frame_dir: None,
            backend: None,
            device: None,
            model: None,
            model_path: None,
            adjudication: AdjudicationSettings::default(),
            max_catalog_entries: limits.max_catalog_entries,
            max_fetched_bytes: limits.max_fetched_bytes,
            max_dereferences: limits.max_dereferences,
            max_navigation_calls: limits.max_navigation_calls,
            max_retries: limits.max_retries,
            max_elapsed_ms: limits.max_elapsed_ms,
            max_history_nodes: limits.max_history_nodes,
            max_record_bytes: limits.max_record_bytes,
            max_slice_bytes: limits.max_slice_bytes,
        }
    }
}

/// Invocation inputs supplied by the host, never restored as authority from disk.
#[derive(Clone, Copy)]
pub struct HarnessLaunch<'a> {
    /// The current workspace, canonicalized before deriving authority.
    pub workspace: &'a Path,
    /// The current caller's actual tool caveats.
    pub caveats: &'a Caveats,
    /// Optional CLI directory override.
    pub frame_dir: Option<&'a Path>,
    /// Journal head selected explicitly by the operator.
    pub resume_from: Option<&'a str>,
    /// Suppress inherited conversation and ambient memory inputs.
    pub hermetic: bool,
}

impl HarnessLaunch<'_> {
    /// Identify current workspace/caveats before the session pins its private store.
    pub fn authority(&self) -> anyhow::Result<String> {
        authority_id(&self.authority_context()?)
    }

    /// Retain the current host scope so forensic readers can explain its identity.
    pub fn authority_context(&self) -> anyhow::Result<Value> {
        let workspace = self.workspace.canonicalize()?;
        let workspace = workspace
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("workspace must have a lossless UTF-8 name"))?;
        Ok(serde_json::json!({"workspace": workspace, "caveats": self.caveats}))
    }
}

impl SmartHarnessConfig {
    /// Resolve the manifest before creating or restoring any session capability.
    pub fn session_config(
        &self,
        launch: &HarnessLaunch<'_>,
        auxiliary: Value,
    ) -> anyhow::Result<SessionConfig> {
        let directory = self.directory(launch)?;
        self.session_config_at(launch, auxiliary, &directory)
    }

    fn session_config_at(
        &self,
        launch: &HarnessLaunch<'_>,
        auxiliary: Value,
        directory: &Path,
    ) -> anyhow::Result<SessionConfig> {
        anyhow::ensure!(
            !(launch.hermetic && launch.resume_from.is_some()),
            "hermetic execution cannot resume a frame"
        );
        self.adjudication.validate()?;
        let mut authority_context = launch.authority_context()?;
        authority_context["frame_directory"] = serde_json::to_value(directory)?;
        Ok(SessionConfig {
            authority: authority_id(&authority_context)?,
            authority_context: Some(authority_context),
            max_catalog_entries: self.max_catalog_entries,
            max_fetched_bytes: self.max_fetched_bytes,
            max_dereferences: self.max_dereferences,
            max_navigation_calls: self.max_navigation_calls,
            max_retries: self.max_retries,
            max_elapsed_ms: self.max_elapsed_ms,
            max_history_nodes: self.max_history_nodes,
            max_record_bytes: self.max_record_bytes,
            max_slice_bytes: self.max_slice_bytes,
            auxiliary,
            hermetic: launch.hermetic,
            admitted_inputs: [
                "task",
                "system",
                "tool_definitions",
                "workspace",
                "configuration",
                "auxiliary_model",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        })
    }

    /// Open durable state with current authority and exact current run settings.
    /// Restoration verifies the full graph before publishing its capability.
    pub fn open_session(
        &self,
        launch: &HarnessLaunch<'_>,
        auxiliary: Value,
    ) -> anyhow::Result<Session> {
        let dir = self.directory(launch)?;
        let config = self.session_config_at(launch, auxiliary, &dir)?;
        match launch.resume_from {
            Some(head) => Ok(Session::restore_with_config(dir, head.parse()?, &config)?),
            None => Ok(Session::open(dir, config)?),
        }
    }

    /// Resolve and validate storage before opening any frame or locator records.
    /// The default is workspace-specific host storage under the user config root.
    pub fn directory(&self, launch: &HarnessLaunch<'_>) -> anyhow::Result<PathBuf> {
        let workspace = launch.workspace.canonicalize()?;
        let dir = match launch.frame_dir.or(self.frame_dir.as_deref()) {
            Some(dir) if dir.is_absolute() => dir.to_path_buf(),
            Some(dir) => workspace.join(dir),
            None => super::Config::user_config_dir()
                .ok_or_else(|| {
                    anyhow::anyhow!("no host config directory for private frame storage")
                })?
                .join("frame")
                .join(
                    RawContentId::from_content(
                        workspace
                            .to_str()
                            .ok_or_else(|| {
                                anyhow::anyhow!("workspace must have a lossless UTF-8 name")
                            })?
                            .as_bytes(),
                    )
                    .to_string(),
                ),
        };
        isolated_directory(&dir, launch.caveats, &workspace)
    }

    /// Check the same store boundary again before dispatch or authority widening.
    pub fn validate_frame_directory(
        dir: &Path,
        caveats: &Caveats,
        workspace: &Path,
    ) -> anyhow::Result<()> {
        isolated_directory(dir, caveats, workspace).map(|_| ())
    }

    fn conversation_locator(directory: &Path, conversation: &str) -> PathBuf {
        directory
            .join("conversations")
            .join(RawContentId::from_content(conversation.as_bytes()).to_string())
    }

    /// Bind an interactive conversation to one immutable run, then follow that
    /// run's durable checkpoint on restart. The locator grants no authority;
    /// restoration still admits every record against the current configuration.
    pub fn open_conversation(
        &self,
        launch: &HarnessLaunch<'_>,
        conversation: &str,
        has_history: bool,
        auxiliary: Value,
    ) -> anyhow::Result<Session> {
        anyhow::ensure!(
            !launch.hermetic && launch.resume_from.is_none(),
            "conversation sessions require resumable invocation"
        );
        let directory = self.directory(launch)?;
        let config = self.session_config_at(launch, auxiliary, &directory)?;
        let locator = Self::conversation_locator(&directory, conversation);
        match std::fs::File::open(&locator) {
            Ok(file) => {
                let run = read_cid(file)?;
                let checkpoint = directory.join("heads").join(run.to_string());
                let head = read_cid(std::fs::File::open(checkpoint)?)?;
                let restored = Session::restore_with_config(&directory, head, &config)?;
                anyhow::ensure!(
                    restored.run_id() == run,
                    "conversation checkpoint points to a different run"
                );
                Ok(restored)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::ensure!(!has_history, "this conversation has no accounted frame; start a new conversation or resume an explicit frame CID with newt solve");
                let session = Session::open(&directory, config)?;
                let destination = crate::atomic_fs::ResolvedPath::resolve(&locator)?;
                let staged = destination.stage_private(session.run_id().to_string().as_bytes())?;
                if let Err(error) = destination.durable_create(&staged) {
                    let _ = std::fs::remove_file(&staged);
                    return Err(error);
                }
                let _ = std::fs::remove_file(&staged);
                Ok(session)
            }
            Err(error) => Err(error.into()),
        }
    }
}

fn isolated_directory(dir: &Path, caveats: &Caveats, workspace: &Path) -> anyhow::Result<PathBuf> {
    let dir = std::path::absolute(dir)?;
    let canonical = resolve_uncreated_path(&dir)?;
    let mut roots = Vec::new();
    let mut writable = Vec::new();
    // Automatic build checks use this calibrated authority independently of
    // the current tool gate; repository-authored commands must not read frames.
    let build = crate::confined_exec::build_tool_caveats(workspace);
    for (scope, write) in [
        (&caveats.fs_read, false),
        (&caveats.fs_write, true),
        (&build.fs_read, false),
        (&build.fs_write, true),
    ] {
        let Scope::Only(paths) = scope else {
            anyhow::bail!("smart-harness frame storage requires confined read and write authority");
        };
        roots.extend(paths.iter().map(PathBuf::from));
        if write {
            writable.extend(paths.iter().map(PathBuf::from));
        }
    }
    // The executor grants these paths in addition to explicit Caveats. Reuse
    // its policy data, including executable search roots, rather than treating
    // a narrow tool-level grant as the full filesystem authority.
    let policy = agent_bridle::SandboxPolicy::default();
    let device_sinks = policy.device_sink_paths.resolve();
    writable.extend(device_sinks.iter().map(PathBuf::from));
    for paths in [
        policy.base_read_paths.resolve(),
        policy.bin_read_paths.resolve(),
        device_sinks,
        policy.loader_paths.resolve(),
    ] {
        roots.extend(paths.into_iter().map(PathBuf::from));
    }
    if let Scope::Only(commands) = &caveats.exec {
        let path_bearing = |name: &str| {
            let path = Path::new(name);
            path.has_root()
                || path
                    .parent()
                    .is_some_and(|parent| !parent.as_os_str().is_empty())
        };
        roots.extend(
            commands
                .iter()
                .filter(|name| path_bearing(name))
                .map(PathBuf::from),
        );
        if commands.iter().any(|name| !path_bearing(name)) {
            // ponytail: exclude whole PATH directories; narrow to executable
            // files when bridle exposes its currently private resolver.
            if let Some(path) = std::env::var_os("PATH") {
                roots.extend(std::env::split_paths(&path).filter(|p| !p.as_os_str().is_empty()));
            }
        }
    }
    let writable = writable
        .into_iter()
        .map(|root| {
            let root = std::path::absolute(root)?;
            Ok((normalize_path(&root)?, resolve_uncreated_path(&root)?))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let lexical = normalize_path(&dir)?;
    for root in roots {
        anyhow::ensure!(
            !root
                .components()
                .any(|part| part == std::path::Component::ParentDir),
            "smart-harness filesystem grants must not contain parent traversal"
        );
        let root = std::path::absolute(root)?;
        let resolved = resolve_uncreated_path(&root)?;
        let named = normalize_path(&root)?;
        anyhow::ensure!(
            !overlaps(&canonical, &resolved) && !overlaps(&lexical, &named),
            "smart-harness frame storage overlaps model filesystem authority"
        );
        validate_stable_anchor(&root, &writable)?;
    }
    Ok(canonical)
}

/// Landlock opens grant roots after admission. Refuse roots or traversed
/// symlink targets whose parents the model could replace before that open.
fn validate_stable_anchor(path: &Path, writable: &[(PathBuf, PathBuf)]) -> anyhow::Result<()> {
    let mut pending = vec![path.to_path_buf()];
    let mut expanded = std::collections::BTreeSet::new();
    while let Some(path) = pending.pop() {
        for ancestor in path.ancestors() {
            if let Some(parent) = ancestor.parent() {
                let named = normalize_path(parent)?;
                let resolved = resolve_uncreated_path(parent)?;
                anyhow::ensure!(
                    !writable.iter().any(|(write_name, write_target)| {
                        named.starts_with(write_name) || resolved.starts_with(write_target)
                    }),
                    "smart-harness filesystem grant anchor has a model-writable ancestor"
                );
            }
            match std::fs::symlink_metadata(ancestor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    if expanded.insert(ancestor.to_path_buf()) {
                        anyhow::ensure!(
                            expanded.len() <= 40,
                            "frame authority symlink inspection limit"
                        );
                        let target = std::fs::read_link(ancestor)?;
                        pending.push(if target.is_absolute() {
                            target
                        } else {
                            ancestor
                                .parent()
                                .ok_or_else(|| anyhow::anyhow!("symlink anchor has no parent"))?
                                .join(target)
                        });
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn normalize_path(path: &Path) -> anyhow::Result<PathBuf> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("frame authority paths must have lossless UTF-8 names"))?;
    Ok(crate::caveats::lexically_normalize(path))
}

/// Resolve existing ancestors without creating paths named by untrusted grants.
/// Unlike ResolvedPath's write setup, admission must have no filesystem effects.
fn resolve_uncreated_path(path: &Path) -> anyhow::Result<PathBuf> {
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let resolved = ancestor.canonicalize()?.join(path.strip_prefix(ancestor)?);
                return normalize_path(&resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("frame authority path has no existing ancestor")
}

fn read_cid(file: std::fs::File) -> anyhow::Result<ContentId> {
    let mut value = String::new();
    file.take(257).read_to_string(&mut value)?;
    anyhow::ensure!(value.len() <= 256, "oversized frame locator");
    Ok(value.trim().parse()?)
}

fn authority_id(context: &Value) -> anyhow::Result<String> {
    Ok(ContentId::from_canonical_bytes(&canonical::to_canonical_dagcbor(context)?).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_same_directory(left: &Path, right: &Path) {
        assert_eq!(left.canonicalize().unwrap(), right.canonicalize().unwrap());
    }

    #[test]
    fn configuration_is_opt_in_and_roundtrips_explicit_budgets() {
        let config: SmartHarnessConfig =
            toml::from_str("enabled = true\nmax_dereferences = 2").unwrap();
        assert!(config.enabled);
        assert_eq!(config.max_dereferences, 2);
        assert!(!SmartHarnessConfig::default().enabled);
        let config: SmartHarnessConfig =
            toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(config.max_dereferences, 2);
    }

    /// Grounds the store's authority boundary in a real workspace directory:
    /// ordinary file tools must never bypass mediated rereads of the frame.
    #[test]
    fn frame_storage_refuses_model_visible_workspace_before_writing_records() {
        let workspace = tempfile::tempdir().unwrap();
        let mut caveats = Caveats::top();
        crate::caveats::lock_fs_to_workspace(
            &mut caveats,
            workspace.path().to_str().unwrap(),
            &[],
            &[],
        );
        let store = workspace.path().join(".newt/frame");
        let launch = HarnessLaunch {
            workspace: workspace.path(),
            caveats: &caveats,
            frame_dir: Some(&store),
            resume_from: None,
            hermetic: false,
        };
        assert!(SmartHarnessConfig::default()
            .open_session(&launch, Value::Null)
            .is_err());
        assert!(!store.exists());
    }

    #[test]
    fn frame_storage_requires_disjoint_read_and_write_grants() {
        let workspace = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let store = private.path().join("frame");
        let base = crate::confined_exec::build_tool_caveats(workspace.path());
        for (read, write) in [
            (crate::Scope::All, base.fs_write.clone()),
            (base.fs_read.clone(), crate::Scope::All),
            (
                crate::Scope::only([private.path().to_str().unwrap().to_owned()]),
                base.fs_write.clone(),
            ),
            (
                base.fs_read.clone(),
                crate::Scope::only([store.join("child").to_str().unwrap().to_owned()]),
            ),
        ] {
            let caveats = Caveats {
                fs_read: read,
                fs_write: write,
                ..base.clone()
            };
            let launch = HarnessLaunch {
                workspace: workspace.path(),
                caveats: &caveats,
                frame_dir: Some(&store),
                resume_from: None,
                hermetic: false,
            };
            assert!(SmartHarnessConfig::default()
                .open_session(&launch, Value::Null)
                .is_err());
            assert!(!store.exists());
        }
        let launch = HarnessLaunch {
            workspace: workspace.path(),
            caveats: &base,
            frame_dir: Some(&store),
            resume_from: None,
            hermetic: false,
        };
        assert!(SmartHarnessConfig::default()
            .open_session(&launch, Value::Null)
            .is_ok());
    }

    #[test]
    #[serial_test::serial]
    fn default_frame_directory_is_outside_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let caveats = crate::confined_exec::build_tool_caveats(workspace.path());
        let launch = HarnessLaunch {
            workspace: workspace.path(),
            caveats: &caveats,
            frame_dir: None,
            resume_from: None,
            hermetic: false,
        };
        let directory = SmartHarnessConfig::default().directory(&launch).unwrap();
        assert!(!directory.starts_with(workspace.path()));
    }

    #[test]
    fn frame_storage_excludes_implicit_sandbox_and_executable_roots() {
        let workspace = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let mut caveats = crate::confined_exec::build_tool_caveats(workspace.path());
        for root in agent_bridle::SandboxPolicy::default()
            .base_read_paths
            .resolve()
        {
            assert!(SmartHarnessConfig::validate_frame_directory(
                Path::new(&root),
                &caveats,
                workspace.path()
            )
            .is_err());
        }
        let executable = private.path().join("tool");
        std::fs::write(&executable, "fixture").unwrap();
        caveats.exec = Scope::only([executable.to_str().unwrap().to_owned()]);
        assert!(SmartHarnessConfig::validate_frame_directory(
            private.path(),
            &caveats,
            workspace.path()
        )
        .is_err());
    }

    #[test]
    fn frame_storage_excludes_automatic_build_cache_grants() {
        let workspace = tempfile::tempdir().unwrap();
        let mut caveats = Caveats::top();
        crate::caveats::lock_fs_to_workspace(
            &mut caveats,
            workspace.path().to_str().unwrap(),
            &[],
            &[],
        );
        let Scope::Only(build_reads) =
            crate::confined_exec::build_tool_caveats(workspace.path()).fs_read
        else {
            panic!("automatic build reads must be scoped");
        };
        for cache in build_reads
            .iter()
            .filter(|root| Path::new(root) != workspace.path())
        {
            assert!(SmartHarnessConfig::validate_frame_directory(
                Path::new(cache),
                &caveats,
                workspace.path()
            )
            .is_err());
        }
    }

    /// Grounds sandbox admission in real grant anchors: a writable parent can
    /// replace a directory or symlink after validation but before Landlock opens it.
    #[cfg(unix)]
    #[test]
    fn frame_storage_refuses_mutable_sandbox_grant_anchors() {
        let workspace = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let store = private.path().join("frame");
        let anchor = workspace.path().join("anchor");
        std::fs::create_dir(&anchor).unwrap();
        let alias = workspace.path().join("alias");
        std::os::unix::fs::symlink(other.path(), &alias).unwrap();
        for root in [&anchor, &alias] {
            for write in [false, true] {
                let mut caveats = crate::confined_exec::build_tool_caveats(workspace.path());
                let scope = if write {
                    &mut caveats.fs_write
                } else {
                    &mut caveats.fs_read
                };
                let Scope::Only(roots) = scope else {
                    panic!("fixture authority must be scoped");
                };
                roots.insert(root.to_str().unwrap().to_owned());
                assert!(SmartHarnessConfig::validate_frame_directory(
                    &store,
                    &caveats,
                    workspace.path()
                )
                .is_err());
            }
        }
        assert!(!store.exists());
    }

    /// Grounds traversal refusal in a real path that resolves outside the
    /// workspace today but crosses a component the model can later replace.
    #[test]
    fn frame_storage_refuses_parent_traversal_in_sandbox_grant_anchors() {
        let paths = tempfile::tempdir().unwrap();
        let workspace = paths.path().join("workspace");
        std::fs::create_dir_all(workspace.join("mutable")).unwrap();
        std::fs::create_dir(paths.path().join("outside")).unwrap();
        let anchor = workspace.join("mutable/../../outside");
        assert_same_directory(&anchor, &paths.path().join("outside"));
        let mut caveats = crate::confined_exec::build_tool_caveats(&workspace);
        let Scope::Only(reads) = &mut caveats.fs_read else {
            panic!("fixture authority must be scoped");
        };
        reads.insert(anchor.to_str().unwrap().to_owned());
        let store = paths.path().join("private/frame");
        assert!(
            SmartHarnessConfig::validate_frame_directory(&store, &caveats, &workspace).is_err()
        );
        assert!(!store.exists());
    }

    /// Grounds stability checks in a symlink chain whose mutable intermediate
    /// target is absent from both the named and fully resolved anchor paths.
    #[cfg(unix)]
    #[test]
    fn frame_storage_refuses_mutable_intermediate_symlink_targets() {
        let workspace = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let safe = tempfile::tempdir().unwrap();
        let mutable = workspace.path().join("mutable");
        std::os::unix::fs::symlink(safe.path(), &mutable).unwrap();
        let anchor = private.path().join("anchor");
        std::os::unix::fs::symlink(&mutable, &anchor).unwrap();
        assert_same_directory(&anchor, safe.path());
        let mut caveats = crate::confined_exec::build_tool_caveats(workspace.path());
        let Scope::Only(reads) = &mut caveats.fs_read else {
            panic!("fixture authority must be scoped");
        };
        reads.insert(anchor.to_str().unwrap().to_owned());
        let store = private.path().join("frame");
        assert!(
            SmartHarnessConfig::validate_frame_directory(&store, &caveats, workspace.path())
                .is_err()
        );
        assert!(!store.exists());
    }

    /// Grounds canonical containment in actual store and grant symlinks.
    #[cfg(unix)]
    #[test]
    fn frame_storage_rejects_store_and_grant_symlink_aliases() {
        let workspace = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let outside = private.path().join("workspace-link");
        std::os::unix::fs::symlink(workspace.path(), &outside).unwrap();
        let caveats = crate::confined_exec::build_tool_caveats(workspace.path());
        let store = outside.join("frame");
        let launch = HarnessLaunch {
            workspace: workspace.path(),
            caveats: &caveats,
            frame_dir: Some(&store),
            resume_from: None,
            hermetic: false,
        };
        assert!(SmartHarnessConfig::default()
            .open_session(&launch, Value::Null)
            .is_err());
        let grant = workspace.path().join("private-link");
        std::os::unix::fs::symlink(private.path(), &grant).unwrap();
        let caveats = Caveats {
            fs_read: crate::Scope::only([grant.to_str().unwrap().to_owned()]),
            ..caveats.clone()
        };
        let store = private.path().join("frame");
        assert!(SmartHarnessConfig::default()
            .open_session(
                &HarnessLaunch {
                    caveats: &caveats,
                    frame_dir: Some(&store),
                    ..launch
                },
                Value::Null,
            )
            .is_err());
        assert!(!store.exists());
    }

    #[test]
    fn authority_is_derived_from_the_current_workspace_and_caveats() {
        let workspace = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let caveats = Caveats::top();
        let launch = HarnessLaunch {
            workspace: workspace.path(),
            caveats: &caveats,
            frame_dir: None,
            resume_from: None,
            hermetic: false,
        };
        let same = HarnessLaunch {
            workspace: &workspace.path().join("."),
            ..launch
        };
        assert_eq!(launch.authority().unwrap(), same.authority().unwrap());
        let changed = HarnessLaunch {
            workspace: other.path(),
            ..launch
        };
        assert_ne!(launch.authority().unwrap(), changed.authority().unwrap());
        let mut narrower = caveats.clone();
        narrower.fs_write = crate::Scope::none();
        let changed = HarnessLaunch {
            caveats: &narrower,
            ..launch
        };
        assert_ne!(launch.authority().unwrap(), changed.authority().unwrap());
    }

    // APFS rejects invalid UTF-8 names before the authority check can run.
    #[cfg(target_os = "linux")]
    #[test]
    fn authority_refuses_lossy_workspace_names() {
        use std::os::unix::ffi::OsStrExt;
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join(std::ffi::OsStr::from_bytes(b"\xff"));
        std::fs::create_dir(&path).unwrap();
        let caveats = Caveats::top();
        let launch = HarnessLaunch {
            workspace: &path,
            caveats: &caveats,
            frame_dir: None,
            resume_from: None,
            hermetic: false,
        };
        assert!(launch.authority().is_err());
    }

    /// Grounds the session's mocked membership check in a real restart directory.
    #[test]
    fn restore_rejects_changed_authority_settings_and_hermetic_resume() {
        let workspace = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let caveats = crate::confined_exec::build_tool_caveats(workspace.path());
        let mut config = SmartHarnessConfig {
            frame_dir: Some(private.path().join("frame")),
            ..Default::default()
        };
        let launch = HarnessLaunch {
            workspace: workspace.path(),
            caveats: &caveats,
            frame_dir: None,
            resume_from: None,
            hermetic: false,
        };
        let auxiliary = serde_json::json!({"placement":"cpu", "model":"fixture"});
        let session = config.open_session(&launch, auxiliary.clone()).unwrap();
        let context = session.config().authority_context.as_ref().unwrap();
        assert_eq!(
            context["workspace"],
            workspace.path().canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(context["caveats"], serde_json::to_value(&caveats).unwrap());
        let head = session.head().to_string();
        drop(session);
        let resumed = HarnessLaunch {
            resume_from: Some(&head),
            ..launch
        };
        assert!(config.open_session(&resumed, auxiliary.clone()).is_ok());
        assert!(config
            .open_session(
                &resumed,
                serde_json::json!({"placement":"cpu", "model":"changed"})
            )
            .is_err());
        assert!(config
            .open_session(
                &HarnessLaunch {
                    hermetic: true,
                    ..resumed
                },
                auxiliary.clone()
            )
            .is_err());
        config.max_dereferences += 1;
        assert!(config.open_session(&resumed, auxiliary.clone()).is_err());
        let store = private.path().join("frame");
        assert!(config
            .open_session(
                &HarnessLaunch {
                    workspace: other.path(),
                    frame_dir: Some(&store),
                    ..resumed
                },
                auxiliary
            )
            .is_err());
    }

    /// Grounds a TUI cold restart in real locator and journal files.
    #[test]
    fn conversation_locator_restores_only_accounted_history() {
        let workspace = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let caveats = crate::confined_exec::build_tool_caveats(workspace.path());
        let config = SmartHarnessConfig {
            frame_dir: Some(private.path().join("frame")),
            ..Default::default()
        };
        let launch = HarnessLaunch {
            workspace: workspace.path(),
            caveats: &caveats,
            frame_dir: None,
            resume_from: None,
            hermetic: false,
        };
        let auxiliary = serde_json::json!({"model":"fixture"});
        assert!(config
            .open_conversation(&launch, "legacy", true, auxiliary.clone())
            .is_err());
        let first = config
            .open_conversation(&launch, "one", false, auxiliary.clone())
            .unwrap();
        let run = first.run_id();
        drop(first);
        let restored = config
            .open_conversation(&launch, "one", true, auxiliary.clone())
            .unwrap();
        assert_eq!(restored.run_id(), run);
        let other = config
            .open_conversation(&launch, "two", false, auxiliary.clone())
            .unwrap();
        assert_ne!(other.run_id(), run);
        let locator =
            SmartHarnessConfig::conversation_locator(&config.directory(&launch).unwrap(), "one");
        std::fs::write(locator, "not-a-cid").unwrap();
        assert!(config
            .open_conversation(&launch, "one", true, auxiliary)
            .is_err());
    }
}
