//! Review an immutable session snapshot, then use the signed encrypted store.

use super::*;
use newt_core::durable_grants::GrantSet;
use std::path::Path;

fn store_exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("cannot inspect permanent permissions: {error}")),
    }
}

pub(crate) fn load_session_grants(
    config: Option<&Path>,
    key: Option<&Path>,
    workspace: &Path,
) -> Result<GrantSet, String> {
    let Some(config) = config else {
        return Ok(GrantSet::new());
    };
    let path = newt_core::durable_grants::store_path(config);
    if !store_exists(&path)? {
        return Ok(GrantSet::new());
    }
    let root = newt_identity::load_user_key(
        key.ok_or("permanent permissions signing key is unavailable")?,
    )
    .map_err(|error| format!("cannot load permanent permissions signing key: {error}"))?;
    let identity = newt_core::secrets::load_identity().map_err(|error| error.to_string())?
        .ok_or("permanent permissions encryption identity is missing; restore it before loading approvals")?;
    newt_core::durable_grants::load(&path, workspace, &root.public(), &identity)
        .map(|verified| verified.grants().clone())
        .map_err(|error| error.to_string())
}

pub(super) fn store_session_grants(
    config: Option<&Path>,
    key: Option<&Path>,
    workspace: &Path,
    snapshot: &GrantSet,
) -> Result<GrantSet, String> {
    let config =
        config.ok_or("select a trusted configuration with --config before saving permissions")?;
    let path = newt_core::durable_grants::store_path(config);
    let root = newt_identity::load_user_key(
        key.ok_or("permanent permissions signing key is unavailable")?,
    )
    .map_err(|error| error.to_string())?;
    let identity = if store_exists(&path)? {
        newt_core::secrets::load_identity().map_err(|error| error.to_string())?
            .ok_or("existing permanent permissions require their original encryption identity; no key was generated")?
    } else {
        newt_core::secrets::load_or_generate_identity().map_err(|error| error.to_string())?
    };
    newt_core::durable_grants::merge(&path, workspace, snapshot, &root, &identity)
        .map(|verified| verified.grants().clone())
        .map_err(|error| error.to_string())
}

pub(super) fn review_session_grants(
    state: &mut PermissionPromptState,
    preset: Option<&newt_core::Caveats>,
    ceiling: Option<&newt_core::Caveats>,
    ask: crate::SlashAsk<'_>,
    save: impl FnOnce(&GrantSet) -> Result<GrantSet, String>,
) -> Result<usize, String> {
    let snapshot = state.session_grants.clone();
    if snapshot.is_empty() {
        return Ok(0);
    }
    let danger = production_danger_table();
    for (kind, target) in &snapshot {
        if state.recall_conflicts_with_denial(*kind, target)
            || danger.classify(*kind, target) == danger::DangerTier::High
            || preset.is_some_and(|floor| {
                *kind != newt_core::DenialKind::RemoteTool && !ceiling_permits(floor, *kind, target)
            })
            || ceiling.is_some_and(|floor| !ceiling_permits(floor, *kind, target))
        {
            return Err(format!("cannot make the snapshot permanent: {}: {target} conflicts with a denial, is high-danger, or exceeds the current authority; nothing was saved", kind.as_str()));
        }
    }
    let list = snapshot
        .iter()
        .map(|(kind, target)| format!("{}: {target}", kind.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    let interaction = SurfaceInteraction::blocking(newt_core::interaction_form::confirm(
        format!("Are you sure? Make all {} session allows permanent for this workspace?\n\n{list}\n\nOnly these session allows will be saved, signed and encrypted. Allow-once, CLI and config grants are excluded.", snapshot.len()),
        "Cancel is the default. PgUp/PgDn scroll the full list. Revocation takes effect after restart.",
        "Make permanent", "Cancel (default)",
    )).with_default_option(OptionId::new(newt_core::interaction_form::NO).expect("constant option id"));
    let confirmed = match ask(&interaction) {
        HumanQuestionOutcome::Answer(answer) => {
            newt_core::interaction_form::resolve(&interaction.definition, &answer)
                .is_some_and(|id| id.as_str() == newt_core::interaction_form::YES)
        }
        _ => false,
    };
    if !confirmed {
        return Ok(0);
    }
    state.durable_grants = save(&snapshot)?;
    Ok(snapshot.len())
}

pub(crate) struct PermissionWorkflow<'a> {
    pub(crate) state: &'a mut PermissionPromptState,
    pub(crate) config_path: Option<&'a Path>,
    pub(crate) key_path: Option<&'a Path>,
    pub(crate) workspace: &'a Path,
    pub(crate) preset: Option<&'a newt_core::Caveats>,
    pub(crate) ceiling: Option<&'a newt_core::Caveats>,
}

impl PermissionWorkflow<'_> {
    pub(crate) fn promote(&mut self, ask: crate::SlashAsk<'_>) -> Result<String, String> {
        if self.state.session_grants.is_empty() {
            return Ok("No session allows to save.".into());
        }
        let count =
            review_session_grants(self.state, self.preset, self.ceiling, ask, |snapshot| {
                store_session_grants(self.config_path, self.key_path, self.workspace, snapshot)
            })?;
        Ok(if count == 0 {
            "Permanent permission save cancelled; no grants changed.".into()
        } else {
            format!("Saved {count} session allows permanently, signed and encrypted for this workspace.")
        })
    }

    #[cfg(feature = "rich-tui")]
    pub(crate) fn run_panel(
        &mut self,
        ask: crate::SlashAsk<'_>,
        mut open: impl FnMut(u16) -> Option<crate::session_worker::PanelWindow>,
        rows: impl Fn(&PermissionPromptState) -> Vec<String>,
    ) -> Result<Vec<String>, String> {
        use crate::permissions_panel::{self, PermissionIntent, PermissionsPanel};
        let mut panel = PermissionsPanel::new(
            rows(self.state),
            self.state.terminal_default(),
            self.state.session_allow_count(),
        );
        let mut messages: Vec<String> = Vec::new();
        loop {
            let mut status = rows(self.state);
            if let Some(message) = messages.last() {
                status.insert(0, message.clone());
            }
            panel.refresh(
                status,
                self.state.terminal_default(),
                self.state.session_allow_count(),
            );
            let action = permissions_panel::run(&mut panel, open(permissions_panel::height()))
                .map_err(|error| error.to_string())?;
            let result = match action {
                None => return Ok(messages),
                Some(PermissionIntent::SaveSessionAllows) => self.promote(ask),
                Some(PermissionIntent::SetDefault(action)) => self
                    .config_path
                    .ok_or_else(|| {
                        "select a trusted configuration with --config before changing defaults"
                            .to_string()
                    })
                    .and_then(|path| {
                        newt_core::Config::set_permission_prompt_default(path, action)
                            .map_err(|error| error.to_string())
                    })
                    .map(|()| {
                        self.state.prompt_default = Some(action);
                        format!("Permission default saved: {}.", action.as_str())
                    }),
            };
            messages.push(result.unwrap_or_else(|error| format!("Permission settings: {error}")));
        }
    }
}

// Model: GPT-6 | Harness: Codex | Operator: Shawn Hartsock | Time: 14:36 EDT | Date: 2026-09-16
