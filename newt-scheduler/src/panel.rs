//! panel.rs — the **diverse-panel layer** (anti-groupthink).
//!
//! Where the [crew](crate::run_crew) is *division of labor* (different roles on one
//! task), the panel is *decorrelation*: it fans the **same** task to N **diverse**
//! voices (each a pinned model / loadout) so no single model-family's blind spot
//! decides the answer. It then **verify-gates each candidate** and accepts the
//! **passers** — selecting by agreement. Objective and deterministic: there is no
//! subjective judge; ground truth (the injected [`Verify`]) breaks ties, and an
//! all-fail panel is an honest [`PanelStatus::NeedsHumanReview`], never a false
//! success.
//!
//! Pure orchestration over the same seams as the crew ([`BackendPool`] +
//! [`Dispatcher`]); the verify side is an injected trait, so the whole layer is
//! unit-testable with mocks and no network. Each voice runs under attenuated
//! `Caveats` (the Confused-Deputy containment) — modelled at the dispatch layer,
//! mocked here.

use crate::{BackendPool, ChatRequest, Dispatcher};
use newt_core::Tier;
use std::collections::HashMap;

/// One voice on the panel: a label + the model it is pinned to (the diversity axis).
#[derive(Debug, Clone)]
// INERT-CODE-RATCHET: X22 DELETE: panel and voting API is a closed tested island with no production caller.
pub struct VoiceSpec {
    pub name: String,
    pub model: String,
}

/// Which voices sit on the panel and at what backend tier they run.
#[derive(Debug, Clone)]
pub struct PanelConfig {
    pub voices: Vec<VoiceSpec>,
    pub tier: Tier,
}

/// One voice's vote: its candidate answer (if it could be reached) and whether
/// that candidate **passed verification**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vote {
    pub voice: String,
    pub passed: bool,
    pub candidate: Option<String>,
}

/// Terminal disposition of a panel run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelStatus {
    /// At least one voice's candidate passed verification.
    Accepted,
    /// No voice passed — escalate to a human. Never reported as success.
    NeedsHumanReview,
}

/// The result of a panel run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelOutcome {
    pub status: PanelStatus,
    /// The accepted answer (the most-agreed *passing* candidate), or `None`.
    pub accepted: Option<String>,
    /// Every voice's vote (for the ledger / inspection).
    pub votes: Vec<Vote>,
}

impl PanelOutcome {
    /// How many voices passed verification.
    #[must_use]
    pub fn passers(&self) -> usize {
        self.votes.iter().filter(|v| v.passed).count()
    }
}

/// The verify seam: does a voice's `candidate` pass the ground-truth check?
/// Injected so the panel stays pure and testable. A real impl applies the
/// candidate to an **isolated worktree** (one per voice — e.g. a `newt-git`
/// worktree) and runs the harness verification (`run_test` / the verify gate);
/// the mock decides directly.
pub trait Verify: Send + Sync {
    fn passes(&self, voice: &str, candidate: &str) -> bool;
}

/// Run the panel on `task`: each voice answers independently; keep the passers;
/// accept the passing candidate with the most agreement.
pub async fn run_panel(
    pool: &BackendPool,
    dispatcher: &dyn Dispatcher,
    verify: &dyn Verify,
    cfg: &PanelConfig,
    task: &str,
) -> PanelOutcome {
    let mut votes = Vec::with_capacity(cfg.voices.len());
    for voice in &cfg.voices {
        let req = ChatRequest::new()
            .system(
                "You are ONE independent voice on a review panel. Answer the task on your \
                 own; do not assume other voices agree with you.",
            )
            .user(task);
        let candidate = pool
            .run_role(dispatcher, cfg.tier, &voice.model, req)
            .await
            .map(|f| f.result.content);
        let passed = candidate
            .as_deref()
            .is_some_and(|c| verify.passes(&voice.name, c));
        votes.push(Vote {
            voice: voice.name.clone(),
            passed,
            candidate,
        });
    }

    let accepted = select_by_agreement(&votes);
    let status = if accepted.is_some() {
        PanelStatus::Accepted
    } else {
        PanelStatus::NeedsHumanReview
    };
    PanelOutcome {
        status,
        accepted,
        votes,
    }
}

/// Among the **passing** votes, the candidate answer with the most agreement
/// (ties broken by first-seen for determinism). `None` if nothing passed.
fn select_by_agreement(votes: &[Vote]) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for v in votes.iter().filter(|v| v.passed) {
        if let Some(c) = v.candidate.as_deref() {
            if counts
                .insert(c, counts.get(c).map_or(1, |n| n + 1))
                .is_none()
            {
                order.push(c);
            }
        }
    }
    order
        .into_iter()
        .max_by_key(|c| counts[c])
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatReply, Health, PoolBackend, StaticSource};
    use async_trait::async_trait;
    use newt_core::BackendKind;

    /// A dispatcher that returns a canned candidate per pinned model.
    struct VoiceMock {
        answers: HashMap<String, String>,
    }
    #[async_trait]
    impl Dispatcher for VoiceMock {
        async fn dispatch(
            &self,
            _b: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            match self.answers.get(model) {
                Some(a) => Ok(ChatReply {
                    content: a.clone(),
                    model_id: model.to_string(),
                    usage: None,
                }),
                None => anyhow::bail!("model {model} produced nothing"),
            }
        }
    }

    /// Verify passes iff the candidate contains the sentinel "GOOD".
    struct SentinelVerify;
    impl Verify for SentinelVerify {
        fn passes(&self, _voice: &str, candidate: &str) -> bool {
            candidate.contains("GOOD")
        }
    }

    fn voices(models: &[&str]) -> PanelConfig {
        PanelConfig {
            voices: models
                .iter()
                .map(|m| VoiceSpec {
                    name: format!("voice-{m}"),
                    model: (*m).to_string(),
                })
                .collect(),
            tier: Tier::Standard,
        }
    }

    fn pool(models: &[&str]) -> BackendPool {
        BackendPool::from_source(&StaticSource {
            backends: vec![
                PoolBackend::new("dgx", "http://dgx:11434", BackendKind::Ollama)
                    .with_models(models.iter().map(|m| m.to_string()).collect::<Vec<_>>())
                    .with_health(Health::Up),
            ],
        })
    }

    fn mock(pairs: &[(&str, &str)]) -> VoiceMock {
        VoiceMock {
            answers: pairs
                .iter()
                .map(|(m, a)| ((*m).to_string(), (*a).to_string()))
                .collect(),
        }
    }

    #[tokio::test]
    async fn accepts_passers_drops_fabricator() {
        let models = ["a", "b", "c"];
        let p = pool(&models);
        // a,b produce a GOOD answer; c fabricates (no GOOD -> verify fails).
        let d = mock(&[
            ("a", "ANS GOOD"),
            ("b", "ANS GOOD"),
            ("c", "fabricated junk"),
        ]);
        let out = run_panel(&p, &d, &SentinelVerify, &voices(&models), "task").await;
        assert_eq!(out.status, PanelStatus::Accepted);
        assert_eq!(out.accepted.as_deref(), Some("ANS GOOD"));
        assert_eq!(out.passers(), 2);
        assert!(
            !out.votes
                .iter()
                .find(|v| v.voice == "voice-c")
                .unwrap()
                .passed
        );
    }

    #[tokio::test]
    async fn agreement_breaks_ties_among_passers() {
        let models = ["a", "b", "c"];
        let p = pool(&models);
        // Two passers agree on "X GOOD"; one passer says "Y GOOD" -> the agreed one wins.
        let d = mock(&[("a", "X GOOD"), ("b", "X GOOD"), ("c", "Y GOOD")]);
        let out = run_panel(&p, &d, &SentinelVerify, &voices(&models), "task").await;
        assert_eq!(out.accepted.as_deref(), Some("X GOOD"));
        assert_eq!(out.passers(), 3);
    }

    #[tokio::test]
    async fn all_fail_is_needs_human_review() {
        let models = ["a", "b"];
        let p = pool(&models);
        let d = mock(&[("a", "no sentinel"), ("b", "also wrong")]);
        let out = run_panel(&p, &d, &SentinelVerify, &voices(&models), "task").await;
        assert_eq!(out.status, PanelStatus::NeedsHumanReview);
        assert!(out.accepted.is_none());
        assert_eq!(out.passers(), 0);
    }

    #[tokio::test]
    async fn unreachable_voice_records_no_candidate() {
        let models = ["a", "b"];
        let p = pool(&models);
        // 'b' has no canned answer -> the dispatcher errors -> run_role None.
        let d = mock(&[("a", "GOOD")]);
        let out = run_panel(&p, &d, &SentinelVerify, &voices(&models), "task").await;
        assert_eq!(out.status, PanelStatus::Accepted);
        let vb = out.votes.iter().find(|v| v.voice == "voice-b").unwrap();
        assert!(vb.candidate.is_none() && !vb.passed);
    }
}
