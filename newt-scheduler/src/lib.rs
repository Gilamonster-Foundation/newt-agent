//! newt-scheduler — the crew's `BackendPool`.
//!
//! An availability-adaptive registry of inference backends. The crew (planner /
//! navigator / triage, `docs/design/crew-loadout.md`) places each role on a
//! backend that can actually serve its model; this is the layer that answers
//! *"given a tier + an optional model pin, which LIVE backend should run it?"*
//!
//! The core is **pure** (no I/O): health is settable state, and population is
//! pluggable via [`PoolSource`] (static config today; a mesh-presence source is a
//! later, feature-gated impl — the pool must not know the source). Probing and
//! failover dispatch build on top of this core.
//!
//! Grounded in the crew-MVP empirical findings
//! (`experiments/crew-mvp/FINDINGS.md`):
//! - **Model-pin is load-bearing.** gpu-runner's 16GB GPU cannot host a 30B planner, so a
//!   pin must route to a backend that genuinely *has* the model — never "any backend".
//! - **Busy is not Down.** The DGX is intermittently busy (70b models hog VRAM); a
//!   busy backend can still take queued work, so it stays a candidate but is
//!   de-prioritised behind an idle one (place-don't-pile-on).
//! - **Count-adaptive dispatch** (`workflow-swarm-harness.md` §3.1): 0 live
//!   candidates → refuse, 1 → time-slice, N → fan out.

use newt_core::{BackendConfig, BackendKind, Tier};

mod crew;
mod dispatch;
mod panel;
mod probe;
mod roster;
mod team;
pub use crew::{run_crew, CrewConfig, CrewOutcome, CrewStatus, Edit, RoleStep, Workspace};
pub use dispatch::{BackendRuntime, ChatReply, ChatRequest, Dispatcher, LocalDispatcher};
pub use panel::{run_panel, PanelConfig, PanelOutcome, PanelStatus, Verify, VoiceSpec, Vote};
pub use probe::{Prober, TcpProber};
pub use roster::{compose_from_pool, compose_roster, ModelPrior, RosterMode, RosterSpec};
pub use team::{run_team, SubtaskResult, SubtaskStatus, TeamConfig, TeamOutcome, TeamStatus};

/// Liveness of a backend.
///
/// `Busy` is distinct from `Down`: a busy backend (e.g. the DGX loading a 70b
/// model) can still accept queued work, so it remains a candidate — just a
/// lower-priority one than an idle `Up` backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Reachable and idle — the preferred target.
    Up,
    /// Reachable but loaded; can still queue work, de-prioritised.
    Busy,
    /// Unreachable — skipped entirely.
    Down,
}

/// One backend in the pool: where it is, what wire it speaks, which tiers and
/// **models** it can serve, and its current health.
#[derive(Debug, Clone)]
pub struct PoolBackend {
    /// Stable name (matches a loadout's `provider`).
    pub name: String,
    /// Endpoint URL.
    pub endpoint: String,
    /// Maximum concurrent requests issued to this named endpoint.
    pub slots: usize,
    /// Shared connection pool and concurrency permits for this endpoint name.
    runtime: BackendRuntime,
    /// Wire protocol.
    pub kind: BackendKind,
    /// Tiers this backend serves. Empty ⇒ serves any tier.
    pub tiers: Vec<Tier>,
    /// Models resident/available here. Empty ⇒ no model-pin can be satisfied.
    pub models: Vec<String>,
    /// Current liveness.
    pub health: Health,
    /// Bearer token for a hosted/OpenAI-compatible backend (`Authorization:
    /// Bearer …`), resolved from config. `None` for keyless local endpoints.
    pub api_key: Option<String>,
    /// For `kind = "embedded"`: the local GGUF model file path (the in-process
    /// engine has no `endpoint`). `None` for HTTP backends.
    pub model_path: Option<String>,
}

impl PoolBackend {
    /// A new `Up` backend with no tier/model constraints yet (use the builders).
    pub fn new(name: impl Into<String>, endpoint: impl Into<String>, kind: BackendKind) -> Self {
        Self {
            name: name.into(),
            endpoint: endpoint.into(),
            slots: 1,
            runtime: BackendRuntime::new(1),
            kind,
            tiers: Vec::new(),
            models: Vec::new(),
            health: Health::Up,
            api_key: None,
            model_path: None,
        }
    }

    /// Builder: maximum concurrent requests issued to this named endpoint.
    #[must_use]
    pub fn with_slots(mut self, slots: usize) -> Self {
        self.slots = slots;
        self.runtime = BackendRuntime::new(slots);
        self
    }

    /// Builder: the local GGUF model file for an `embedded` backend.
    #[must_use]
    pub fn with_model_path(mut self, model_path: Option<String>) -> Self {
        self.model_path = model_path.filter(|p| !p.is_empty());
        self
    }

    /// Builder: a bearer token for a hosted/OpenAI-compatible endpoint.
    #[must_use]
    pub fn with_api_key(mut self, api_key: Option<String>) -> Self {
        self.api_key = api_key.filter(|k| !k.is_empty());
        self
    }

    /// Builder: the tiers this backend serves.
    #[must_use]
    pub fn with_tiers(mut self, tiers: Vec<Tier>) -> Self {
        self.tiers = tiers;
        self
    }

    /// Builder: the models available on this backend.
    #[must_use]
    pub fn with_models<S: Into<String>>(mut self, models: impl IntoIterator<Item = S>) -> Self {
        self.models = models.into_iter().map(Into::into).collect();
        self
    }

    /// Builder: initial health.
    #[must_use]
    pub fn with_health(mut self, health: Health) -> Self {
        self.health = health;
        self
    }

    /// Live = anything but `Down`. A `Busy` backend can still take queued work.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.health != Health::Down
    }

    /// Whether this backend can serve a `(tier, optional model pin)`: the tier is
    /// supported (or it serves any), AND — if a model is pinned — it has that model.
    #[must_use]
    pub fn serves(&self, tier: Tier, model_pin: Option<&str>) -> bool {
        let tier_ok = self.tiers.is_empty() || self.tiers.contains(&tier);
        let model_ok = model_pin.is_none_or(|m| self.models.iter().any(|x| x == m));
        tier_ok && model_ok
    }
}

impl From<&BackendConfig> for PoolBackend {
    /// A config-declared backend becomes a pool entry serving its single default
    /// model. (Richer per-backend model inventories come from a probe / mesh source.)
    fn from(c: &BackendConfig) -> Self {
        // Unset kind means probe-at-connect (session/setup). The pool needs a
        // concrete wire kind for dispatch; until a probe fills it in, treat as
        // Ollama — the historical default — so undeclared-kind backends still
        // appear in the pool rather than being dropped.
        PoolBackend::new(
            c.name.clone(),
            c.endpoint.clone(),
            c.kind.unwrap_or(BackendKind::Ollama),
        )
        .with_slots(c.slots.get())
        .with_tiers(c.tiers.clone())
        .with_models(c.effective_model().map(str::to_string))
        .with_api_key(c.resolve_api_key())
        .with_model_path(c.model_path.clone())
    }
}

/// How to dispatch, given how many LIVE backends can serve the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchStrategy {
    /// Nothing live can serve it — fail closed (do not silently pick a wrong model).
    Refuse,
    /// Exactly one — serialize / time-slice on it.
    TimeSlice,
    /// `n` live candidates — fan out across them.
    FanOut(usize),
}

/// The outcome of a successful [`BackendPool::dispatch_with_failover`]: which
/// backend served it, the attempt's result, and the names that failed first (the
/// caller marks those `Busy`/`Down`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failover<T> {
    /// The backend that succeeded.
    pub chosen: String,
    /// The successful attempt's value.
    pub result: T,
    /// Backends that failed before `chosen` succeeded, in the order tried.
    pub failed: Vec<String>,
}

/// Populates a [`BackendPool`]. The pool is source-agnostic: `StaticSource` reads
/// config today; a `MeshSource` (mDNS presence) is a later, feature-gated impl.
pub trait PoolSource {
    /// The current set of backends this source knows about.
    fn backends(&self) -> Vec<PoolBackend>;
}

/// A fixed set of backends (from `[backends]` / `[backend_pool]` config).
#[derive(Debug, Clone, Default)]
pub struct StaticSource {
    /// The configured backends.
    pub backends: Vec<PoolBackend>,
}

impl StaticSource {
    /// Build from config-declared backends.
    pub fn from_configs<'a>(configs: impl IntoIterator<Item = &'a BackendConfig>) -> Self {
        Self {
            backends: configs.into_iter().map(PoolBackend::from).collect(),
        }
    }
}

impl PoolSource for StaticSource {
    fn backends(&self) -> Vec<PoolBackend> {
        self.backends.clone()
    }
}

/// The availability-adaptive registry of inference backends.
#[derive(Debug, Default, Clone)]
pub struct BackendPool {
    backends: Vec<PoolBackend>,
}

impl BackendPool {
    /// Build a pool from a source (config now, mesh later).
    pub fn from_source(src: &dyn PoolSource) -> Self {
        Self {
            backends: src.backends(),
        }
    }

    /// All backends, regardless of health.
    #[must_use]
    pub fn backends(&self) -> &[PoolBackend] {
        &self.backends
    }

    /// The distinct models the environment **actually offers right now** — every
    /// model on a live backend, deduplicated, sorted. This is the "what it saw in
    /// its environment" survey the roster composer reads.
    #[must_use]
    pub fn live_models(&self) -> Vec<String> {
        let mut models: Vec<String> = self
            .backends
            .iter()
            .filter(|b| b.is_live())
            .flat_map(|b| b.models.iter().cloned())
            .collect();
        models.sort();
        models.dedup();
        models
    }

    /// Live backends that can serve `(tier, model_pin)`.
    #[must_use]
    pub fn candidates(&self, tier: Tier, model_pin: Option<&str>) -> Vec<&PoolBackend> {
        self.backends
            .iter()
            .filter(|b| b.is_live() && b.serves(tier, model_pin))
            .collect()
    }

    /// Live candidates for `(tier, model_pin)`, **best-first**: idle `Up` before
    /// `Busy` (place-don't-pile-on). The failover order.
    #[must_use]
    pub fn ranked_candidates(&self, tier: Tier, model_pin: Option<&str>) -> Vec<&PoolBackend> {
        let mut c = self.candidates(tier, model_pin);
        c.sort_by_key(|b| match b.health {
            Health::Up => 0u8,
            Health::Busy => 1,
            Health::Down => 2,
        });
        c
    }

    /// Pick one backend for `(tier, model_pin)` — the best-ranked candidate. `None`
    /// when nothing live can serve it (the caller fails closed rather than picking a
    /// wrong model).
    #[must_use]
    pub fn select(&self, tier: Tier, model_pin: Option<&str>) -> Option<&PoolBackend> {
        self.ranked_candidates(tier, model_pin).into_iter().next()
    }

    /// Dispatch with **failover**: try `attempt` against each candidate best-first
    /// until one succeeds. Returns the chosen backend + result and the names that
    /// failed (so the caller can [`mark`](Self::mark) them `Busy`/`Down` — done
    /// outside this borrow). `None` when no candidate succeeds (or none exist).
    ///
    /// The I/O is the injected closure, so this stays pure + testable — and it's the
    /// answer to `MeshAsker` being single-peer today: re-select on a failed peer.
    pub fn dispatch_with_failover<T, E>(
        &self,
        tier: Tier,
        model_pin: Option<&str>,
        mut attempt: impl FnMut(&PoolBackend) -> Result<T, E>,
    ) -> Option<Failover<T>> {
        let mut failed = Vec::new();
        for b in self.ranked_candidates(tier, model_pin) {
            match attempt(b) {
                Ok(result) => {
                    return Some(Failover {
                        chosen: b.name.clone(),
                        result,
                        failed,
                    })
                }
                Err(_) => failed.push(b.name.clone()),
            }
        }
        None
    }

    /// The count-adaptive dispatch strategy for `(tier, model_pin)`.
    #[must_use]
    pub fn strategy(&self, tier: Tier, model_pin: Option<&str>) -> DispatchStrategy {
        match self.candidates(tier, model_pin).len() {
            0 => DispatchStrategy::Refuse,
            1 => DispatchStrategy::TimeSlice,
            n => DispatchStrategy::FanOut(n),
        }
    }

    /// Update a backend's health (after a probe, a failover timeout, or a mesh
    /// presence change). Returns false if no backend by that name exists.
    pub fn mark(&mut self, name: &str, health: Health) -> bool {
        match self.backends.iter_mut().find(|b| b.name == name) {
            Some(b) => {
                b.health = health;
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dgx() -> PoolBackend {
        PoolBackend::new("dgx", "https://REDACTED-HOST", BackendKind::Ollama)
            .with_tiers(vec![
                Tier::Fast,
                Tier::Standard,
                Tier::Complex,
                Tier::Review,
            ])
            .with_models([
                "devstral-small-2:24b",
                "qwen3-coder:30b",
                "qwen2.5-coder:3b",
            ])
    }
    fn gpu_runner() -> PoolBackend {
        // gpu-runner's 16GB cannot host a 30B — it serves the small models only.
        PoolBackend::new("gpu-runner", "http://localhost:11434", BackendKind::Ollama)
            .with_models(["qwen2.5-coder:3b", "qwen2.5-coder:7b"])
    }

    #[test]
    fn serves_respects_tier_and_model_pin() {
        let b = dgx();
        assert!(b.serves(Tier::Complex, None));
        assert!(b.serves(Tier::Complex, Some("qwen3-coder:30b")));
        assert!(!b.serves(Tier::Complex, Some("not-here:1b")));
        // empty tiers ⇒ any tier; pin still enforced
        let g = gpu_runner();
        assert!(g.serves(Tier::Fast, Some("qwen2.5-coder:3b")));
        assert!(
            !g.serves(Tier::Fast, Some("qwen3-coder:30b")),
            "gpu-runner cannot host the 30B"
        );
    }

    #[test]
    fn model_pin_routes_around_a_backend_that_lacks_the_model() {
        // The crew reality: planner pins qwen3-coder:30b → must land on the DGX,
        // never gpu-runner (which doesn't have it).
        let pool = BackendPool::from_source(&StaticSource {
            backends: vec![dgx(), gpu_runner()],
        });
        let pick = pool.select(Tier::Complex, Some("qwen3-coder:30b")).unwrap();
        assert_eq!(pick.name, "dgx");
        // triage pins the small model → gpu-runner is a candidate (and preferred Up).
        assert_eq!(
            pool.candidates(Tier::Fast, Some("qwen2.5-coder:3b")).len(),
            2
        );
    }

    #[test]
    fn select_prefers_up_over_busy_and_skips_down() {
        let mut pool = BackendPool::from_source(&StaticSource {
            backends: vec![
                dgx().with_health(Health::Busy),
                gpu_runner().with_health(Health::Up),
            ],
        });
        // both serve the small model; the idle gpu-runner wins over the busy dgx.
        assert_eq!(
            pool.select(Tier::Fast, Some("qwen2.5-coder:3b"))
                .unwrap()
                .name,
            "gpu-runner"
        );
        // mark gpu-runner down → falls back to the busy dgx (still live).
        assert!(pool.mark("gpu-runner", Health::Down));
        assert_eq!(
            pool.select(Tier::Fast, Some("qwen2.5-coder:3b"))
                .unwrap()
                .name,
            "dgx"
        );
        // a down backend is not a candidate.
        assert!(pool.mark("dgx", Health::Down));
        assert!(pool.select(Tier::Fast, Some("qwen2.5-coder:3b")).is_none());
    }

    #[test]
    fn strategy_is_count_adaptive() {
        let pool = BackendPool::from_source(&StaticSource {
            backends: vec![dgx(), gpu_runner()],
        });
        // 0 live candidates for an unknown model → refuse.
        assert_eq!(
            pool.strategy(Tier::Complex, Some("ghost:1b")),
            DispatchStrategy::Refuse
        );
        // exactly one has the 30B → time-slice.
        assert_eq!(
            pool.strategy(Tier::Complex, Some("qwen3-coder:30b")),
            DispatchStrategy::TimeSlice
        );
        // both serve the small model → fan out across 2.
        assert_eq!(
            pool.strategy(Tier::Fast, Some("qwen2.5-coder:3b")),
            DispatchStrategy::FanOut(2)
        );
    }

    #[test]
    fn mark_unknown_backend_is_false() {
        let mut pool = BackendPool::from_source(&StaticSource {
            backends: vec![gpu_runner()],
        });
        assert!(!pool.mark("nope", Health::Down));
        assert!(pool.mark("gpu-runner", Health::Busy));
    }

    #[test]
    fn from_backend_config_maps_fields() {
        let cfg = BackendConfig {
            name: "remote".into(),
            endpoint: "http://remote:8000".into(),
            model: Some("qwen3:32b".into()),
            model_path: None,
            tiers: vec![Tier::Standard],
            kind: Some(BackendKind::Openai),
            api: Default::default(),
            api_key_file: None,
            api_key_env: None,
            ..Default::default()
        };
        let pb = PoolBackend::from(&cfg);
        assert_eq!(pb.name, "remote");
        assert_eq!(pb.kind, BackendKind::Openai);
        assert!(pb.serves(Tier::Standard, Some("qwen3:32b")));
        assert!(
            !pb.serves(Tier::Complex, None),
            "only the Standard tier was declared"
        );
        assert_eq!(pb.model_path, None, "HTTP backend has no model_path");
        // An embedded backend carries its GGUF path through to the pool entry.
        let emb = BackendConfig {
            kind: Some(BackendKind::Embedded),
            endpoint: String::new(),
            model: Some("qwen2.5-1.5b".into()),
            model_path: Some("/models/qwen.gguf".into()),
            ..cfg.clone()
        };
        assert_eq!(
            PoolBackend::from(&emb).model_path.as_deref(),
            Some("/models/qwen.gguf")
        );
        // StaticSource::from_configs round-trips a config slice.
        let src = StaticSource::from_configs([&cfg]);
        assert_eq!(BackendPool::from_source(&src).backends().len(), 1);
    }

    #[test]
    fn dispatch_with_failover_skips_failed_then_succeeds() {
        // dgx (Up) is tried first, fails; gpu-runner (Up) succeeds — both serve the small model.
        let pool = BackendPool::from_source(&StaticSource {
            backends: vec![dgx(), gpu_runner()],
        });
        let out = pool
            .dispatch_with_failover(Tier::Fast, Some("qwen2.5-coder:3b"), |b| {
                if b.name == "dgx" {
                    Err("timeout")
                } else {
                    Ok(format!("served by {}", b.name))
                }
            })
            .unwrap();
        assert_eq!(out.chosen, "gpu-runner");
        assert_eq!(out.result, "served by gpu-runner");
        assert_eq!(
            out.failed,
            vec!["dgx".to_string()],
            "dgx failed first, caller marks it"
        );
    }

    #[test]
    fn dispatch_with_failover_none_when_all_fail_or_none_serve() {
        let pool = BackendPool::from_source(&StaticSource {
            backends: vec![dgx(), gpu_runner()],
        });
        // every attempt errors → None, and the caller could mark all tried.
        let all_fail: Option<Failover<()>> =
            pool.dispatch_with_failover(Tier::Fast, Some("qwen2.5-coder:3b"), |_| Err(()));
        assert!(all_fail.is_none());
        // no candidate serves the model → None without any attempt.
        let mut attempts = 0;
        let none: Option<Failover<()>> =
            pool.dispatch_with_failover(Tier::Fast, Some("ghost:1b"), |_| -> Result<(), ()> {
                attempts += 1;
                Ok(())
            });
        assert!(none.is_none());
        assert_eq!(
            attempts, 0,
            "no candidates ⇒ the attempt closure never runs"
        );
    }

    #[test]
    fn ranked_candidates_orders_up_before_busy() {
        let pool = BackendPool::from_source(&StaticSource {
            backends: vec![
                dgx().with_health(Health::Busy),
                gpu_runner().with_health(Health::Up),
            ],
        });
        let ranked = pool.ranked_candidates(Tier::Fast, Some("qwen2.5-coder:3b"));
        assert_eq!(
            ranked.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["gpu-runner", "dgx"]
        );
    }

    #[test]
    fn empty_pool_refuses() {
        let pool = BackendPool::default();
        assert_eq!(pool.strategy(Tier::Fast, None), DispatchStrategy::Refuse);
        assert!(pool.select(Tier::Fast, None).is_none());
        assert!(pool.backends().is_empty());
    }
}
