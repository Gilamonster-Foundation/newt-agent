//! **The setup transaction** — how a wizard decision becomes durable bytes.
//!
//! Split out of `setup.rs` by D1b-0 (#1892). Nothing here talks to a human:
//! there is no `Console`, no prompt, and no answer. It takes a decision that
//! has already been made and commits it — the lock, the staging, the
//! crash-safe ordering, the no-clobber rules, and the drop-in directory
//! reading that the interactive wizard and the rich-TUI backend panel both
//! depend on.
//!
//! That second caller is why this is a module and not a section. The panel
//! (#1667) reuses this machinery wholesale; it was never wizard code that
//! grew a transaction, it was a transaction engine parked in the wizard's
//! file. Separating them means the D1 interaction migration cannot reach the
//! crash-safety invariants by accident — the failure mode here is a corrupted
//! config, not a badly-worded prompt.
//!
//! ## The invariant the ordering exists for
//!
//! A backend is published only after its immutable credential exists. See
//! [`super::run_setup_commit`] for the choke point that enforces the step
//! order, and [`SetupCommitGuard`] for what unwinds when a later step fails.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

// Only `persist_detected_setup` names it, and that helper is `#[cfg(test)]`
// — it is the detected-flow entry the tests drive directly.
#[cfg(test)]
use newt_core::backend_probe::EndpointProbeResult;
use newt_core::{BackendConfig, BackendKind, Config, OpenAiApi};

// Explicit, not `use super::*`. The whole point of the split is that the
// coupling to the wizard is legible: this module needs exactly one of its
// types and two of its pure helpers, and a glob would hide that from the
// next person deciding whether the boundary still holds.
use super::{backend_from_verified_probe, backend_name, VerifiedTargetHit};

pub(super) fn persist_verified_setup(
    config_path: &Path,
    probes: &[VerifiedTargetHit],
    token_env: Option<&str>,
    token_file: Option<&Path>,
) -> anyhow::Result<Vec<PathBuf>> {
    if probes.is_empty() {
        anyhow::bail!("cannot persist an empty endpoint probe result");
    }
    if let Some(parent) = config_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let setup_lock = acquire_setup_lock(config_path)?;
    let old_config = read_setup_config(setup_lock.destination.as_path())?;
    let backend_dir = config_path.with_file_name("backends");
    let existing = read_existing_setup_backends(&backend_dir)?;
    let mut used_names: HashSet<String> = existing.iter().map(|item| item.name.clone()).collect();
    let token_file_ref = token_file.and_then(Path::to_str);
    let mut planned = Vec::with_capacity(probes.len());

    for verified in probes {
        let probe = &verified.probe;
        let normalized = normalize_setup_endpoint(&probe.endpoint)?;
        let base_name = backend_name(&probe.endpoint)?;
        if let Some(found) = existing
            .iter()
            .filter(|item| {
                !item.probe_owned
                    && item.endpoint.as_deref() == Some(normalized.as_str())
                    && item.matches_token_reference(token_env, token_file_ref)
                    && item.matches_probe(verified)
            })
            .min_by_key(|item| (item.name != base_name, item.name.as_str()))
        {
            planned.push(PlannedSetupBackend {
                name: found.name.clone(),
                endpoint: normalized,
                path: found.path.clone(),
                body: None,
                replace: false,
            });
            continue;
        }
        if let Some(found) = planned.iter().find(|item: &&PlannedSetupBackend| {
            item.endpoint == normalized
                && item.matches_generated_reference(token_env, token_file_ref)
        }) {
            planned.push(PlannedSetupBackend {
                name: found.name.clone(),
                endpoint: normalized,
                path: found.path.clone(),
                body: None,
                replace: false,
            });
            continue;
        }

        let name = allocate_backend_name(&base_name, &mut used_names);
        let mut backend = backend_from_verified_probe(verified, token_env, token_file)?;
        backend.name.clone_from(&name);
        let body = toml::to_string(&backend)?;
        planned.push(PlannedSetupBackend {
            path: backend_dir.join(format!("{name}.toml")),
            name,
            endpoint: normalized,
            body: Some(body.into_bytes()),
            replace: false,
        });
    }

    let default_name = &planned[0].name;
    let updated_config = Config::with_default_backend(&old_config, default_name)?;
    // The wizard only ever CREATES drop-ins (no `replace`), so the after-commit
    // warning sink stays empty here; the backend panel's edit path is the one
    // that can fill it.
    let mut warnings = Vec::new();
    commit_setup_plan(
        config_path,
        &setup_lock.destination,
        &old_config,
        &updated_config,
        &planned,
        &mut warnings,
    )
}

/// Test-only compatibility wrapper for the lower-level persistence regressions
/// that construct already-detected probes without running generation.
#[cfg(test)]
pub(super) fn persist_detected_setup(
    config_path: &Path,
    probes: &[EndpointProbeResult],
    token_env: Option<&str>,
    token_file: Option<&Path>,
) -> anyhow::Result<Vec<PathBuf>> {
    let verified: Vec<VerifiedTargetHit> = probes
        .iter()
        .cloned()
        .map(|probe| VerifiedTargetHit { probe, api: None })
        .collect();
    persist_verified_setup(config_path, &verified, token_env, token_file)
}

#[derive(Debug)]
pub(super) struct ExistingSetupBackend {
    pub(super) name: String,
    pub(super) path: PathBuf,
    pub(super) endpoint: Option<String>,
    pub(super) api_key_env: Option<String>,
    pub(super) api_key_file: Option<String>,
    pub(super) kind: Option<BackendKind>,
    pub(super) api: Option<newt_core::config::OpenAiApi>,
    pub(super) serving: Option<newt_core::Serving>,
    pub(super) model: Option<String>,
    pub(super) generated_by_setup: bool,
    /// The file is a machine-owned probe overlay (`record = "probe_v1"` or
    /// the unambiguous legacy cache) — it RESERVES its filename but is
    /// never REUSABLE as a setup backend definition: adopting a probe cache
    /// as the definition is how the real backend silently vanished (#1819
    /// ownership taxonomy).
    pub(super) probe_owned: bool,
}

impl ExistingSetupBackend {
    fn matches_token_reference(&self, env: Option<&str>, file: Option<&str>) -> bool {
        self.api_key_env.as_deref() == env && self.api_key_file.as_deref() == file
    }

    pub(super) fn matches_probe(&self, verified: &VerifiedTargetHit) -> bool {
        let probe = &verified.probe;
        let kind_matches = self.kind == Some(probe.kind);
        let api_matches = self.api.is_none()
            || self.api == verified.api
            // Setup deliberately persists Chat acceptance without a surface
            // pin so runtime capability probing may select Responses.  Such a
            // writeback is the same verified backend, not a name collision.
            || (self.api == Some(OpenAiApi::Responses)
                && verified.api == Some(OpenAiApi::ChatCompletions));
        let serving_matches = self.serving.is_none_or(|serving| serving == probe.serving);
        let model_matches = self
            .model
            .as_ref()
            .is_none_or(|model| probe.models.contains(model));
        kind_matches
            && api_matches
            && serving_matches
            && model_matches
            && (!self.generated_by_setup || (self.serving.is_some() && self.model.is_some()))
    }
}

#[derive(Debug)]
pub(super) struct PlannedSetupBackend {
    pub(super) name: String,
    pub(super) endpoint: String,
    pub(super) path: PathBuf,
    pub(super) body: Option<Vec<u8>>,
    /// `true` = durably REPLACE an existing drop-in (the backend panel's edit,
    /// #1667); `false` = create-only, refusing to clobber a file that appeared
    /// concurrently (the setup wizard's add semantics, #1660).
    pub(super) replace: bool,
}

impl PlannedSetupBackend {
    fn matches_generated_reference(&self, env: Option<&str>, file: Option<&str>) -> bool {
        let Some(body) = self.body.as_deref() else {
            return true;
        };
        toml::from_slice::<BackendConfig>(body).is_ok_and(|backend| {
            backend.api_key_env.as_deref() == env && backend.api_key_file.as_deref() == file
        })
    }
}

pub(super) fn read_setup_config(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn normalize_setup_endpoint(endpoint: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|error| anyhow::anyhow!("invalid backend endpoint `{endpoint}`: {error}"))?;
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn read_existing_setup_backends(dir: &Path) -> anyhow::Result<Vec<ExistingSetupBackend>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();

    let mut backends = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(name) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let body = std::fs::read_to_string(&path).ok();
        let probe_owned = body.as_deref().is_some_and(|text| {
            matches!(
                newt_core::classify_backend_dropin(text),
                Ok(newt_core::DropinOwnership::Probe)
            )
        });
        let parsed = body.and_then(|body| toml::from_str::<BackendConfig>(&body).ok());
        backends.push(ExistingSetupBackend {
            name,
            endpoint: parsed
                .as_ref()
                .and_then(|backend| normalize_setup_endpoint(&backend.endpoint).ok()),
            api_key_env: parsed
                .as_ref()
                .and_then(|backend| backend.api_key_env.clone()),
            api_key_file: parsed
                .as_ref()
                .and_then(|backend| backend.api_key_file.clone()),
            kind: parsed.as_ref().and_then(|backend| backend.kind),
            api: parsed.as_ref().and_then(|backend| backend.api),
            serving: parsed.as_ref().and_then(|backend| backend.serving),
            model: parsed.as_ref().and_then(|backend| backend.model.clone()),
            generated_by_setup: parsed.as_ref().is_some_and(|backend| {
                backend
                    .provenance
                    .as_ref()
                    .and_then(|provenance| provenance.source.as_deref())
                    .is_some_and(|source| source.starts_with("newt setup v"))
            }),
            probe_owned,
            path,
        });
    }
    Ok(backends)
}

fn allocate_backend_name(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    for suffix in 2_u32.. {
        let candidate = format!("{base}-{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("u32 backend-name suffix space exhausted")
}

#[derive(Debug)]
pub(super) struct SetupLock {
    pub(super) destination: newt_core::atomic_fs::ResolvedPath,
    _guard: newt_core::atomic_fs::LockGuard,
}

pub(super) fn acquire_setup_lock(config_path: &Path) -> anyhow::Result<SetupLock> {
    let destination = setup_config_destination(config_path)?;
    let guard = newt_core::atomic_fs::acquire_lock(&destination.lock_path())?;
    Ok(SetupLock {
        destination,
        _guard: guard,
    })
}

pub(super) fn setup_config_destination(
    path: &Path,
) -> anyhow::Result<newt_core::atomic_fs::ResolvedPath> {
    newt_core::atomic_fs::ResolvedPath::resolve(path)
}

pub(super) fn setup_file_permissions(path: &Path) -> anyhow::Result<Option<std::fs::Permissions>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.permissions())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn stage_setup_file(
    destination: &newt_core::atomic_fs::ResolvedPath,
    body: &[u8],
    permissions: Option<&std::fs::Permissions>,
) -> anyhow::Result<PathBuf> {
    destination.stage_with_permissions(body, permissions, true)
}

#[derive(Default)]
pub(super) struct SetupCommitGuard {
    temporary: Vec<PathBuf>,
    pub(super) created: Vec<PathBuf>,
    committed: bool,
}

impl SetupCommitGuard {
    pub(super) fn stage(
        &mut self,
        destination: &newt_core::atomic_fs::ResolvedPath,
        body: &[u8],
        permissions: Option<&std::fs::Permissions>,
    ) -> anyhow::Result<PathBuf> {
        let path = stage_setup_file(destination, body, permissions)?;
        self.temporary.push(path.clone());
        Ok(path)
    }

    pub(super) fn finish(mut self) -> Vec<PathBuf> {
        self.committed = true;
        std::mem::take(&mut self.created)
    }

    pub(super) fn retain_created(&mut self) {
        self.committed = true;
    }
}

impl Drop for SetupCommitGuard {
    fn drop(&mut self) {
        for path in &self.temporary {
            let _ = std::fs::remove_file(path);
        }
        if !self.committed {
            for path in &self.created {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn commit_backend_no_clobber(
    temp: &Path,
    destination: &newt_core::atomic_fs::ResolvedPath,
) -> anyhow::Result<()> {
    match destination.durable_create(temp) {
        Ok(()) => Ok(()),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::AlreadyExists) =>
        {
            Err(anyhow::anyhow!(
                "backend {} appeared while setup was running; retry setup",
                destination.as_path().display()
            ))
        }
        Err(error) => Err(error.context(format!(
            "could not durably create backend {} without overwriting it",
            destination.as_path().display()
        ))),
    }
}

/// Classify a durable-replace failure on a drop-in the plan REPLACES (#1667
/// review §10). An **after-commit** failure means the rename already succeeded
/// — the new bytes ARE the file, only the parent-directory fsync failed — so it
/// is a durability WARNING, never a "save failed" that would leave the caller
/// reporting a write that is visibly on disk as lost. A before-commit failure is
/// a real failure and propagates.
pub(super) fn replace_warning(
    result: Result<(), newt_core::atomic_fs::DurableReplaceError>,
) -> Result<Option<String>, newt_core::atomic_fs::DurableReplaceError> {
    match result {
        Ok(()) => Ok(None),
        Err(error) if error.committed() => Ok(Some(error.to_string())),
        Err(error) => Err(error),
    }
}

/// `warnings` collects non-fatal after-commit durability problems (see
/// [`replace_warning`]): the bytes are on disk, but a sync step failed. The
/// caller reports them alongside a SUCCESSFUL write.
pub(super) fn commit_setup_plan(
    config_path: &Path,
    config_destination: &newt_core::atomic_fs::ResolvedPath,
    old_config: &str,
    updated_config: &str,
    planned: &[PlannedSetupBackend],
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<PathBuf>> {
    commit_setup_plan_with(
        config_path,
        config_destination,
        old_config,
        updated_config,
        planned,
        warnings,
        |staged, destination| destination.durable_replace(staged),
    )
}

pub(super) fn commit_setup_plan_with(
    config_path: &Path,
    config_destination: &newt_core::atomic_fs::ResolvedPath,
    old_config: &str,
    updated_config: &str,
    planned: &[PlannedSetupBackend],
    warnings: &mut Vec<String>,
    commit_config: impl FnOnce(
        &Path,
        &newt_core::atomic_fs::ResolvedPath,
    ) -> Result<(), newt_core::atomic_fs::DurableReplaceError>,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut guard = SetupCommitGuard::default();
    let mut staged_backends = Vec::new();
    for backend in planned {
        if let Some(body) = backend.body.as_deref() {
            let destination = setup_config_destination(&backend.path)?;
            staged_backends.push((
                guard.stage(&destination, body, None)?,
                destination,
                backend.replace,
            ));
        }
    }
    let config_permissions = setup_file_permissions(config_destination.as_path())?;
    let config_stage = if updated_config != old_config {
        Some(guard.stage(
            config_destination,
            updated_config.as_bytes(),
            config_permissions.as_ref(),
        )?)
    } else {
        None
    };
    let filename = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let backup_path = config_path.with_file_name(format!("{filename}.bak"));
    let backup_destination = setup_config_destination(&backup_path)?;
    let backup_stage = if !old_config.is_empty() && updated_config != old_config {
        Some(guard.stage(
            &backup_destination,
            old_config.as_bytes(),
            config_permissions.as_ref(),
        )?)
    } else {
        None
    };
    let previous_backup_stage = if backup_stage.is_some() {
        match std::fs::read(backup_destination.as_path()) {
            Ok(body) => {
                let permissions = setup_file_permissions(backup_destination.as_path())?;
                Some(guard.stage(&backup_destination, &body, permissions.as_ref())?)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };

    if config_stage.is_some() && read_setup_config(config_destination.as_path())? != old_config {
        anyhow::bail!(
            "{} changed while setup was preparing its update; retry setup",
            config_path.display()
        );
    }

    for (temp, destination, replace) in &staged_backends {
        if *replace {
            // The backend panel's EDIT (#1667): durably replace the existing
            // drop-in. Deliberately NOT registered for rollback — the original
            // bytes are gone once replaced, and the new content is itself a
            // valid drop-in, so a later failure must not delete it. An
            // after-commit sync failure is a warning, not a failure: the file
            // on disk IS the edit (review §10).
            warnings.extend(replace_warning(destination.durable_replace(temp))?);
        } else {
            commit_backend_no_clobber(temp, destination)?;
            guard.created.push(destination.as_path().to_path_buf());
        }
    }
    if config_stage.is_some() && read_setup_config(config_destination.as_path())? != old_config {
        anyhow::bail!(
            "{} changed while setup was preparing its update; retry setup",
            config_path.display()
        );
    }
    if let Some(temp) = backup_stage.as_ref() {
        backup_destination.durable_replace(temp)?;
    }
    if let Some(temp) = config_stage.as_ref() {
        if let Err(config_error) = commit_config(temp, config_destination) {
            if config_error.committed() {
                // The new config may already select these drop-ins. Keep every
                // prerequisite and its old-config backup even though the
                // replacement's parent-directory sync failed.
                guard.retain_created();
                return Err(config_error.into());
            }
            let restore_result = if let Some(previous) = previous_backup_stage.as_ref() {
                backup_destination
                    .durable_replace(previous)
                    .map_err(anyhow::Error::from)
            } else {
                std::fs::remove_file(backup_destination.as_path())
                    .or_else(|error| {
                        if error.kind() == io::ErrorKind::NotFound {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })
                    .map_err(anyhow::Error::from)
            };
            if let Err(restore_error) = restore_result {
                anyhow::bail!(
                    "could not update {} ({config_error}); also could not restore its previous \
                     backup ({restore_error})",
                    config_path.display()
                );
            }
            return Err(config_error.into());
        }
    }
    Ok(guard.finish())
}

// ---------------------------------------------------------------------------
// Backend panel persistence (#1667) — REUSES the wizard's crash-safe machinery
// (acquire_setup_lock → plan → commit_setup_plan, #1660); the panel never gets
// a second write path.
// ---------------------------------------------------------------------------

/// The result of a panel save: the written path plus any non-fatal durability
/// warnings (the bytes ARE on disk — see [`replace_warning`], review §10).
#[cfg(feature = "rich-tui")]
#[derive(Debug)]
pub(crate) struct PanelSave {
    pub path: PathBuf,
    pub warnings: Vec<String>,
}

/// A valid backend file-stem (the panel's name grammar, shared by the write and
/// delete paths so a traversal shape can never reach the filesystem).
#[cfg(feature = "rich-tui")]
pub(super) fn valid_panel_backend_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The TOML keys the panel form manages, paired with the dirty flags that say
/// whether the operator actually touched each one (three Cs: the mapping is
/// data, so a new form field is one row here). Only DIRTY keys are overlaid.
#[cfg(feature = "rich-tui")]
pub(super) fn dirty_dropin_edits(
    edit: &crate::backend_panel::BackendEdit,
) -> Vec<(&'static str, Option<String>)> {
    let dirty = edit.dirty;
    [
        (
            "kind",
            dirty.kind,
            edit.kind.map(|kind| kind.label().to_string()),
        ),
        (
            "endpoint",
            dirty.endpoint,
            Some(edit.endpoint.trim().to_string()).filter(|url| !url.is_empty()),
        ),
        ("model", dirty.model, edit.model.clone()),
        ("api_key_env", dirty.api_key_env, edit.api_key_env.clone()),
        (
            "api_key_file",
            dirty.api_key_file,
            edit.api_key_file.clone(),
        ),
    ]
    .into_iter()
    .filter(|(_, dirty, _)| *dirty)
    .map(|(key, _, value)| (key, value))
    .collect()
}

/// Write the panel's add/edit form as the drop-in `backends/<name>.toml`, under
/// the setup lock, via the staged [`commit_setup_plan`] commit.
///
/// An EDIT (`edit.replace`) **re-reads the file at SAVE time** and overlays only
/// the keys the operator actually changed ([`dirty_dropin_edits`]), through
/// `BackendConfig::with_dropin_edits` — a `toml_edit` overlay, so comments, key
/// order, and keys `BackendConfig` does not model survive (review §6 and §8;
/// the old serde round-trip silently destroyed both, and re-applying the whole
/// panel-open prefill silently reverted a concurrent writer's untouched fields).
/// A field the form never touched — the `kind` dial included — is left
/// byte-for-byte alone, which is the persistence half of the review §1 fix (the
/// dial half is `KIND_LADDER` + `begin_edit`'s fail-closed refusal).
///
/// **Residual race:** the read → stage → replace window is guarded by the setup
/// lock, so any *newt* writer serializes behind it; a foreign editor writing the
/// drop-in inside that window still loses its change to the overlay. Narrowing
/// that further needs a content hash / O_EXCL exchange the drop-in format does
/// not carry yet.
///
/// An ADD refuses to clobber an existing file. `config.toml` is never rewritten
/// here (the default pointer is the chooser's job, not the editor's).
#[cfg(feature = "rich-tui")]
pub(crate) fn persist_panel_backend(
    config_path: &Path,
    edit: &crate::backend_panel::BackendEdit,
) -> anyhow::Result<PanelSave> {
    anyhow::ensure!(!edit.name.trim().is_empty(), "backend needs a name");
    anyhow::ensure!(
        valid_panel_backend_name(edit.name.trim()),
        "invalid backend name '{}'",
        edit.name
    );
    if let Some(parent) = config_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let setup_lock = acquire_setup_lock(config_path)?;
    let old_config = read_setup_config(setup_lock.destination.as_path())?;
    let path = config_path
        .with_file_name("backends")
        .join(format!("{}.toml", edit.name));
    let body = if edit.replace {
        let existing = std::fs::read_to_string(&path)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
        BackendConfig::with_dropin_edits(&existing, &dirty_dropin_edits(edit))
            .map_err(|error| anyhow::anyhow!("edit {}: {error}", path.display()))?
    } else if path.exists() {
        anyhow::bail!("backend '{}' already exists — edit it instead", edit.name);
    } else {
        let backend = BackendConfig {
            name: edit.name.clone(),
            endpoint: edit.endpoint.clone(),
            kind: edit.kind,
            model: edit.model.clone(),
            api_key_env: edit.api_key_env.clone(),
            api_key_file: edit.api_key_file.clone(),
            ..BackendConfig::default()
        };
        toml::to_string(&backend)?
    };
    // Parse what we are about to write: the drop-in must still be a valid
    // backend after the overlay, and the plan wants the normalized endpoint.
    let parsed: BackendConfig = toml::from_str(&body)
        .map_err(|error| anyhow::anyhow!("{} would become invalid: {error}", path.display()))?;
    let endpoint = if parsed.endpoint.trim().is_empty() {
        // A `kind = "embedded"` drop-in has a model_path, not a URL.
        String::new()
    } else {
        normalize_setup_endpoint(&parsed.endpoint)?
    };
    let planned = [PlannedSetupBackend {
        name: edit.name.clone(),
        endpoint,
        path: path.clone(),
        body: Some(body.into_bytes()),
        replace: edit.replace,
    }];
    // old == updated: the plan stages/commits ONLY the drop-in; config.toml is
    // left byte-for-byte alone (no backup dance, no default_backend rewrite).
    let mut warnings = Vec::new();
    commit_setup_plan(
        config_path,
        &setup_lock.destination,
        &old_config,
        &old_config,
        &planned,
        &mut warnings,
    )?;
    Ok(PanelSave { path, warnings })
}

/// The `default_backend` a config.toml TEXT names, if any.
#[cfg(feature = "rich-tui")]
pub(super) fn default_backend_in(config_text: &str) -> Option<String> {
    toml::from_str::<toml::Value>(config_text)
        .ok()?
        .get("default_backend")?
        .as_str()
        .map(str::to_string)
}

/// The `[[backends]]` names declared INLINE in a config.toml text.
#[cfg(feature = "rich-tui")]
pub(super) fn inline_backend_names_in(config_text: &str) -> Vec<String> {
    toml::from_str::<toml::Value>(config_text)
        .ok()
        .as_ref()
        .and_then(|value| value.get("backends"))
        .and_then(toml::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("name").and_then(toml::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The `[[backends]]` names declared inline in the config.toml at `config_path`
/// — a drop-in that shares one of these names does NOT fully shadow it: the
/// merge re-inherits `api_key_*` / `tiers` the drop-in omits
/// (`Config::merge_backends_from_dir`), so clearing an auth field in the panel
/// can silently come back (review §4). The panel marks those rows and says so
/// in the save note.
#[cfg(feature = "rich-tui")]
pub(crate) fn inline_backend_names(config_path: &Path) -> Vec<String> {
    std::fs::read_to_string(config_path)
        .map(|text| inline_backend_names_in(&text))
        .unwrap_or_default()
}

/// Delete the drop-in `backends/<name>.toml` under the setup lock, durably
/// syncing the parent directory — the panel's `:d <name>` (#1667). Returns the
/// operator-visible notes the caller must report (a `default_backend` repoint, a
/// non-durable delete).
///
/// **The durable default pointer is part of this transaction** (review §2/§7/§11):
/// removing the backend `config.toml`'s `default_backend` names would leave a
/// dangling pointer, which `Config::select_backend` treats as a hard
/// `UnknownNamed` operator error (the ACP worker `bail!`s on it, and no
/// settings.toml mask exists there). So when the removed name IS the default,
/// this refuses unless the same transaction hands over `repoint_default_to` —
/// the backend the caller just applied — and then repoints `default_backend`
/// (comment-preserving, via `Config::with_default_backend`) BEFORE unlinking, so
/// a failed delete can never leave the pointer dangling.
///
/// The caller (the panel) additionally refuses to remove the ACTIVE backend
/// unless a different named selection is applied in the same transaction; this
/// function guards the filesystem invariants (a sane file-stem name, the file
/// existing) and the durable pointer.
#[cfg(feature = "rich-tui")]
pub(crate) fn remove_panel_backend(
    config_path: &Path,
    name: &str,
    repoint_default_to: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        valid_panel_backend_name(name),
        "invalid backend name '{name}'"
    );
    if let Some(new) = repoint_default_to {
        anyhow::ensure!(
            valid_panel_backend_name(new),
            "invalid backend name '{new}'"
        );
        anyhow::ensure!(
            new != name,
            "cannot repoint default_backend at '{new}' while removing it"
        );
    }
    let setup_lock = acquire_setup_lock(config_path)?;
    let old_config = read_setup_config(setup_lock.destination.as_path())?;
    let backends_dir = config_path.with_file_name("backends");
    let path = backends_dir.join(format!("{name}.toml"));
    anyhow::ensure!(
        path.exists(),
        "no backend drop-in named '{name}' ({})",
        path.display()
    );
    let mut notes = Vec::new();
    if default_backend_in(&old_config).as_deref() == Some(name) {
        let Some(new) = repoint_default_to else {
            anyhow::bail!(
                "'{name}' is config.toml's default_backend — removing it would leave a dangling \
                 default (a hard 'unknown backend' error for `newt solve` and the ACP worker); \
                 dial another named backend first so the switch and the removal happen in one \
                 transaction"
            );
        };
        anyhow::ensure!(
            backends_dir.join(format!("{new}.toml")).exists()
                || inline_backend_names_in(&old_config)
                    .iter()
                    .any(|n| n == new),
            "cannot repoint default_backend at unknown backend '{new}'"
        );
        let updated_config = Config::with_default_backend(&old_config, new)?;
        let mut warnings = Vec::new();
        commit_setup_plan(
            config_path,
            &setup_lock.destination,
            &old_config,
            &updated_config,
            &[],
            &mut warnings,
        )?;
        notes.extend(warnings);
        notes.push(format!(
            "default_backend now points at '{new}' ({})",
            config_path.display()
        ));
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {
            // Surface a non-durable delete the way the write side surfaces a
            // post-rename sync failure (review §9) — the unlink HAPPENED, so
            // this is a warning on a success, not an error.
            if let Err(error) = newt_core::atomic_fs::sync_parent(&path) {
                notes.push(format!(
                    "removed {}, but could not durably sync its parent directory: {error:#}",
                    path.display()
                ));
            }
            Ok(notes)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(anyhow::anyhow!(
            "no backend drop-in named '{name}' ({})",
            path.display()
        )),
        Err(error) => Err(error.into()),
    }
}

/// The names of the per-file backend drop-ins next to `config_path` — which
/// chooser entries the panel may edit/remove (inline `[[backends]]` in
/// config.toml stay read-only there). Reuses the wizard's directory reader.
#[cfg(feature = "rich-tui")]
pub(crate) fn panel_backend_file_names(config_path: &Path) -> Vec<String> {
    read_existing_setup_backends(&config_path.with_file_name("backends"))
        .map(|found| found.into_iter().map(|item| item.name).collect())
        .unwrap_or_default()
}
