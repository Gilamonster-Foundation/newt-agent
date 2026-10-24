//! #144: `propose_config_edit` — newt proposes an edit to its own
//! `config.toml` and the operator applies or declines it at the terminal.
//!
//! `config.toml` sits outside the write fence, so no file tool can reach it,
//! and that is the point: the only path is this tool, and it writes only on an
//! explicit `apply`. The diff goes to the operator alone, never into the tool
//! result, because the surrounding config can hold secrets.

use std::path::Path;

use super::super::permissions::{ChoiceSpec, HumanQuestionOutcome, PermissionGate};
use crate::config::{backup_path, Config, ConfigEdit};

/// `no` first: a blank submission is the default, and the default declines.
const CONFIG_EDIT_CHOICES: &[ChoiceSpec] = &[
    ChoiceSpec {
        id: "no",
        label: "no — leave the config unchanged",
        key: "n",
        role: newt_interaction::SemanticRole::Deny,
    },
    ChoiceSpec {
        id: "apply",
        label: "apply — write this edit",
        key: "a",
        role: newt_interaction::SemanticRole::Allow,
    },
];

const NO_OPERATOR: &str = "propose_config_edit: no operator to confirm; config unchanged.";

pub(super) fn propose_config_edit_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "propose_config_edit",
            "description": "Propose an edit to newt's own config.toml (e.g. widen \
                            shell.env_passthrough). The operator sees the diff and \
                            must choose apply; nothing is written otherwise.",
            "parameters": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Dotted config key, e.g. shell.env_passthrough" },
                    "value": { "description": "New value: string, number, boolean or list" },
                    "append": { "type": "boolean", "description": "Add to a list instead of replacing it" },
                    "reason": { "type": "string", "description": "Why, shown to the operator" }
                },
                "required": ["key", "value", "reason"]
            }
        }
    })
}

/// Ask the operator to apply one config edit. `config` is the user config
/// path and `receipts` the settings journal; the dispatcher resolves both.
pub(super) fn execute_propose_config_edit(
    args: &serde_json::Value,
    gate: Option<&mut dyn PermissionGate>,
    config: Option<&Path>,
    receipts: Option<&Path>,
) -> String {
    let key = args["key"].as_str().unwrap_or("").trim();
    let reason = args["reason"].as_str().unwrap_or("").trim();
    if key.is_empty() || reason.is_empty() || args["value"].is_null() {
        return "propose_config_edit: 'key', 'value' and 'reason' are required.".to_string();
    }
    let (Some(gate), Some(path)) = (gate, config) else {
        return NO_OPERATOR.to_string();
    };
    let edit = ConfigEdit {
        key: key.to_string(),
        value: args["value"].clone(),
        append: args["append"].as_bool().unwrap_or(false),
    };
    let current = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return format!("propose_config_edit: read config: {error}"),
    };
    let proposed = match Config::with_config_edit(&current, &edit) {
        Ok(text) => text,
        Err(error) => return format!("propose_config_edit: {error}; config unchanged."),
    };
    if proposed == current {
        return format!("propose_config_edit: {key} already has that value; config unchanged.");
    }
    let question = format!(
        "newt proposes an edit to {}\nreason: {reason}\n\n{}",
        path.display(),
        diffy::create_patch(&current, &proposed)
    );
    match gate.ask_choice(&question, CONFIG_EDIT_CHOICES) {
        HumanQuestionOutcome::Answer(answer) if answer.trim().eq_ignore_ascii_case("apply") => {}
        HumanQuestionOutcome::Answer(_) => {
            return "propose_config_edit: the operator declined; config unchanged.".to_string()
        }
        _ => return NO_OPERATOR.to_string(),
    }
    let written = match Config::apply_config_edit(path, &current, &edit) {
        Ok(text) => text,
        Err(error) => return format!("propose_config_edit: {error}"),
    };
    // Best effort: a journal failure must not undo the applied edit. The
    // receipt names file digests, never values, so no secret lands in it.
    if let Some(receipts) = receipts {
        let digest = |text: &str| {
            content_addressable::RawContentId::from_content(text.as_bytes()).to_string()
        };
        let change = crate::settings_receipt::SettingChange::new(
            &format!("config.{key}"),
            digest(&current),
            digest(&written),
            "propose_config_edit",
        );
        if let Ok(receipt) = crate::settings_receipt::SettingReceipt::mint(change) {
            let _ = receipt.append_jsonl(receipts);
        }
    }
    format!(
        "propose_config_edit: applied {key} to {}; backup at {}. It takes effect when newt next reads its config.",
        path.display(),
        backup_path(path).display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gate whose operator gives one scripted outcome to `ask_choice`.
    struct ChoiceGate {
        outcome: HumanQuestionOutcome,
        asked: Vec<(String, Vec<&'static str>)>,
    }

    impl PermissionGate for ChoiceGate {
        fn ask(
            &mut self,
            _: &[super::super::super::permissions::PermissionRequest],
        ) -> super::super::super::permissions::PermissionDecision {
            super::super::super::permissions::PermissionDecision::Deny
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            panic!("a config edit is a choice, not free text")
        }
        fn ask_choice(&mut self, question: &str, options: &[ChoiceSpec]) -> HumanQuestionOutcome {
            self.asked.push((
                question.to_string(),
                options.iter().map(|option| option.id).collect(),
            ));
            self.outcome.clone()
        }
    }

    fn gate(outcome: HumanQuestionOutcome) -> ChoiceGate {
        ChoiceGate {
            outcome,
            asked: Vec::new(),
        }
    }

    const ORIGINAL: &str =
        "# mine\napi_secret_note = \"s3cret\"\n[shell]\nenv_passthrough = [\"PATH\"]\n";

    fn browser() -> serde_json::Value {
        serde_json::json!({
            "key": "shell.env_passthrough",
            "value": ["BROWSER"],
            "append": true,
            "reason": "open a browser"
        })
    }

    #[test]
    fn propose_config_edit_confirm_writes_and_keeps_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let receipts = dir.path().join("receipts.jsonl");
        std::fs::write(&path, ORIGINAL).unwrap();
        let mut operator = gate(HumanQuestionOutcome::Answer("apply".into()));

        let out = execute_propose_config_edit(
            &browser(),
            Some(&mut operator),
            Some(&path),
            Some(&receipts),
        );

        assert!(out.contains("applied shell.env_passthrough"), "{out}");
        let (question, options) = &operator.asked[0];
        assert_eq!(options, &["no", "apply"]);
        assert!(question.contains("+env_passthrough = [\"PATH\", \"BROWSER\"]"));
        // The diff (and the config around it) is for the operator only.
        assert!(!out.contains("s3cret") && !out.contains("BROWSER"));
        let saved: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved.shell.unwrap().env_passthrough.unwrap(),
            vec!["PATH", "BROWSER"]
        );
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            ORIGINAL
        );
        let journal =
            crate::settings_receipt::read_jsonl(&std::fs::read_to_string(receipts).unwrap());
        assert_eq!(journal.len(), 1);
        assert!(journal[0].is_intact());
        assert_eq!(journal[0].change.setting, "config.shell.env_passthrough");
        assert!(!journal[0].change.to.to_string().contains("BROWSER"));
    }

    #[test]
    fn propose_config_edit_decline_leaves_bytes_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, ORIGINAL).unwrap();
        // `no`, the blank default resolved by the TUI, and stray text all decline.
        for answer in ["no", "", "yes"] {
            let mut operator = gate(HumanQuestionOutcome::Answer(answer.into()));
            let out =
                execute_propose_config_edit(&browser(), Some(&mut operator), Some(&path), None);
            assert!(out.contains("declined"), "{answer}: {out}");
            assert_eq!(operator.asked.len(), 1);
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ORIGINAL);
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn propose_config_edit_headless_refuses_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, ORIGINAL).unwrap();
        assert_eq!(
            execute_propose_config_edit(&browser(), None, Some(&path), None),
            NO_OPERATOR
        );
        for outcome in [
            HumanQuestionOutcome::Unavailable,
            HumanQuestionOutcome::Cancelled,
            HumanQuestionOutcome::ExitRequested,
            HumanQuestionOutcome::InputClosed,
            HumanQuestionOutcome::InputFailed,
        ] {
            let mut operator = gate(outcome);
            assert_eq!(
                execute_propose_config_edit(&browser(), Some(&mut operator), Some(&path), None),
                NO_OPERATOR
            );
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ORIGINAL);
        assert!(!backup_path(&path).exists());
    }
}
