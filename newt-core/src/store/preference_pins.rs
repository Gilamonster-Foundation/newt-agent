//! Atomic updates of the existing preference column, including inverse evidence.
use super::ConversationStore;
use crate::psyche::{ObsessiveMode, ObsessivePin, ObsessiveSelection};
use crate::{OperatorPreferencePin, PreferenceActions};
use rusqlite::{params, OptionalExtension, TransactionBehavior};

impl ConversationStore {
    pub(super) fn validate_preference_pin(
        &self,
        id: &str,
        pin: &OperatorPreferencePin,
    ) -> anyhow::Result<()> {
        if let Some(overlay) = &pin.obsessive {
            let original = overlay.verified_selection(id, &self.workspace_id)?;
            let (cognition, tenacity) = match overlay.mode() {
                ObsessiveMode::On => (
                    Some(crate::psyche::OBSESSIVE_COGNITION.label().into()),
                    Some(crate::psyche::OBSESSIVE_TENACITY),
                ),
                ObsessiveMode::Off => (
                    OperatorPreferencePin::cognition_field(original.cognition),
                    original.tenacity,
                ),
            };
            anyhow::ensure!(
                pin.cognition == cognition && pin.tenacity == tenacity,
                "obsessive preference pin disagrees with its addressed effort selection"
            );
        }
        Ok(())
    }

    /// Read, verify, merge and write while holding the same database transaction.
    /// Returns false only when this workspace has no durable row for `id` yet;
    /// the caller must retain its pending original selectors in that case.
    pub fn merge_preference_actions(
        &self,
        id: &str,
        actions: &PreferenceActions,
    ) -> anyhow::Result<bool> {
        let mut connection = self.lock_conn();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let json: Option<String> = transaction
            .query_row(
                "SELECT preference_pin FROM conversations WHERE id = ?1 AND workspace_key = ?2",
                params![id, self.workspace_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(json) = json else {
            return Ok(false);
        };
        let stored: OperatorPreferencePin = serde_json::from_str(&json)?;
        self.validate_preference_pin(id, &stored)?;
        let mut next = stored.merged(actions);
        if let Some(Some(original)) = actions.obsessive {
            next.cognition = Some(crate::psyche::OBSESSIVE_COGNITION.label().into());
            next.tenacity = Some(crate::psyche::OBSESSIVE_TENACITY);
            next.obsessive = Some(ObsessivePin::mint(
                id,
                &self.workspace_id,
                ObsessiveMode::On,
                original,
            )?);
        } else if actions.obsessive == Some(None)
            || stored
                .obsessive
                .as_ref()
                .is_some_and(|pin| pin.mode() == ObsessiveMode::Off)
        {
            let previous = stored.obsessive.as_ref().map(ObsessivePin::original);
            // Off after a rowless launch still has the acted restored axes.
            // Never reconstruct an original from ambient defaults or assume
            // auto when neither the action nor verified stored pin supplied it.
            let original = ObsessiveSelection {
                cognition: actions
                    .cognition
                    .or_else(|| previous.map(|s| s.cognition))
                    .ok_or_else(|| {
                        anyhow::anyhow!("off action lacks cognition restore evidence")
                    })?,
                tenacity: actions
                    .tenacity
                    .or_else(|| previous.map(|s| s.tenacity))
                    .ok_or_else(|| anyhow::anyhow!("off action lacks tenacity restore evidence"))?,
            };
            next.cognition = OperatorPreferencePin::cognition_field(original.cognition);
            next.tenacity = original.tenacity;
            next.obsessive = Some(ObsessivePin::mint(
                id,
                &self.workspace_id,
                ObsessiveMode::Off,
                original,
            )?);
        }
        self.validate_preference_pin(id, &next)?;
        if next != stored {
            let changed = transaction.execute(
                "UPDATE conversations SET preference_pin = ?2 WHERE id = ?1 AND workspace_key = ?3",
                params![id, serde_json::to_string(&next)?, self.workspace_id],
            )?;
            anyhow::ensure!(changed == 1, "preference owner disappeared during update");
        }
        transaction.commit()?;
        Ok(true)
    }
}
