//! Host semantic registrations, independent of model relevance proposals.
use content_addressable::{ContentAddressable, ContentError, ContentId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Why a message cannot be removed by relevance selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PinClass {
    Objective,
    SelectedTarget,
    ObservedFacts,
    LatestOperator,
    SystemConstraint,
    LiveToolExchange,
}

/// An explicit host registration in the supplied, pre-wire message array.
/// Indices are consumed at registration; persisted bindings name occurrences.
#[derive(Debug, Clone, Copy)]
pub struct HostPin {
    pub class: PinClass,
    pub index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pins {
    /// Host objective locator, not a content identity or read capability.
    pub objective: String,
    pub entries: BTreeMap<PinClass, ContentId>,
}
impl ContentAddressable for Pins {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}

impl PinClass {
    pub(crate) fn protocol(
        latest_operator: bool,
        system: bool,
        live_exchange: bool,
    ) -> Option<Self> {
        if system {
            Some(Self::SystemConstraint)
        } else if latest_operator {
            Some(Self::LatestOperator)
        } else if live_exchange {
            Some(Self::LiveToolExchange)
        } else {
            None
        }
    }

    pub(crate) fn semantic(self) -> bool {
        matches!(
            self,
            Self::Objective | Self::SelectedTarget | Self::ObservedFacts
        )
    }
    pub(crate) fn host_material(self) -> bool {
        matches!(self, Self::SelectedTarget | Self::ObservedFacts)
    }
}
