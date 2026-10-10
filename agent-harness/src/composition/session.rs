use super::*;
use crate::composition::{HostPin, PinClass, Pins};

impl Session {
    /// Replace the objective's semantic slots from trusted host state. This is
    /// not exposed to models. Ordinary selection cannot revise these bindings.
    pub fn register_semantic_pins(
        &mut self,
        objective: &str,
        messages: &mut Vec<Value>,
        registrations: &[HostPin],
    ) -> Result<ContentId> {
        let mut classes = BTreeSet::new();
        let mut indices = BTreeSet::new();
        if objective.is_empty() || objective.len() > 4096 {
            return Err(Error::Proposal("invalid objective locator".into()));
        }
        for pin in registrations {
            let message = messages
                .get(pin.index)
                .ok_or_else(|| integrity("pin index absent"))?;
            if !pin.class.semantic()
                || !classes.insert(pin.class)
                || !indices.insert(pin.index)
                || message["role"] != "user"
                || !message["content"].is_string()
                || message.as_object().is_none_or(|m| m.len() != 2)
            {
                return Err(Error::Proposal(
                    "semantic pins require distinct typed text messages".into(),
                ));
            }
        }
        if !classes.contains(&PinClass::Objective) {
            return Err(Error::Proposal("semantic pins require an objective".into()));
        }
        let entries = self.ingest_registered(messages, registrations)?;
        let pins = Pins {
            objective: objective.to_owned(),
            entries: registrations
                .iter()
                .map(|p| (p.class, entries[p.index].event))
                .collect(),
        };
        let id = pins.content_id().map_err(integrity)?;
        let retired: BTreeSet<_> = self
            .semantic_pins
            .iter()
            .flat_map(|old| old.entries.iter())
            .filter(|(class, event)| {
                class.host_material() && !pins.entries.values().any(|new| new == *event)
            })
            .map(|(_, event)| *event)
            .collect();
        if self.semantic_pins.as_ref() != Some(&pins) {
            self.append(JournalEntry::SemanticPins { pins })?;
        }
        if !retired.is_empty() {
            let mut index = 0;
            messages.retain(|_| {
                let keep = !retired.contains(&entries[index].event);
                index += 1;
                keep
            });
            self.record_messages(messages)?;
        }
        Ok(id)
    }

    pub(super) fn apply_semantic_pins(&mut self, pins: &Pins) -> Result<()> {
        if pins.objective.is_empty()
            || pins.objective.len() > 4096
            || !pins.entries.contains_key(&PinClass::Objective)
        {
            return Err(integrity("invalid semantic pin snapshot"));
        }
        let mut distinct = BTreeSet::new();
        for (class, id) in &pins.entries {
            let event = self
                .events
                .get(id)
                .ok_or_else(|| integrity("pin event absent"))?;
            let expected = if class.host_material() {
                EventOrigin::Harness
            } else {
                EventOrigin::Operator
            };
            let message: Value = serde_json::from_slice(&self.store.source(&event.body().payload)?)
                .map_err(integrity)?;
            if !class.semantic()
                || !distinct.insert(id)
                || event.body().origin != expected
                || message["role"] != "user"
                || !message["content"].is_string()
                || message.as_object().is_none_or(|m| m.len() != 2)
            {
                return Err(integrity("semantic pin origin or envelope mismatch"));
            }
        }
        self.semantic_pins = Some(pins.clone());
        Ok(())
    }

    pub(super) fn require_semantic_pins(&self, entries: &[Entry]) -> Result<()> {
        if self.semantic_pins.as_ref().is_some_and(|pins| {
            pins.entries
                .values()
                .any(|id| !entries.iter().any(|entry| entry.event == *id))
        }) {
            return Err(Error::Proposal(
                "context omits a current semantic pin (possibly a stale revision)".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn semantic_pin(&self, id: ContentId) -> bool {
        self.semantic_pins
            .as_ref()
            .is_some_and(|pins| pins.entries.values().any(|pin| *pin == id))
    }
}
