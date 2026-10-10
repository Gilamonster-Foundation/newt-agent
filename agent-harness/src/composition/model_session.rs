//! Model inputs stay pending until a closed, host-prepared request boundary.
use super::*;
use crate::composition::{Card, Page, Queued};

impl Session {
    pub fn composition_page(
        &mut self,
        catalog: ContentId,
        offset: usize,
        limit: usize,
        max_bytes: usize,
    ) -> Result<(ContentId, Page)> {
        self.navigation_work(|s| {
            s.ensure_writer()?;
            s.check_navigation()?;
            if limit == 0
                || limit > s.config.max_catalog_entries
                || max_bytes == 0
                || max_bytes > s.config.max_slice_bytes
                || s.dereferences >= s.config.max_dereferences
            {
                return Err(Error::Budget("composition page bounds".into()));
            }
            let mut page = s.make_composition_page(catalog, offset, limit)?;
            loop {
                let size = serde_json::to_vec(&page).map_err(integrity)?.len();
                if size <= max_bytes {
                    if s.fetched.saturating_add(size) > s.config.max_fetched_bytes {
                        return Err(Error::Budget("composition catalog bytes".into()));
                    }
                    let id = s.store.put(&page)?;
                    s.append(JournalEntry::CompositionPage { page: id })?;
                    s.fetched += size;
                    s.dereferences += 1;
                    return Ok((id, page));
                }
                if page.cards.len() <= 1 {
                    return Err(Error::Budget("composition page cannot fit one card".into()));
                }
                page.cards.pop();
                page.next_offset = Some(offset + page.cards.len());
            }
        })
    }

    fn make_composition_page(
        &self,
        catalog: ContentId,
        offset: usize,
        limit: usize,
    ) -> Result<Page> {
        if !self.composition.catalogs.contains(&catalog) {
            return Err(Error::Access("catalog outside session".into()));
        }
        let c: Catalog = self.store.get(&catalog)?;
        let before = self.checked_composition_projection(c.before)?;
        let universe = self.checked_composition_projection(c.universe)?;
        if offset >= universe.entries.len() || limit == 0 {
            return Err(Error::Proposal("catalog offset outside inventory".into()));
        }
        let messages = universe
            .entries
            .iter()
            .map(|e| serde_json::from_slice(&self.store.source(&e.source)?).map_err(integrity))
            .collect::<Result<Vec<Value>>>()?;
        let candidates = self.candidates(&messages, &universe.entries)?;
        let end = offset.saturating_add(limit).min(universe.entries.len());
        let cards = (offset..end)
            .map(|i| Card {
                cid: universe.entries[i].event.to_string(),
                parked: !before.entries.contains(&universe.entries[i]),
                required: candidates[i].required,
                bytes: candidates[i].bytes,
                preview: messages[i].to_string().chars().take(96).collect(),
            })
            .collect();
        Ok(Page {
            catalog,
            expected_head: c.expected_head,
            offset,
            next_offset: (end < universe.entries.len()).then_some(end),
            cards,
        })
    }

    pub(in crate::session) fn apply_composition_page(&mut self, id: ContentId) -> Result<()> {
        let page: Page = self.store.get(&id)?;
        if page.cards.len() > self.config.max_catalog_entries
            || page != self.make_composition_page(page.catalog, page.offset, page.cards.len())?
        {
            return Err(integrity("substituted catalog page"));
        }
        self.composition
            .pages
            .entry(page.catalog)
            .or_default()
            .insert(id);
        Ok(())
    }

    /// Retain a proposal during a live tool call. Acceptance is deferred and the
    /// returned CID is only a queue receipt, never a claim that the view changed.
    pub fn queue_composition(
        &mut self,
        catalog: ContentId,
        actor: Actor,
        proposal: Proposal,
    ) -> Result<ContentId> {
        self.ensure_writer()?;
        self.check_navigation()?;
        if !self.composition.pending.is_empty()
            || self.composition.attempts > self.config.max_retries
        {
            return Err(Error::Budget("composition pending proposals".into()));
        }
        let queued = Queued {
            catalog,
            actor,
            proposal,
        };
        let id = self.store.put(&queued)?;
        self.check_composition_queue(id)?;
        self.append(JournalEntry::CompositionQueued { queued: id })?;
        Ok(id)
    }

    pub(in crate::session) fn apply_composition_queue(&mut self, id: ContentId) -> Result<()> {
        self.check_composition_queue(id)?;
        self.composition.pending.push(id);
        self.composition.attempts += 1;
        Ok(())
    }
    fn check_composition_queue(&self, id: ContentId) -> Result<()> {
        let q: Queued = self.store.get(&id)?;
        if !self.composition.catalogs.contains(&q.catalog)
            || self.composition.pending.contains(&id)
            || q.actor.model.is_empty()
            || q.actor.harness.is_empty()
            || q.actor.model.len() > 4096
            || q.actor.harness.len() > 4096
            || q.proposal.changes.len() > self.config.max_catalog_entries
            || q.proposal.inverse.is_some()
        {
            return Err(Error::Proposal("invalid model composition proposal".into()));
        }
        let offered = self
            .composition
            .pages
            .get(&q.catalog)
            .into_iter()
            .flatten()
            .map(|id| self.store.get::<Page>(id))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|p| p.catalog == q.catalog)
            .flat_map(|p| p.cards.into_iter().map(|c| c.cid))
            .collect::<BTreeSet<_>>();
        if q.proposal.changes.iter().any(|c| {
            !offered.contains(&c.occurrence.to_string())
                || c.reason.trim().is_empty()
                || c.reason.len() > 4096
        }) {
            return Err(Error::Proposal(
                "proposal requires offered occurrences and bounded reasons".into(),
            ));
        }
        Ok(())
    }
}
