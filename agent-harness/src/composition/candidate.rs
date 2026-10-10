//! Pure candidate validation: no live state or checkpoint mutations.
use super::*;

impl Session {
    pub(super) fn composition_candidate(
        &self,
        c: &Catalog,
        proposal: &Proposal,
    ) -> Result<std::result::Result<Projection, Refusal>> {
        // Even refused proposals must not conceal unavailable retained evidence.
        let universe = self.checked_composition_projection(c.universe)?;
        let before = self.checked_composition_projection(c.before)?;
        let policy: Policy = self.store.get(&c.policy)?;
        if let Some(pins) = c.pins {
            self.store.get::<composition::Pins>(&pins)?;
        }
        if c.run != self.run
            || c.root != self.root
            || c.context_revision != self.composition.context_revision
            || c.pin_revision != self.composition.pin_revision
            || c.pins != composition::pin_identity(&self.semantic_pins)?
            || c.expected_head != self.composition.head
            || proposal.expected_head != c.expected_head
            || c.before != self.composition_current_projection()?
            || self.composition.latest_request != Some(c.request)
        {
            return Ok(Err(Refusal::Stale));
        }
        let mut view = before.clone();
        if let Some(inverse) = proposal.inverse {
            let Some(original) = self.composition.decisions.get(&inverse) else {
                return Ok(Err(Refusal::InvalidChange));
            };
            if !proposal.changes.is_empty()
                || original.outcome != Outcome::Accepted
                || original.after != Some(c.before)
                || original.root != c.root
                || original.context_revision != c.context_revision
                || original.pin_revision != c.pin_revision
                || original.pins != c.pins
                || original.policy != c.policy
            {
                return Ok(Err(Refusal::Stale));
            }
            view = self.checked_composition_projection(original.before)?;
        } else {
            if proposal.changes.len() > self.config.max_catalog_entries {
                return Ok(Err(Refusal::InvalidChange));
            }
            let mut changed = BTreeSet::new();
            let mut selected: BTreeSet<_> = before.entries.iter().map(|e| e.event).collect();
            for change in &proposal.changes {
                if !changed.insert(change.occurrence)
                    || change.reason.trim().is_empty()
                    || change.reason.len() > 4096
                    || !universe
                        .entries
                        .iter()
                        .any(|e| e.event == change.occurrence)
                {
                    return Ok(Err(Refusal::InvalidChange));
                }
                let progress = match change.action {
                    Action::Include => selected.insert(change.occurrence),
                    Action::Park => selected.remove(&change.occurrence),
                };
                if !progress {
                    return Ok(Err(Refusal::NoProgress));
                }
            }
            view.entries = universe
                .entries
                .iter()
                .filter(|e| selected.contains(&e.event))
                .cloned()
                .collect();
        }
        if view == before {
            return Ok(Err(Refusal::NoProgress));
        }
        if view.template != before.template
            || view.renderer != before.renderer
            || view.field != before.field
            || view.entries.iter().any(|e| !universe.entries.contains(e))
        {
            return Err(integrity(
                "inverse projection is outside its original occurrence universe",
            ));
        }
        let messages = universe
            .entries
            .iter()
            .map(|e| serde_json::from_slice(&self.store.source(&e.source)?).map_err(integrity))
            .collect::<Result<Vec<Value>>>()?;
        let candidates = self.candidates(&messages, &universe.entries)?;
        let selected = view.entries.iter().map(|e| e.event).collect::<Vec<_>>();
        match crate::navigation::validate_selection(&candidates, &selected, policy.max_bytes) {
            Ok(()) => {}
            Err(Error::Budget(_)) => return Ok(Err(Refusal::Capacity)),
            Err(Error::Proposal(_)) => return Ok(Err(Refusal::RequiredInput)),
            Err(e) => return Err(e),
        }
        if view.render(&self.store)?.len() > policy.max_bytes {
            return Ok(Err(Refusal::Capacity));
        }
        Ok(Ok(view))
    }
}
