//! Prepare model choices before committing the final provider request.
use super::*;

impl Session {
    pub fn latest_composition_catalog(&self) -> Option<ContentId> {
        self.composition.latest_catalog
    }

    /// Carry retained parked occurrences across an append-only host continuation.
    pub fn refresh_composition_catalog(
        &mut self,
        request: ContentId,
        max_bytes: usize,
    ) -> Result<ContentId> {
        if let Some(old) = self.composition.latest_catalog {
            match self.carry_catalog(old, request, Policy { max_bytes }) {
                Ok(id) => return Ok(id),
                Err(Error::Proposal(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(self.composition_catalog(request, Policy { max_bytes })?.0)
    }

    /// A host-only preflight projection for the auxiliary, not a provider send.
    /// The special format does not pin or change the session's provider format.
    pub fn composition_context(
        &mut self,
        messages: &[Value],
        max_bytes: usize,
    ) -> Result<ContentId> {
        self.record_messages(messages)?;
        let r = self.record_request(json!({"messages":messages}), "composition-context-v1")?;
        self.refresh_composition_catalog(r.id, max_bytes)
    }

    fn projection_messages(&self, p: &Projection) -> Result<Vec<Value>> {
        p.entries
            .iter()
            .map(|e| serde_json::from_slice(&self.store.source(&e.source)?).map_err(integrity))
            .collect()
    }

    /// Preflight relevance with the same validator. Only resolve_queued_composition
    /// publishes a decision, against the complete final provider template.
    pub fn preview_queued_composition(&mut self) -> Result<Vec<Value>> {
        let id = self
            .composition
            .pending
            .first()
            .ok_or_else(|| Error::Proposal("no pending proposal".into()))?;
        let q: Queued = self.store.get(id)?;
        let old: Catalog = self.store.get(&q.catalog)?;
        if q.proposal.expected_head != old.expected_head {
            return Err(Error::Proposal("stale expected head".into()));
        }
        let request = self
            .composition
            .latest_request
            .ok_or_else(|| integrity("request absent"))?;
        let policy: Policy = self.store.get(&old.policy)?;
        let carried = self.carry_catalog(q.catalog, request, policy)?;
        let c: Catalog = self.store.get(&carried)?;
        let mut proposal = q.proposal;
        proposal.expected_head = c.expected_head;
        let view = self
            .composition_candidate(&c, &proposal)?
            .map_err(|reason| Error::Proposal(format!("composition refused: {reason:?}")))?;
        self.projection_messages(&view)
    }

    /// Expand only an exact preflight prefix; the final host template is retained.
    /// This lets the actual request receive its own decision and byte commitment.
    pub fn expand_composition_preflight(&self, messages: &[Value]) -> Result<Vec<Value>> {
        let Some(id) = self.composition.pending.first() else {
            return Ok(messages.to_vec());
        };
        let q: Queued = self.store.get(id)?;
        let c: Catalog = self.store.get(&q.catalog)?;
        let before = self.projection_messages(&self.checked_composition_projection(c.before)?)?;
        if messages.starts_with(&before) {
            return Ok(messages.to_vec());
        }
        // Reconstruct the proposed order without accepting it. Final admission
        // still checks current pins, tools, scope, head and full rendered bytes.
        let universe = self.checked_composition_projection(c.universe)?;
        let mut ids = self
            .checked_composition_projection(c.before)?
            .entries
            .iter()
            .map(|e| e.event)
            .collect::<BTreeSet<_>>();
        for change in &q.proposal.changes {
            match change.action {
                Action::Include => {
                    ids.insert(change.occurrence);
                }
                Action::Park => {
                    ids.remove(&change.occurrence);
                }
                Action::Summarise => return Ok(messages.to_vec()),
            }
        }
        let mut view = universe.clone();
        view.entries.retain(|e| ids.contains(&e.event));
        let preview = self.projection_messages(&view)?;
        if messages.starts_with(&preview) {
            let mut result = before;
            result.extend_from_slice(&messages[preview.len()..]);
            Ok(result)
        } else {
            Ok(messages.to_vec())
        }
    }

    /// Carry the last dispatched view into the host's next conversation vector.
    /// Matching ordered prefixes preserves repeated-text occurrence distinctions.
    pub fn normalize_composition_messages(&self, messages: &mut Vec<Value>) -> Result<()> {
        let Some(d) = self
            .composition
            .head
            .and_then(|id| self.composition.decisions.get(&id))
        else {
            return Ok(());
        };
        let Some(after) = d.after else {
            return Ok(());
        };
        let before = self.projection_messages(&self.checked_composition_projection(d.before)?)?;
        if messages.starts_with(&before) {
            let mut selected =
                self.projection_messages(&self.checked_composition_projection(after)?)?;
            selected.extend_from_slice(&messages[before.len()..]);
            *messages = selected;
        }
        Ok(())
    }
}

impl Session {
    pub fn refuse_pending_composition(&mut self, max_bytes: usize) -> Result<Vec<Receipt>> {
        let request = self
            .composition
            .latest_request
            .ok_or_else(|| integrity("request absent"))?;
        let id = self
            .composition
            .pending
            .first()
            .ok_or_else(|| Error::Proposal("no queued proposal".into()))?;
        let q: Queued = self.store.get(id)?;
        let catalog: Catalog = self.store.get(&q.catalog)?;
        let policy: Policy = self.store.get(&catalog.policy)?;
        self.resolve_queued_composition(request, max_bytes.min(policy.max_bytes))
    }
}

impl Session {
    /// Catalog pin cards are host constraints, never derivation evidence. Only
    /// admitted non-generated source occurrences support the auxiliary prompt.
    pub fn record_composition_navigation(
        &mut self,
        catalog: ContentId,
        prompt: &str,
    ) -> Result<ContentId> {
        if !self.composition.catalogs.contains(&catalog) {
            return Err(Error::Access("catalog absent".into()));
        }
        let c: Catalog = self.store.get(&catalog)?;
        let candidates = c
            .entries
            .iter()
            .filter(|e| {
                self.events
                    .get(&e.event)
                    .is_some_and(|event| !is_generated(event))
            })
            .map(|e| json!({"cid":e.event.to_string()}))
            .collect::<Vec<_>>();
        self.record_navigation_request(&json!({"candidates":candidates}), prompt)
    }
}
