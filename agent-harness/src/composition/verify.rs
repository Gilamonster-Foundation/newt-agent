//! The same verifier admits newly staged decisions and restored checkpoints.
use super::*;

pub(in crate::session) struct Checked {
    decision: Decision,
    view: Option<Projection>,
    units: Vec<(ContentId, Unit)>,
}

impl Session {
    pub(in crate::session) fn active_composition(&self) -> Option<&Decision> {
        self.composition
            .active
            .and_then(|id| self.composition.decisions.get(&id))
            .filter(|d| {
                d.root == self.root
                    && d.pin_revision == self.composition.pin_revision
                    && d.context_revision == self.composition.context_revision
            })
    }
    pub(super) fn composition_current_projection(&self) -> Result<ContentId> {
        match self.active_composition() {
            Some(d) => d
                .after
                .ok_or_else(|| integrity("accepted composition has no view")),
            None => {
                let id = self.composition.latest_request.ok_or_else(|| {
                    Error::Proposal("composition requires a recorded request".into())
                })?;
                Ok(self.store.get::<RequestRecord>(&id)?.projection)
            }
        }
    }
    pub(super) fn checked_composition_projection(&self, id: ContentId) -> Result<Projection> {
        let p: Projection = self.store.get(&id)?;
        let mut seen = BTreeSet::new();
        for entry in &p.entries {
            let admitted = self
                .events
                .get(&entry.event)
                .ok_or_else(|| integrity("composition source is not admitted"))?;
            let raw: RawEvent = self.store.get(&entry.event)?;
            let event = Event::admit(raw, |id| self.events.get(id).cloned()).map_err(integrity)?;
            let root: RootEvent = self.store.get(&event.body().root)?;
            self.store.source(&root.content)?;
            if &event != admitted
                || entry.source != event.body().payload
                || !seen.insert(entry.event)
            {
                return Err(integrity(
                    "composition substitutes or repeats an occurrence",
                ));
            }
            let bytes = self.store.source(&entry.source)?;
            if entry.span != Span::new(0, bytes.len() as u64) {
                return Err(integrity(
                    "composition requires complete message occurrences",
                ));
            }
        }
        p.render(&self.store)?;
        Ok(p)
    }
    pub(in crate::session) fn apply_composition_catalog(&mut self, id: ContentId) -> Result<()> {
        self.ensure_tools_closed()?;
        let c: Catalog = self.store.get(&id)?;
        let policy: Policy = self.store.get(&c.policy)?;
        let before = self.checked_composition_projection(c.before)?;
        let universe = self.checked_composition_projection(c.universe)?;
        let expected_universe = match self.active_composition() {
            Some(d) => self.store.get::<Catalog>(&d.catalog)?.universe,
            None => c.before,
        };
        if c.run != self.run
            || c.root != self.root
            || c.expected_head != self.composition.head
            || self.composition.latest_request != Some(c.request)
            || c.before != self.composition_current_projection()?
            || !self.projections.contains(&c.before)
            || c.universe != expected_universe
            || before.entries.iter().map(|e| e.event).collect::<Vec<_>>() != self.transcript
            || c.entries != universe.entries
            || c.entries.len() > self.config.max_catalog_entries
            || c.context_revision != self.composition.context_revision
            || c.pin_revision != self.composition.pin_revision
            || c.pins != composition::pin_identity(&self.semantic_pins)?
            || policy.max_bytes == 0
            || before.template != universe.template
            || before.renderer != universe.renderer
            || before.field != universe.field
        {
            return Err(integrity("invalid composition catalog"));
        }
        if let Some(id) = c.pins {
            let pins: composition::Pins = self.store.get(&id)?;
            if Some(&pins) != self.semantic_pins.as_ref() {
                return Err(integrity("catalog pin substitution"));
            }
        }
        self.require_semantic_pins(&before.entries)?;
        self.composition.catalogs.insert(id);
        Ok(())
    }
    pub(in crate::session) fn apply_composition_proposal(&mut self, id: ContentId) -> Result<()> {
        self.ensure_tools_closed()?;
        let event = self.checked_event(id)?;
        let s: Submission = content_addressable::canonical::from_canonical_dagcbor_checked(
            &self.store.source(&event.body().payload)?,
        )
        .map_err(integrity)?;
        let actor: Actor = self.store.get(&s.actor)?;
        let c: Catalog = self.store.get(&s.catalog)?;
        let before = self.checked_composition_projection(c.before)?;
        if !self.composition.catalogs.contains(&s.catalog)
            || actor.model.is_empty()
            || actor.harness.is_empty()
            || actor.model.len() > 4096
            || actor.harness.len() > 4096
            || event.body().origin != EventOrigin::Model
            || event.body().kind != EventKind::Observation
            || !event.body().sources.is_empty()
            || event.parents() != &before.entries.iter().map(|e| e.event).collect()
        {
            return Err(integrity("invalid composition proposal attribution"));
        }
        self.events.insert(id, event);
        self.composition.proposals.insert(id);
        Ok(())
    }
    pub(super) fn composition_submission(&self, id: ContentId) -> Result<Submission> {
        if !self.composition.proposals.contains(&id) {
            return Err(integrity("unadmitted composition proposal"));
        }
        let raw: RawEvent = self.store.get(&id)?;
        let event = self
            .events
            .get(&id)
            .ok_or_else(|| integrity("proposal event absent"))?;
        if raw.content_id().map_err(integrity)? != event.content_id().map_err(integrity)? {
            return Err(integrity("proposal substitution"));
        }
        let s: Submission = content_addressable::canonical::from_canonical_dagcbor_checked(
            &self.store.source(&event.body().payload)?,
        )
        .map_err(integrity)?;
        self.store.get::<Actor>(&s.actor)?;
        Ok(s)
    }

    pub(in crate::session) fn verify_composition(&self, id: ContentId) -> Result<Checked> {
        self.ensure_tools_closed()?;
        let d: Decision = self.store.get(&id)?;
        let s = self.composition_submission(d.proposal)?;
        let c: Catalog = self.store.get(&s.catalog)?;
        if d.version != 1
            || d.run != self.run
            || d.root != self.root
            || d.expected_head != self.composition.head
            || d.catalog != s.catalog
            || d.actor != s.actor
            || d.policy != c.policy
            || d.before != self.composition_current_projection()?
            || d.context_revision != self.composition.context_revision
            || d.pin_revision != self.composition.pin_revision
            || d.pins != composition::pin_identity(&self.semantic_pins)?
            || d.objective_ref != self.semantic_pins.as_ref().map(|p| p.objective.clone())
            || self
                .composition
                .decisions
                .values()
                .any(|old| old.proposal == d.proposal)
        {
            return Err(integrity("composition decision context mismatch"));
        }
        let candidate = self.composition_candidate(&c, &s.proposal)?;
        let before = self.checked_composition_projection(d.before)?;
        let mut units = Vec::new();
        let view = match candidate {
            Ok(expected) => {
                let after = d
                    .after
                    .ok_or_else(|| integrity("accepted decision lacks its projection"))?;
                let actual = self.checked_composition_projection(after)?;
                if d.outcome != Outcome::Accepted
                    || actual != expected
                    || d.bytes != actual.render(&self.store)?.len()
                {
                    return Err(integrity("composition candidate or budget was substituted"));
                }
                let parked: Vec<_> = before
                    .entries
                    .iter()
                    .filter(|e| !actual.entries.iter().any(|v| e.event == v.event))
                    .collect();
                if parked.len() != d.elisions.len() {
                    return Err(integrity("composition elision closure differs"));
                }
                for (entry, receipt) in parked.iter().zip(&d.elisions) {
                    let unit = self.store.unit(receipt.unit)?;
                    let expected = Unit::seal(
                        Op::Elide,
                        &self.store.source(&entry.source)?,
                        entry.span,
                        self.events[&entry.event].body().root,
                    )
                    .map_err(integrity)?;
                    if receipt.occurrence != entry.event
                        || unit.id().map_err(integrity)? != expected.id().map_err(integrity)?
                    {
                        return Err(integrity(
                            "composition elision is not bound to its occurrence",
                        ));
                    }
                    units.push((receipt.unit, unit));
                }
                Some(actual)
            }
            Err(code) => {
                if d.outcome != Outcome::Refused(code)
                    || d.after.is_some()
                    || !d.elisions.is_empty()
                    || d.bytes != before.render(&self.store)?.len()
                {
                    return Err(integrity(
                        "refusal mutated the view or misreported its cause",
                    ));
                }
                None
            }
        };
        Ok(Checked {
            decision: d,
            view,
            units,
        })
    }
    pub(in crate::session) fn install_composition(&mut self, id: ContentId, checked: Checked) {
        let Checked {
            decision,
            view,
            units,
        } = checked;
        if let Some(view) = view {
            self.transcript = view.entries.iter().map(|e| e.event).collect();
            self.restored_transcript = self.transcript.clone();
            self.projections.extend(decision.after);
            self.units.extend(units);
            self.composition.active = Some(id);
        }
        self.composition.head = Some(id);
        self.composition.decisions.insert(id, decision);
    }
    pub(in crate::session) fn composition_entries(
        &self,
        messages: &[Value],
    ) -> Result<Option<Vec<Entry>>> {
        let Some(d) = self.active_composition() else {
            return Ok(None);
        };
        let view = self.checked_composition_projection(
            d.after.ok_or_else(|| integrity("accepted view absent"))?,
        )?;
        let expected = view
            .entries
            .iter()
            .map(|e| serde_json::from_slice(&self.store.source(&e.source)?).map_err(integrity))
            .collect::<Result<Vec<Value>>>()?;
        if expected != messages {
            return Err(Error::Proposal(
                "request differs from accepted composition".into(),
            ));
        }
        Ok(Some(view.entries))
    }

    pub(in crate::session) fn composition_binding(
        &self,
        projection: ContentId,
    ) -> Result<Option<ContentId>> {
        let Some(d) = self.active_composition() else {
            return Ok(None);
        };
        if d.after != Some(projection) {
            return Err(Error::Proposal(
                "request differs from accepted composition".into(),
            ));
        }
        Ok(self.composition.active)
    }
    pub(in crate::session) fn verify_composition_request(
        &self,
        record: &RequestRecord,
    ) -> Result<()> {
        if record.composition != self.composition_binding(record.projection)? {
            return Err(integrity("request composition binding mismatch"));
        }
        Ok(())
    }
}
