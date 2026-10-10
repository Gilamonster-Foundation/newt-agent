//! Verify offered catalogs against append-only host continuations.
use super::*;

impl Session {
    /// Reconstruct a catalog from a prior offered inventory and an append-only
    /// extension. Any host pin revision, unrelated history edit or intervening
    /// composition refuses carry-forward. Parked entries keep their old order.
    pub(super) fn carried_universe(
        &self,
        offered: ContentId,
        current: &Projection,
    ) -> Result<Projection> {
        if !self.composition.catalogs.contains(&offered) {
            return Err(Error::Access("offered catalog absent".into()));
        }
        let old: Catalog = self.store.get(&offered)?;
        if old.root != self.root
            || old.pin_revision != self.composition.pin_revision
            || old.pins != composition::pin_identity(&self.semantic_pins)?
        {
            return Err(Error::Proposal("catalog pins changed".into()));
        }
        let before = if self.composition.head == old.expected_head {
            old.before
        } else {
            let d = self
                .composition
                .head
                .and_then(|id| self.composition.decisions.get(&id))
                .ok_or_else(|| Error::Proposal("catalog head changed".into()))?;
            if d.catalog != offered || d.outcome != Outcome::Accepted {
                return Err(Error::Proposal("catalog head changed".into()));
            }
            d.after.ok_or_else(|| integrity("accepted view absent"))?
        };
        let prior = self.checked_composition_projection(before)?;
        if !current.entries.starts_with(&prior.entries) {
            return Err(Error::Proposal(
                "catalog view is not an append-only continuation".into(),
            ));
        }
        let old_universe = self.checked_composition_projection(old.universe)?;
        let mut result = current.clone();
        result.entries = old_universe.entries;
        for entry in &current.entries[prior.entries.len()..] {
            if result.entries.iter().any(|e| e.event == entry.event) {
                return Err(integrity("continued occurrence repeated"));
            }
            result.entries.push(entry.clone());
        }
        Ok(result)
    }

    pub(super) fn carry_catalog(
        &mut self,
        offered: ContentId,
        request: ContentId,
        policy: Policy,
    ) -> Result<ContentId> {
        let record: RequestRecord = self.store.get(&request)?;
        let before = self.checked_composition_projection(record.projection)?;
        let universe = self.carried_universe(offered, &before)?;
        if universe.entries.len() > self.config.max_catalog_entries {
            return Err(Error::Budget("continued catalog entries".into()));
        }
        let universe_id = self.store.put(&universe)?;
        let c = Catalog {
            offered: Some(offered),
            run: self.run,
            root: self.root,
            request,
            expected_head: self.composition.head,
            before: record.projection,
            universe: universe_id,
            policy: self.store.put(&policy)?,
            context_revision: self.composition.context_revision,
            pin_revision: self.composition.pin_revision,
            pins: composition::pin_identity(&self.semantic_pins)?,
            entries: universe.entries,
        };
        let id = self.store.put(&c)?;
        self.append(JournalEntry::CompositionCatalog { catalog: id })?;
        Ok(id)
    }

    pub(super) fn verify_queued_submission(&self, s: &Submission, c: &Catalog) -> Result<()> {
        if let Some(id) = s.queued {
            if self.composition.pending.first() != Some(&id) {
                return Err(integrity("queued proposal absent or reordered"));
            }
            let q: Queued = self.store.get(&id)?;
            let actor: Actor = self.store.get(&s.actor)?;
            if q.actor != actor
                || q.proposal.changes != s.proposal.changes
                || q.proposal.inverse != s.proposal.inverse
                || (c.offered != Some(q.catalog) && s.catalog != q.catalog)
            {
                return Err(integrity("queued proposal substituted"));
            }
            let old: Catalog = self.store.get(&q.catalog)?;
            if c.offered.is_some()
                && (q.proposal.expected_head != old.expected_head
                    || s.proposal.expected_head != c.expected_head)
            {
                return Err(integrity("queued expected head substituted"));
            }
        }
        Ok(())
    }

    pub fn has_pending_composition(&self) -> bool {
        !self.composition.pending.is_empty()
    }

    /// Caller has recorded the complete host request after tools and pin updates.
    /// Stale proposals are retained as refusals; accepted views are ready to bind.
    pub fn resolve_queued_composition(
        &mut self,
        request: ContentId,
        max_bytes: usize,
    ) -> Result<Vec<Receipt>> {
        self.ensure_writer()?;
        self.ensure_tools_closed()?;
        let mut receipts = Vec::new();
        while let Some(id) = self.composition.pending.first().copied() {
            let q: Queued = self.store.get(&id)?;
            let old: Catalog = self.store.get(&q.catalog)?;
            let mut proposal = q.proposal;
            let catalog = if proposal.expected_head == old.expected_head {
                match self.carry_catalog(q.catalog, request, Policy { max_bytes }) {
                    Ok(id) => {
                        proposal.expected_head = self.composition.head;
                        id
                    }
                    Err(Error::Proposal(_)) => q.catalog,
                    Err(e) => return Err(e),
                }
            } else {
                q.catalog
            };
            let event = self.submit_composition(catalog, q.actor, proposal, Some(id))?;
            receipts.push(self.decide_composition(event)?);
        }
        Ok(receipts)
    }
}
