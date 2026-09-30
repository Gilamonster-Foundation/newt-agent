//! Operator-only workspace launch profiles. Saving never changes this session.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use newt_core::durable_grants::{self, ReviewedWorkspaceEdit, VerifiedSnapshot, WorkspaceProfile};
use newt_core::interaction_form::{self, NO, YES};
use newt_core::interaction_surface::SurfaceInteraction;
use newt_core::{Caveats, HumanQuestionOutcome, PermissionPreset, Scope, ToolPermissions};
use newt_interaction::OptionId;

pub(crate) struct WorkspaceSettingsContext<'a> {
    pub workspace: &'a Path,
    pub current_dir: &'a Path,
    pub active: &'a Caveats,
    pub permissions: &'a ToolPermissions,
    pub config_path: Option<&'a Path>,
    pub key_path: Option<&'a Path>,
    pub interactive_operator: bool,
    pub additional_access: &'a [String],
    pub validate: &'a dyn Fn(&Path, &WorkspaceProfile) -> Result<(), String>,
}

pub(crate) fn requested(input: &str) -> bool {
    matches!(
        input.split_whitespace().collect::<Vec<_>>().as_slice(),
        ["/workspace"] | ["/settings", "workspaces"]
    )
}

pub(crate) fn run(
    context: WorkspaceSettingsContext<'_>,
    ask: crate::SlashAsk<'_>,
) -> Result<Vec<String>, String> {
    // The caller prints error receipts as terminal text, not as a typed form.
    // Keep paths and backend errors literal on that final display path too.
    run_operator(context, ask).map_err(|error| format!("{error:?}"))
}

fn run_operator(
    context: WorkspaceSettingsContext<'_>,
    ask: crate::SlashAsk<'_>,
) -> Result<Vec<String>, String> {
    if !context.interactive_operator {
        return Err("Workspace settings require an interactive operator.".into());
    }
    let config = context
        .config_path
        .ok_or("A trusted operator configuration is required to save workspace settings.")?;
    let key = context
        .key_path
        .ok_or("The workspace settings signing key is unavailable.")?;
    let snapshot =
        crate::workspace_launch::read_snapshot(config, key).map_err(|error| error.to_string())?;
    let Some(review) = review_edit(&context, &snapshot, ask)? else {
        return Ok(vec!["Workspace settings cancelled; nothing saved.".into()]);
    };
    // Revalidate the reviewed paths immediately before persistence. Neither the
    // preview nor its confirmation grants new authority to the running process.
    validate_changed_profiles(&context, &snapshot, &review)?;
    let path = durable_grants::store_path(config);
    let root = newt_identity::load_user_key(key).map_err(|error| error.to_string())?;
    let existing = match std::fs::symlink_metadata(&path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.to_string()),
    };
    // A new encryption identity is allowed only after this first-save ceremony.
    // Never replace a missing identity belonging to an existing encrypted store.
    let identity = if existing {
        newt_core::secrets::load_identity().map_err(|error| error.to_string())?
            .ok_or("Existing workspace settings require their original encryption identity; no identity was generated.")?
    } else {
        newt_core::secrets::load_or_generate_identity().map_err(|error| error.to_string())?
    };
    durable_grants::commit(&path, &review, &root, &identity).map_err(|error| error.to_string())?;
    Ok(vec!["Workspace settings saved for next launch. This session's authority is unchanged. Close Newt and start a real Newt process to apply the saved profile; /restart only resets the conversation.".into()])
}

struct Draft {
    workspace: PathBuf,
    profile: WorkspaceProfile,
    default_workspace: Option<PathBuf>,
    remove: bool,
}

impl Draft {
    fn select(workspace: PathBuf, snapshot: &VerifiedSnapshot) -> Self {
        let profile = snapshot
            .profiles()
            .get(&workspace.to_string_lossy().into_owned())
            .cloned()
            .unwrap_or_else(|| WorkspaceProfile {
                preset: PermissionPreset::ReadOnly,
                default_cwd: workspace.clone(),
                read_dirs: BTreeSet::new(),
                write_dirs: BTreeSet::new(),
            });
        Self {
            workspace,
            profile,
            default_workspace: snapshot.default_workspace().map(Path::to_path_buf),
            remove: false,
        }
    }

    fn reviewed(
        &self,
        context: &WorkspaceSettingsContext<'_>,
        snapshot: &VerifiedSnapshot,
    ) -> Result<ReviewedWorkspaceEdit, String> {
        let mut edit = snapshot.edit();
        if self.remove {
            edit.remove_profile(
                self.workspace
                    .to_str()
                    .ok_or("Workspace must be valid UTF-8.")?,
            )
            .map_err(|error| error.to_string())?;
        } else {
            let profile = WorkspaceProfile::new(
                &self.workspace,
                &self.profile.default_cwd,
                self.profile.preset.clone(),
                self.profile.read_dirs.clone(),
                self.profile.write_dirs.clone(),
            )
            .map_err(|error| error.to_string())?;
            (context.validate)(&self.workspace, &profile)?;
            edit.set_profile(&self.workspace, profile)
                .map_err(|error| error.to_string())?;
        }
        // remove_profile clears a matching default; it must not be re-added.
        let default = self
            .default_workspace
            .as_deref()
            .filter(|path| !self.remove || *path != self.workspace);
        if edit.default_workspace() != default {
            edit.set_default_workspace(default)
                .map_err(|error| error.to_string())?;
        }
        let review = edit.review().map_err(|error| error.to_string())?;
        validate_changed_profiles(context, snapshot, &review)?;
        Ok(review)
    }
}

fn validate_changed_profiles(
    context: &WorkspaceSettingsContext<'_>,
    snapshot: &VerifiedSnapshot,
    review: &ReviewedWorkspaceEdit,
) -> Result<(), String> {
    for (workspace, profile) in review.profiles() {
        let changed = snapshot.profiles().get(workspace) != Some(profile);
        let new_default = review.default_workspace() != snapshot.default_workspace()
            && review.default_workspace() == Some(Path::new(workspace));
        if changed || new_default {
            (context.validate)(Path::new(workspace), profile)?;
        }
    }
    Ok(())
}

fn review_edit(
    context: &WorkspaceSettingsContext<'_>,
    snapshot: &VerifiedSnapshot,
    ask: crate::SlashAsk<'_>,
) -> Result<Option<ReviewedWorkspaceEdit>, String> {
    let mut draft = Draft::select(context.workspace.to_path_buf(), snapshot);
    let mut notice = String::new();
    loop {
        let body = format!(
            "Workspace settings\n\n{}\n{}",
            preview(context, snapshot, &draft),
            notice
        );
        let Some(choice) = menu(
            body,
            "Paths are literal. Relative paths use the selected workspace; no shell expansion.",
            &[
                ("1", "Select workspace (discard unsaved draft)"),
                ("2", "Default working directory"),
                ("3", "Additional read directories"),
                ("4", "Additional write directories"),
                ("5", "Command and write preset"),
                ("6", "Default workspace for future launches"),
                ("7", "Review and save"),
                ("8", "Remove saved profile"),
                ("0", "Cancel"),
            ],
            ask,
        ) else {
            return Ok(None);
        };
        notice.clear();
        match choice.as_str() {
            "1" => match select_workspace(context, snapshot, ask) {
                Ok(Some(workspace)) => draft = Draft::select(workspace, snapshot),
                Ok(None) => {}
                Err(error) => notice = format!("Cannot select workspace: {}", literal(&error)),
            },
            "2" => {
                let Some(value) = text(format!("Default working directory for {}", path_literal(&draft.workspace)), "Enter keeps the current directory. It must stay inside the selected workspace.", ask) else { return Ok(None); };
                if !value.is_empty() {
                    draft.profile.default_cwd = draft.workspace.join(value);
                    draft.remove = false;
                }
            }
            "3" | "4" => {
                let roots = if choice == "3" {
                    &mut draft.profile.read_dirs
                } else {
                    &mut draft.profile.write_dirs
                };
                let before = roots.clone();
                if !edit_directories(&draft.workspace, roots, ask)? {
                    return Ok(None);
                }
                if *roots != before {
                    draft.remove = false;
                }
            }
            "5" => {
                let Some(choice) = menu("Choose the saved command and write preset.".into(), "Networking is configured separately and will not change.", &[
                    ("1", "Read only: no writes or commands"),
                    ("2", "Workspace edit: writes, no commands"),
                    ("3", "Workspace development: writes and configured development commands"),
                    ("4", "Workspace full access: writes and all commands under filesystem confinement"),
                    ("0", "Back"),
                ], ask) else { return Ok(None); };
                let preset = match choice.as_str() {
                    "1" => Some(PermissionPreset::ReadOnly),
                    "2" => Some(PermissionPreset::WorkspaceEdit),
                    "3" => Some(PermissionPreset::WorkspaceDev),
                    "4" => Some(PermissionPreset::WorkspaceFullAccess),
                    _ => None,
                };
                if let Some(preset) = preset {
                    draft.profile.preset = preset;
                    draft.remove = false;
                }
            }
            "6" => {
                let Some(choice) = menu(
                    "Default workspace for future launches without an explicit directory.".into(),
                    "This does not move the current session.",
                    &[
                        ("1", "Use selected workspace"),
                        ("2", "Clear saved default workspace"),
                        ("0", "Back"),
                    ],
                    ask,
                ) else {
                    return Ok(None);
                };
                match choice.as_str() {
                    "1" => {
                        draft.default_workspace = Some(draft.workspace.clone());
                        draft.remove = false;
                    }
                    "2" => draft.default_workspace = None,
                    _ => {}
                }
            }
            "7" => {
                let review = match draft.reviewed(context, snapshot) {
                    Ok(review) => review,
                    Err(error) => {
                        notice = format!("Cannot save this draft: {}", literal(&error));
                        continue;
                    }
                };
                // Render the exact canonical candidate sealed above, not unresolved draft spellings.
                if let Some(profile) = review
                    .profiles()
                    .get(&draft.workspace.to_string_lossy().into_owned())
                {
                    draft.profile = profile.clone();
                }
                draft.default_workspace = review.default_workspace().map(Path::to_path_buf);
                let body = format!("Review workspace settings\n\n{}\n\nThe current session stays unchanged. Applying this saved policy requires a real Newt process restart. /restart only resets the conversation. To stop current access immediately, end this session. Other saved profiles and separate approvals are retained.", preview(context, snapshot, &draft));
                let interaction = SurfaceInteraction::blocking(interaction_form::confirm(
                    body,
                    "Cancel is the default. Review all paths and separate approvals before saving.",
                    "yes, save for next launch",
                    "no, cancel (default)",
                ))
                .with_default_option(OptionId::new(NO).expect("constant option id"));
                return Ok(match ask(&interaction) {
                    HumanQuestionOutcome::Answer(answer)
                        if interaction_form::resolve(
                            &interaction.definition,
                            interaction.answer_or_default(&answer),
                        )
                        .is_some_and(|id| id.as_str() == YES) =>
                    {
                        Some(review)
                    }
                    _ => None,
                });
            }
            "8" => {
                if snapshot
                    .profiles()
                    .contains_key(&draft.workspace.to_string_lossy().into_owned())
                {
                    draft.remove = true;
                    if draft.default_workspace.as_deref() == Some(&draft.workspace) {
                        draft.default_workspace = None;
                    }
                    notice = "Remove saved profile is staged. Review and save to apply it next launch; separate approvals remain.".into();
                } else {
                    notice = "No saved profile exists for the selected workspace.".into();
                }
            }
            _ => return Ok(None),
        }
    }
}

fn select_workspace(
    context: &WorkspaceSettingsContext<'_>,
    snapshot: &VerifiedSnapshot,
    ask: crate::SlashAsk<'_>,
) -> Result<Option<PathBuf>, String> {
    let mut workspaces = vec![context.workspace.to_path_buf()];
    workspaces.extend(
        snapshot
            .profiles()
            .keys()
            .map(PathBuf::from)
            .filter(|path| path != context.workspace),
    );
    let mut entries = vec![("0".to_owned(), "Other directory".to_owned())];
    entries.extend(
        workspaces
            .iter()
            .enumerate()
            .map(|(i, path)| ((i + 1).to_string(), format!("{path:?}"))),
    );
    entries.push(("back".into(), "Back".into()));
    let refs = entries
        .iter()
        .map(|(id, label)| (id.as_str(), label.as_str()))
        .collect::<Vec<_>>();
    let Some(choice) = menu(
        "Select a workspace. Its own saved profile will replace this unsaved draft.".into(),
        "Selecting does not change this session or the saved launch default.",
        &refs,
        ask,
    ) else {
        return Ok(None);
    };
    if choice == "0" {
        let Some(value) = text(
            "Workspace directory".into(),
            "Use an existing directory. Relative paths use the current session workspace.",
            ask,
        ) else {
            return Ok(None);
        };
        if value.is_empty() {
            return Ok(None);
        }
        let path = context
            .workspace
            .join(value)
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if !path.is_dir() {
            return Err("Workspace must be a directory.".into());
        }
        if path.to_str().is_none() {
            return Err("Workspace must be valid UTF-8.".into());
        }
        Ok(Some(path))
    } else {
        Ok(choice
            .parse::<usize>()
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| workspaces.get(i))
            .cloned())
    }
}

fn edit_directories(
    workspace: &Path,
    roots: &mut BTreeSet<PathBuf>,
    ask: crate::SlashAsk<'_>,
) -> Result<bool, String> {
    let mut notice = String::new();
    loop {
        let body = format!(
            "Additional directories\n{}\n{}",
            directory_list(roots),
            notice
        );
        let Some(choice) = menu(body, "The workspace root remains included by the preset. Removing a child does not revoke access through a remaining parent or separate approval.", &[("1", "Add directory"), ("2", "Remove directory"), ("3", "Clear additional directories"), ("0", "Back")], ask) else { return Ok(false); };
        notice.clear();
        match choice.as_str() {
            "1" => {
                let Some(value) = text("Directory to add".into(), "One literal directory; spaces are preserved. Relative paths use the selected workspace.", ask) else { return Ok(false); };
                if value.is_empty() {
                    continue;
                }
                match workspace.join(value).canonicalize() {
                    Ok(path) if path.is_dir() && path.to_str().is_some() => {
                        roots.insert(path);
                    }
                    Ok(_) => notice = "Use an existing UTF-8 directory.".into(),
                    Err(error) => {
                        notice =
                            format!("Cannot resolve directory: {}", literal(&error.to_string()));
                    }
                }
            }
            "2" if !roots.is_empty() => {
                let paths = roots.iter().cloned().collect::<Vec<_>>();
                let entries = paths
                    .iter()
                    .enumerate()
                    .map(|(i, path)| ((i + 1).to_string(), format!("{path:?}")))
                    .chain(std::iter::once(("0".into(), "Back".into())))
                    .collect::<Vec<_>>();
                let refs = entries
                    .iter()
                    .map(|(id, label)| (id.as_str(), label.as_str()))
                    .collect::<Vec<_>>();
                let Some(choice) = menu(
                    "Remove an additional directory".into(),
                    "Remaining parent grants still apply.",
                    &refs,
                    ask,
                ) else {
                    return Ok(false);
                };
                if let Some(path) = choice
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| paths.get(i))
                {
                    roots.remove(path);
                }
            }
            "3" => roots.clear(),
            "0" => return Ok(true),
            _ => {}
        }
    }
}

fn preview(
    context: &WorkspaceSettingsContext<'_>,
    snapshot: &VerifiedSnapshot,
    draft: &Draft,
) -> String {
    let mut lines = vec![
        format!("Active this session: {}", path_literal(context.workspace)),
        format!(
            "Current working directory: {}",
            path_literal(context.current_dir)
        ),
        access_summary(context.active),
        format!("Selected workspace: {}", path_literal(&draft.workspace)),
        "Saved for next launch (currently stored):".into(),
    ];
    if let Some(saved) = snapshot
        .profiles()
        .get(&draft.workspace.to_string_lossy().into_owned())
    {
        lines.push(profile_summary(saved));
    } else {
        lines.push("No saved profile; configured permissions are the fallback.".into());
    }
    lines.push(format!(
        "Stored default workspace: {}",
        optional_path(snapshot.default_workspace())
    ));
    if let Some(config) = context.config_path {
        lines.push(format!(
            "Saved settings store: {}",
            path_literal(&durable_grants::store_path(config))
        ));
    }
    lines.push("Draft for next launch:".into());
    if draft.remove {
        lines.push("Remove saved profile. Configured permissions become the fallback; separate approvals remain.".into());
        lines.push(access_summary(
            &context
                .permissions
                .to_caveats(&draft.workspace.to_string_lossy()),
        ));
    } else {
        lines.push(profile_summary(&draft.profile));
        match WorkspaceProfile::new(
            &draft.workspace,
            &draft.profile.default_cwd,
            draft.profile.preset.clone(),
            draft.profile.read_dirs.clone(),
            draft.profile.write_dirs.clone(),
        )
        .and_then(|profile| profile.caveats(&draft.workspace, context.permissions))
        {
            Ok(caveats) => {
                lines.push(
                    "Profile access, before separate approvals and launch constraints:".into(),
                );
                lines.push(access_summary(&caveats));
            }
            Err(error) => lines.push(format!(
                "Draft needs correction before Save: {}",
                literal(&error.to_string())
            )),
        }
    }
    lines.push(format!(
        "Proposed default workspace: {}",
        optional_path(draft.default_workspace.as_deref())
    ));
    lines.push("Network settings and development command additions come from independent configuration and explicit launch choices; this editor does not save or freeze them.".into());
    lines.push("Separate saved approvals for this selected workspace (retained):".into());
    if let Some(grants) = snapshot
        .grants()
        .get(&draft.workspace.to_string_lossy().into_owned())
        .filter(|grants| !grants.is_empty())
    {
        lines.extend(
            grants
                .iter()
                .map(|(kind, target)| format!("{}: {}", kind.as_str(), literal(target))),
        );
    } else {
        lines.push("None.".into());
    }
    lines.push("Other launch access and constraints (separate from this profile):".into());
    if context.additional_access.is_empty() {
        lines.push(
            "No additional launch access reported. Request-specific approvals can still apply."
                .into(),
        );
    } else {
        lines.extend(context.additional_access.iter().map(|line| literal(line)));
    }
    lines.push("The current session stays unchanged; a saved profile is not a ceiling on separate approvals. A default working directory selects where commands start; it grants no access itself.".into());
    lines.join("\n\n")
}

fn profile_summary(profile: &WorkspaceProfile) -> String {
    format!("Preset: {} — {}\nDefault working directory: {}\nAdditional read directories:\n{}\nAdditional write directories:\n{}",profile.preset.as_str(),profile.preset.description(),path_literal(&profile.default_cwd),directory_list(&profile.read_dirs),directory_list(&profile.write_dirs))
}

fn access_summary(caveats: &Caveats) -> String {
    format!(
        "Read: {}\nWrite: {}\nCommands: {}\nNetwork: {}",
        scope(&caveats.fs_read, true),
        scope(&caveats.fs_write, true),
        scope(&caveats.exec, false),
        scope(&caveats.net, false)
    )
}

fn scope(scope: &Scope<String>, paths: bool) -> String {
    match scope {
        Scope::All => "all".into(),
        Scope::Only(values) if paths => directory_list(&values.iter().map(PathBuf::from).collect()),
        Scope::Only(values) => {
            if values.is_empty() {
                "none".into()
            } else {
                values
                    .iter()
                    .map(|value| literal(value))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        }
    }
}

fn directory_list(paths: &BTreeSet<PathBuf>) -> String {
    if paths.is_empty() {
        return "None.".into();
    }
    paths
        .iter()
        .map(|path| {
            let overlap = paths
                .iter()
                .find(|parent| *parent != path && path.starts_with(parent));
            match overlap {
                Some(parent) => format!(
                    "{} (also covered by {})",
                    path_literal(path),
                    path_literal(parent)
                ),
                None => path_literal(path),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn optional_path(path: Option<&Path>) -> String {
    path.map_or_else(
        || "none (normal launch directory selection)".into(),
        path_literal,
    )
}
fn path_literal(path: &Path) -> String {
    literal(&path.to_string_lossy())
}

fn literal(value: &str) -> String {
    let quoted = format!("{value:?}");
    let longest = quoted.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let delimiter = "`".repeat(longest + 1);
    format!("{delimiter}{quoted}{delimiter}")
}

fn menu(
    body: String,
    hint: &str,
    entries: &[(&str, &str)],
    ask: crate::SlashAsk<'_>,
) -> Option<String> {
    let interaction = SurfaceInteraction::blocking(interaction_form::menu(body, hint, entries));
    match ask(&interaction) {
        HumanQuestionOutcome::Answer(answer) => {
            interaction_form::resolve(&interaction.definition, &answer)
                .map(|id| id.as_str().to_owned())
        }
        _ => None,
    }
}
fn text(body: String, hint: &str, ask: crate::SlashAsk<'_>) -> Option<String> {
    let interaction = SurfaceInteraction::blocking(interaction_form::text_field(body, hint));
    match ask(&interaction) {
        HumanQuestionOutcome::Answer(answer) => Some(answer),
        _ => None,
    }
}

#[cfg(test)]
#[path = "workspace_settings_tests.rs"]
pub(crate) mod tests;
