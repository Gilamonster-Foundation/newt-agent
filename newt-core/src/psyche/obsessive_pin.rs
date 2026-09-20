//! An addressed inverse embedded in the existing conversation preference pin.
//! This checks content and ownership, not writer authenticity or history.
use content_addressable::{canonical, ContentAddressable, ContentError, ContentId};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use super::ObsessiveSelection;

const SCHEMA: &str = "newt.obsessive-inverse/v1";

/// An acted choice, distinct from the absence of any conversation preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObsessiveMode {
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inverse {
    schema: String,
    conversation: String,
    workspace: String,
    mode: ObsessiveMode,
    // While on these are the captured original selectors. While off they are
    // the restored selectors, updated only by subsequent operator actions.
    original: ObsessiveSelection,
}

impl ContentAddressable for Inverse {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

/// Original selections with their content identity and conversation owner.
/// Removing the whole optional record or replacing both payload and identity
/// is not detectable without an independent commitment; no such claim is made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObsessivePin {
    id: String,
    inverse: Inverse,
}

impl ObsessivePin {
    /// Bind an operator-captured original to the store's existing owner keys.
    pub fn mint(
        conversation: &str,
        workspace: &str,
        mode: ObsessiveMode,
        original: ObsessiveSelection,
    ) -> Result<Self, ContentError> {
        let inverse = Inverse {
            schema: SCHEMA.to_owned(),
            conversation: conversation.to_owned(),
            workspace: workspace.to_owned(),
            mode,
            original,
        };
        Ok(Self {
            id: inverse.content_id()?.to_string(),
            inverse,
        })
    }

    /// Verify before consumption, including the expected store/conversation.
    pub fn verified_selection(
        &self,
        conversation: &str,
        workspace: &str,
    ) -> anyhow::Result<ObsessiveSelection> {
        anyhow::ensure!(
            self.inverse.schema == SCHEMA
                && self.inverse.conversation == conversation
                && self.inverse.workspace == workspace,
            "obsessive restore evidence has the wrong schema or owner"
        );
        let id = ContentId::from_str(&self.id)?;
        anyhow::ensure!(
            self.inverse.verify(&id)?,
            "obsessive original selections do not match their content identity"
        );
        Ok(self.inverse.original)
    }

    /// Projection from an already verified pin. Store reads validate first.
    pub fn original(&self) -> ObsessiveSelection {
        self.inverse.original
    }

    /// Acted mode from the same owner-verified, content-addressed payload.
    pub fn mode(&self) -> ObsessiveMode {
        self.inverse.mode
    }
}
