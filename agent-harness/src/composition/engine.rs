//! Host-only composition transactions, using the existing journal/checkpoint.
use super::*;
use crate::composition::{
    self, Action, Actor, Catalog, Decision, Elision, Outcome, Policy, Proposal, Receipt, Refusal,
    Submission,
};

#[path = "candidate.rs"]
mod candidate;
#[path = "model_carry.rs"]
mod model_carry;
#[path = "model_preflight.rs"]
mod model_preflight;
#[path = "model_session.rs"]
mod model_session;
use crate::composition::Queued;
#[cfg(test)]
#[path = "transaction_tests.rs"]
mod transaction_tests;
#[path = "verify.rs"]
mod verify;

impl Session {
    /// Capture a host catalog of admitted occurrences. This is not a model tool.
    pub fn composition_catalog(
        &mut self,
        request: ContentId,
        policy: Policy,
    ) -> Result<(ContentId, Catalog)> {
        self.ensure_writer()?;
        self.ensure_tools_closed()?;
        if self.composition.latest_request != Some(request) || policy.max_bytes == 0 {
            return Err(Error::Proposal(
                "composition needs the current request and a positive budget".into(),
            ));
        }
        let record: RequestRecord = self.store.get(&request)?;
        let (before, universe) = match self.active_composition() {
            Some(d) => {
                let old: Catalog = self.store.get(&d.catalog)?;
                (
                    d.after.ok_or_else(|| integrity("accepted view absent"))?,
                    old.universe,
                )
            }
            None => (record.projection, record.projection),
        };
        let view = self.checked_composition_projection(before)?;
        if view.entries.iter().map(|e| e.event).collect::<Vec<_>>() != self.transcript {
            return Err(Error::Proposal(
                "record the current host context before composing".into(),
            ));
        }
        let inventory = self.checked_composition_projection(universe)?;
        if inventory.entries.len() > self.config.max_catalog_entries {
            return Err(Error::Budget("composition catalog entries".into()));
        }
        self.require_semantic_pins(&inventory.entries)?;
        let policy = self.store.put(&policy)?;
        let pins = self
            .semantic_pins
            .as_ref()
            .map(|p| self.store.put(p))
            .transpose()?;
        let catalog = Catalog {
            offered: None,
            run: self.run,
            root: self.root,
            request,
            expected_head: self.composition.head,
            before,
            universe,
            policy,
            context_revision: self.composition.context_revision,
            pin_revision: self.composition.pin_revision,
            pins,
            entries: inventory.entries,
        };
        let cid = self.store.put(&catalog)?;
        self.append(JournalEntry::CompositionCatalog { catalog: cid })?;
        Ok((cid, catalog))
    }

    /// Retain the exact typed proposal before judging its relevance or legality.
    /// Actor metadata describes the proposer; it never grants execution authority.
    pub fn record_composition_proposal(
        &mut self,
        catalog: ContentId,
        actor: Actor,
        proposal: Proposal,
    ) -> Result<ContentId> {
        self.submit_composition(catalog, actor, proposal, None)
    }

    pub(super) fn submit_composition(
        &mut self,
        catalog: ContentId,
        actor: Actor,
        proposal: Proposal,
        queued: Option<ContentId>,
    ) -> Result<ContentId> {
        self.ensure_writer()?;
        self.ensure_tools_closed()?;
        if !self.composition.catalogs.contains(&catalog) {
            return Err(Error::Access("catalog is outside this session".into()));
        }
        if actor.model.is_empty()
            || actor.harness.is_empty()
            || actor.model.len() > 4096
            || actor.harness.len() > 4096
        {
            return Err(Error::Proposal("invalid proposer manifest".into()));
        }
        self.check_navigation()?;
        let actor = self.store.put(&actor)?;
        let submission = Submission {
            queued,
            catalog,
            actor,
            proposal,
        };
        let bytes = submission.canonical_form().map_err(integrity)?;
        let context: Catalog = self.store.get(&catalog)?;
        let parents = self
            .checked_composition_projection(context.before)?
            .entries
            .iter()
            .map(|e| e.event)
            .collect();
        let event = self.event(
            EventOrigin::Model,
            EventKind::Observation,
            &bytes,
            parents,
            BTreeSet::new(),
            0,
        )?;
        self.append(JournalEntry::CompositionProposed { event })?;
        Ok(event)
    }

    /// Stage a candidate, then publish ONE checkpoint. A refusal is also durable
    /// but never installs a candidate view. Integrity/storage errors stop dispatch.
    pub fn decide_composition(&mut self, proposal: ContentId) -> Result<Receipt> {
        self.ensure_writer()?;
        self.ensure_tools_closed()?;
        let result = self.decide_composition_inner(proposal);
        if matches!(result, Err(Error::Storage(_) | Error::Integrity(_))) {
            self.aborted = true;
        }
        result
    }

    fn decide_composition_inner(&mut self, proposal: ContentId) -> Result<Receipt> {
        let id = self.stage_composition(proposal)?;
        self.publish_composition(id)?;
        let d = &self.composition.decisions[&id];
        Ok(Receipt {
            decision: id,
            outcome: d.outcome.clone(),
            projection: d.after.unwrap_or(d.before),
        })
    }

    fn stage_composition(&mut self, proposal: ContentId) -> Result<ContentId> {
        if !self.composition.proposals.contains(&proposal)
            || self
                .composition
                .decisions
                .values()
                .any(|d| d.proposal == proposal)
        {
            return Err(Error::Proposal("proposal absent or already decided".into()));
        }
        let submission = self.composition_submission(proposal)?;
        let catalog: Catalog = self.store.get(&submission.catalog)?;
        let before = self.composition_current_projection()?;
        let candidate = self.composition_candidate(&catalog, &submission.proposal)?;
        let mut elisions = Vec::new();
        let (after, outcome, bytes) = match candidate {
            Ok(view) => {
                let bytes = view.render(&self.store)?.len();
                let old = self.checked_composition_projection(before)?;
                for entry in &old.entries {
                    if !view.entries.iter().any(|e| e.event == entry.event) {
                        let event = &self.events[&entry.event];
                        let source = self.store.source(&entry.source)?;
                        let unit = Unit::seal(Op::Elide, &source, entry.span, event.body().root)
                            .map_err(integrity)?;
                        elisions.push(Elision {
                            occurrence: entry.event,
                            unit: self.store.put(&unit)?,
                        });
                    }
                }
                (Some(self.store.put(&view)?), Outcome::Accepted, bytes)
            }
            Err(code) => (
                None,
                Outcome::Refused(code),
                self.checked_composition_projection(before)?
                    .render(&self.store)?
                    .len(),
            ),
        };
        let decision = Decision {
            queued: submission.queued,
            version: 1,
            run: self.run,
            root: self.root,
            expected_head: self.composition.head,
            catalog: submission.catalog,
            proposal,
            actor: submission.actor,
            objective_ref: self.semantic_pins.as_ref().map(|p| p.objective.clone()),
            context_revision: self.composition.context_revision,
            pin_revision: self.composition.pin_revision,
            pins: composition::pin_identity(&self.semantic_pins)?,
            policy: catalog.policy,
            before,
            after,
            outcome: outcome.clone(),
            elisions,
            bytes,
        };
        self.store.put(&decision)
    }

    fn publish_composition(&mut self, id: ContentId) -> Result<()> {
        if self.journal_len >= self.config.max_history_nodes {
            return Err(Error::Budget("session journal".into()));
        }
        // Validation reads the complete closure, before either live-view mutation
        // or head publication. Orphan staged objects convey no admission rights.
        let checked = self.verify_composition(id)?;
        let head = self.store.put(&MerkleNode::new(
            JournalEntry::Composition { decision: id },
            [self.head],
        ))?;
        if let Err(e) = self.store.publish_head(&self.writer, Some(self.head), head) {
            self.aborted = true;
            return Err(e);
        }
        self.install_composition(id, checked);
        self.head = head;
        self.journal_len += 1;
        Ok(())
    }

    /// Record the accepted projection through its original provider renderer.
    /// The returned bytes carry the decision binding; this does not send them.
    pub fn record_composed_request(&mut self) -> Result<PreparedRequest> {
        self.ensure_writer()?;
        self.ensure_tools_closed()?;
        let d = self
            .active_composition()
            .ok_or_else(|| Error::Proposal("no current accepted composition".into()))?;
        let view = self.checked_composition_projection(
            d.after.ok_or_else(|| integrity("accepted view absent"))?,
        )?;
        let latest = self
            .composition
            .latest_request
            .ok_or_else(|| integrity("request absent"))?;
        let format = self.store.get::<RequestRecord>(&latest)?.format;
        let body =
            serde_json::from_slice(&self.store.source(&view.template)?).map_err(integrity)?;
        let messages = view
            .entries
            .iter()
            .map(|e| serde_json::from_slice(&self.store.source(&e.source)?).map_err(integrity))
            .collect::<Result<Vec<Value>>>()?;
        self.prepare_request(body, &format, &view.field, &messages, &view.renderer)
    }

    pub fn composition_head(&self) -> Result<Option<ContentId>> {
        self.ensure_active()?;
        Ok(self.composition.head)
    }
    pub fn composition_decision(&self, id: ContentId) -> Result<Decision> {
        self.ensure_active()?;
        if !self.composition.decisions.contains_key(&id) {
            return Err(Error::Access("decision is outside this session".into()));
        }
        self.store.get(&id)
    }
    /// Render the current accepted view, without recording or sending a request.
    pub fn composition_bytes(&self) -> Result<Vec<u8>> {
        self.ensure_active()?;
        self.checked_composition_projection(self.composition_current_projection()?)?
            .render(&self.store)
    }
}
