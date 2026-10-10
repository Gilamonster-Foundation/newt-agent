//! Primary tools and pressure-triggered auxiliary proposals share one admission path.
use super::*;
use agent_harness::composition::{Action, Actor, Change, Proposal};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    catalog: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    limit: Option<usize>,
    max_bytes: Option<usize>,
    expected_head: Option<String>,
    changes: Option<Vec<WireChange>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireChange {
    cid: String,
    action: WireAction,
    reason: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireAction {
    Include,
    Park,
    Summarise,
}
fn page_limit() -> usize {
    16
}
fn page_bytes() -> usize {
    4096
}

impl SmartHarness {
    pub(crate) fn normalize_composition(&self, messages: &mut Vec<Value>) -> anyhow::Result<()> {
        if self.settings.composition_enabled {
            self.state()?
                .session
                .normalize_composition_messages(messages)?;
        }
        Ok(())
    }

    pub(crate) fn propose_context(&self, args: &Value) -> anyhow::Result<String> {
        anyhow::ensure!(
            self.settings.composition_enabled,
            "context composition is disabled"
        );
        let mut state = self.state()?;
        self.composition_operation(&mut state, args, "primary")
    }

    fn composition_operation(
        &self,
        state: &mut State,
        args: &Value,
        proposer: &str,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(
            serde_json::to_vec(args)?.len() <= self.settings.max_output_bytes,
            "composition input exceeds its byte budget"
        );
        let input: Input = serde_json::from_value(args.clone())?;
        let offset = input.offset.unwrap_or(0);
        let limit = input.limit.unwrap_or_else(page_limit);
        let max_bytes = input.max_bytes.unwrap_or_else(page_bytes);
        let catalog = input
            .catalog
            .as_deref()
            .map(str::parse)
            .transpose()?
            .or_else(|| state.session.latest_composition_catalog())
            .ok_or_else(|| anyhow::anyhow!("no offered catalog"))?;
        if let Some(changes) = input.changes {
            anyhow::ensure!(
                input.catalog.is_some() && offset == 0,
                "proposal requires a catalog and no page offset"
            );
            let changes = changes
                .into_iter()
                .map(|c| {
                    Ok(Change {
                        occurrence: c.cid.parse()?,
                        action: match c.action {
                            WireAction::Include => Action::Include,
                            WireAction::Park => Action::Park,
                            WireAction::Summarise => Action::Summarise,
                        },
                        reason: c.reason,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let expected_head = input.expected_head.as_deref().map(str::parse).transpose()?;
            let model = if proposer == "auxiliary" {
                state.session.config().auxiliary["model"]
                    .as_str()
                    .unwrap_or("unknown auxiliary")
            } else {
                &state.primary_model
            };
            let actor = Actor {
                model: model.into(),
                harness: format!("newt-agent {}", env!("CARGO_PKG_VERSION")),
            };
            let queued = state.session.queue_composition(
                catalog,
                actor,
                Proposal {
                    expected_head,
                    changes,
                    inverse: None,
                },
            )?;
            Ok(serde_json::json!({"queued":queued.to_string(),"status":"pending host validation at the next request boundary"}).to_string())
        } else {
            anyhow::ensure!(
                limit <= 32 && max_bytes <= 4096 && input.expected_head.is_none(),
                "catalog page bounds"
            );
            let (_, page) = state
                .session
                .composition_page(catalog, offset, limit, max_bytes)?;
            Ok(serde_json::to_string(&page)?)
        }
    }

    pub(super) fn preview_composition(&self, state: &mut State) -> anyhow::Result<Vec<Value>> {
        match state.session.preview_queued_composition() {
            Ok(messages) => Ok(messages),
            Err(error) => {
                // Keep the typed refusal, then stop instead of silently restoring
                // a legacy selection or using a generated summary.
                state
                    .session
                    .refuse_pending_composition(self.settings.composition_max_bytes)?;
                Err(anyhow::anyhow!("context composition refused: {error}"))
            }
        }
    }

    pub(super) fn composition_request(
        &self,
        state: &mut State,
        body: &Value,
        format: &str,
        canonical: Option<&[Value]>,
    ) -> anyhow::Result<agent_harness::PreparedRequest> {
        if !self.settings.composition_enabled {
            return Ok(match canonical {
                Some(messages) => {
                    state
                        .session
                        .record_rendered_request(body.clone(), format, messages)?
                }
                None => state.session.record_request(body.clone(), format)?,
            });
        }
        state.primary_model = body["model"]
            .as_str()
            .unwrap_or("unknown primary")
            .to_owned();
        let field = if body.get("input").is_some() {
            "input"
        } else {
            "messages"
        };
        let owned;
        let messages = match canonical {
            Some(m) => m,
            None => {
                owned = match body.get(field) {
                    Some(Value::Array(m)) => m.clone(),
                    Some(Value::String(s)) => vec![serde_json::json!({"role":"user","content":s})],
                    _ => anyhow::bail!("request has no messages"),
                };
                &owned
            }
        };
        let expanded = state.session.expand_composition_preflight(messages)?;
        state.session.record_messages(&expanded)?;
        let original = if canonical.is_some() {
            state
                .session
                .record_rendered_request(body.clone(), format, &expanded)?
        } else {
            let mut full = body.clone();
            full[field] = Value::Array(expanded);
            state.session.record_request(full, format)?
        };
        let decisions = state
            .session
            .resolve_queued_composition(original.id, self.settings.composition_max_bytes)?;
        anyhow::ensure!(
            decisions
                .iter()
                .all(|r| r.outcome == agent_harness::composition::Outcome::Accepted),
            "context composition refused; request not dispatched: {decisions:?}"
        );
        let result = if decisions.is_empty() {
            original
        } else {
            state.session.record_composed_request()?
        };
        state
            .session
            .refresh_composition_catalog(result.id, self.settings.composition_max_bytes)?;
        Ok(result)
    }

    pub(super) async fn project_composition(
        &self,
        messages: &[Value],
        max_bytes: usize,
    ) -> anyhow::Result<Vec<Value>> {
        let (request, prompt) = {
            let mut state = self.state()?;
            if state.session.has_pending_composition() {
                return self.preview_composition(&mut state);
            }
            let catalog = state.session.composition_context(
                messages,
                max_bytes.min(self.settings.composition_max_bytes),
            )?;
            let mut pages = Vec::new();
            let mut offset = 0;
            loop {
                let args = serde_json::json!({"catalog":catalog.to_string(),"offset":offset});
                let page: Value = serde_json::from_str(&self.composition_operation(
                    &mut state,
                    &args,
                    "auxiliary",
                )?)?;
                let next = page["next_offset"].as_u64();
                pages.push(page);
                anyhow::ensure!(
                    serde_json::to_vec(&pages)?.len() <= self.settings.max_input_bytes,
                    "composition catalog exceeds auxiliary input budget"
                );
                match next {
                    Some(n) => offset = usize::try_from(n)?,
                    None => break,
                }
            }
            let prompt=format!("Return only a propose_context JSON object with catalog, expected_head, and changes [{{cid,action:include|park,reason}}]. Select relevance; keep required cards and whole tool exchanges. summarise is unsupported. Request budget: {max_bytes} bytes. Catalog pages:\n{}",serde_json::to_string(&pages)?);
            (
                state
                    .session
                    .record_composition_navigation(catalog, &prompt)?,
                prompt,
            )
        };
        let completion = {
            let mut timer = NavigationTimer {
                harness: self,
                started: Instant::now(),
                pending: Some(request),
            };
            let result = self.complete_bounded(prompt).await;
            timer.pending = None;
            result
        };
        let mut state = self.state()?;
        let raw = match completion {
            Ok(raw) => raw,
            Err(e) => {
                state
                    .session
                    .record_navigation_failure(request, &e.to_string())?;
                return Err(e.context("context composition auxiliary failed"));
            }
        };
        state.session.record_navigation_reply(request, &raw)?;
        let result = (|| {
            self.admit_output(&raw)?;
            let args: Value =
                serde_json::from_str(super::super::adjudicate::strip_code_fence(&raw))?;
            anyhow::ensure!(
                args.get("changes").is_some(),
                "auxiliary must propose changes"
            );
            self.composition_operation(&mut state, &args, "auxiliary")?;
            self.preview_composition(&mut state)
        })();
        if let Err(error) = &result {
            state
                .session
                .record_navigation_failure(request, &format!("{error:#}"))?;
        }
        result
    }
}

#[cfg(test)]
#[path = "context_composition_tests.rs"]
mod tests;
